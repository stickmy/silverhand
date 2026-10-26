//! The human credentialed-session HTTP surface — the #2192 "closed set" of five
//! routes:
//!
//! * `POST /auth/signup`  — public creator of an email+password identity.
//! * `POST /auth/login`   — public credential verification.
//! * `POST /auth/logout`  — public bearer revocation.
//! * `GET  /api/me`       — the caller's own profile (behind the session perimeter).
//! * `PATCH /api/me`      — the caller's own `display_name` (behind the perimeter).
//!
//! These handlers are thin transports over the application flows in
//! [`silverhand_backend_core::identity_access::human_auth_flows`] and
//! [`silverhand_backend_core::identity_access::user_identity`]: the domain owns the security
//! ordering (hash → handle → email), the uniform credential answer, the
//! committed-fact seal, the change gate, and the `user_changed` announce fate.
//! This module's only jobs are request-boundary admission — the frozen
//! [`ValidatedJson<T>`](crate::foundation::wire_admission::ValidatedJson) extractor decodes
//! the body into a typed DTO (native `Json` owns JSON syntax / wrong-type /
//! content-type rejection), garde enforces the transport password bounds, and a
//! `TryFrom`/`into_command` normalizes the identity value objects — and the wire
//! projection.
//!
//! ## Token delivery is BEARER, never a cookie
//!
//! The TS contract returns the freshly minted session token in the JSON body as
//! `user_token` on signup/login, and reads `Authorization: Bearer <token>` on the
//! authenticated routes. There is no `Set-Cookie` anywhere in this surface, so the
//! port replicates exactly that: `user_token` in the body, and the shared
//! [`crate::identity_access::auth::require_human_session`] perimeter (which reads the same bearer
//! grammar as logout) guards `GET`/`PATCH /api/me`. Logout is NOT behind the
//! perimeter — it must read the raw bearer itself to revoke unknown/expired
//! bearers with the same `401` a missing bearer gets — so it derives the bearer
//! through the one canonical [`crate::foundation::bearer::strict_bearer_credential`] the
//! perimeter uses, never a second grammar.
//!
//! ## Two deliberate, reachability-priced wire divergences (flagged for review)
//!
//! 1. A semantic-admission failure is terminally `422 { "error": "invalid input" }`,
//!    WITHOUT the Zod `issues` array the TS route also emits, AND the empty
//!    `PATCH /api/me` object (the Zod `.refine` "no fields to update") collapses to
//!    the same body rather than TS's distinct `{ "error": "no fields to update" }`.
//!    No shared response schema pins `issues`, and no consumer branches on it or on
//!    the refine string. This surface takes the frozen admission model's minimal
//!    semantic refusal; native JSON structural faults (malformed / wrong-type /
//!    missing content-type) keep their native `400`/`422`/`415`, and the old
//!    hand-rolled `400 invalid input` for those structural cases is gone.
//! 2. Under the bearer perimeter, an authenticated session that resolves to no
//!    `users` row is a dangling pointer — persistence corruption — so `GET`/`PATCH
//!    /api/me` take the lane's safe-500 (bare `text/plain`), NOT the TS `404
//!    user_not_found`. The core deliberately models this as the corruption seam
//!    (`read_self_profile`/`UserProfileUpdate::NoSuchUser` doc comments): a 404
//!    would dress corruption as a product answer a client could probe.
//!
//! ## Faults take the lane's safe `500`, never a JSON envelope
//!
//! Every [`HumanAuthError`](silverhand_backend_core::identity_access::human_auth_flows::HumanAuthError)
//! arm, a revoke persistence fault, and an untrustworthy committed row take the
//! crate-wide bare `text/plain` `Internal Server Error`, so no adapter detail can
//! leak and a misconfiguration is never dressed as a product answer.

use axum::{
    Json, Router,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    conversation_messaging::messaging::PrincipalKind,
    foundation::identity::UserId,
    identity_access::auth::{
        AuthenticateHumanSession, IssuedSessionSecret, SessionAudit, SessionBearer, SessionClock,
        SessionIssue, SessionRevoke, SessionTokenMint,
    },
    identity_access::credential::{LoginCredentialLookup, PasswordHasher},
    identity_access::human_auth_flows::{
        HandleNamespaceRead, HumanRegistrationUnitOfWork, Login, LoginCommand, LoginPorts, Signup,
        SignupCommand, SignupPorts, log_in, log_out, sign_up,
    },
    identity_access::user_identity::{
        DisplayName, EmailAddress, ParticipantHandle, UserChangedAnnounce, UserChangedSink,
        UserEmailLookup, UserProfileRead, UserProfileUpdate, UserProfileWriteUnitOfWork,
        UserRecord, read_self_profile, update_self_display_name,
    },
};

use crate::conversation_messaging::messaging::author_wire::principal_avatar_url;
use crate::foundation::bearer::strict_bearer_credential;
use crate::foundation::error;
use crate::foundation::wire_admission::{self, ValidatedJson, ValidatedJsonRejection};
use crate::foundation::wire_patch::RequiredPatchField;
use crate::identity_access::auth::{PublicJsonUnauthorized, require_human_session};

/// `authPaths.signup`.
pub const SIGNUP_PATH: &str = "/auth/signup";
/// `authPaths.login`.
pub const LOGIN_PATH: &str = "/auth/login";
/// `authPaths.logout`.
pub const LOGOUT_PATH: &str = "/auth/logout";
/// `apiPaths.me` — one path, `GET` and `PATCH`.
pub const ME_PATH: &str = "/api/me";

/// The transport length bounds Zod imposes at the route, in UTF-16 code units
/// (JS `.length`). The value objects enforce the identity rules (email grammar,
/// handle regex, display-name charset); these two are the raw-password bounds Zod
/// owns and the domain deliberately does not (`sign_up`/`log_in` take an
/// unvalidated password string).
pub(crate) const SIGNUP_PASSWORD_MIN_UTF16: usize = 8;
pub(crate) const LOGIN_PASSWORD_MIN_UTF16: usize = 1;
const PASSWORD_MAX_UTF16: usize = 1024;

/// The safe-500 body, a bare `text/plain`, matching the crate's other routes.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

// ===========================================================================
// Composition: the ports the five handlers need, generic per capability.
// ===========================================================================

/// Everything the human-auth routes need, assembled by the composition root. It
/// is generic over each collaborator INDEPENDENTLY — one type per bounded-context
/// capability — so a test drives driver-free fakes and the composition root wires
/// the SQLite adapters (in production several of these resolve to one store, but
/// the router depends only on the capability, never the concrete). Used directly
/// as the axum state; it is `Clone` because every port is.
///
/// The session-read authority for the `/api/me` perimeter is passed separately to
/// [`router`] (it is applied as a middleware layer, not read by a handler).
#[derive(Clone)]
pub struct HumanAuthDeps<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk> {
    /// The @-handle existence read (both halves of the mention namespace).
    pub handles: Ha,
    /// The `users.getByEmail` existence read signup consults.
    pub emails: El,
    /// The Argon2id hasher (verify + dummy-verify + hash).
    pub hasher: Hr,
    /// The registration unit of work (atomic `users` + credential insert).
    pub registration: Reg,
    /// The session token mint (CSPRNG bearer + its lookup coordinate).
    pub mint: Mint,
    /// The session issue write (persist a fresh coordinate).
    pub sessions: Iss,
    /// The session revoke write (logout).
    pub revoke: Rev,
    /// The login credential lookup (email → user + PHC).
    pub credentials: Cred,
    /// The profile read (`GET /api/me`, and login's post-verify user fetch).
    pub reads: Rd,
    /// The `display_name` write unit of work (`PATCH /api/me`).
    pub writes: Wr,
    /// The `user_changed` announce sink (signup + profile edit). No production
    /// adapter ships in this vertical — the #2215 integration residual wires it.
    pub user_changed: Sink,
    /// The request-instant clock. Reuses the auth context's [`SessionClock`]
    /// rather than minting a second time authority.
    pub clock: Clk,
    /// Whether `POST /auth/signup` is enabled. `false` answers the typed `403`
    /// before the body is read, so Desktop/ops can tell disabled signup from a
    /// missing route.
    pub signup_enabled: bool,
}

/// Mounts all five routes generic over their ports. The public trio
/// (signup/login/logout) is merged OUTSIDE the session perimeter; the `me` pair is
/// wrapped by [`require_human_session`] so its handlers receive the sole core-wide
/// [`UserId`] in their extensions. Route registration into the served application
/// is a separate integration seam (this crate never touches the composition root).
#[allow(clippy::too_many_arguments)]
pub fn router<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk, A>(
    deps: HumanAuthDeps<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>,
    session_authority: A,
) -> Router
where
    Ha: HandleNamespaceRead,
    El: UserEmailLookup,
    Hr: PasswordHasher,
    Reg: HumanRegistrationUnitOfWork,
    Mint: SessionTokenMint,
    Iss: SessionIssue,
    Rev: SessionRevoke,
    Cred: LoginCredentialLookup,
    Rd: UserProfileRead,
    Wr: UserProfileWriteUnitOfWork,
    Sink: UserChangedSink,
    Clk: SessionClock,
    A: AuthenticateHumanSession,
{
    let public = Router::new()
        .route(
            SIGNUP_PATH,
            post(signup::<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>),
        )
        .route(
            LOGIN_PATH,
            post(login::<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>),
        )
        .route(
            LOGOUT_PATH,
            post(logout::<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>),
        )
        .with_state(deps.clone());

    let protected = Router::new()
        .route(
            ME_PATH,
            get(me_get::<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>)
                .patch(me_patch::<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>),
        )
        .with_state(deps);
    let protected = require_human_session(protected, session_authority, PublicJsonUnauthorized);

    public.merge(protected)
}

// ===========================================================================
// POST /auth/signup
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn signup<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>(
    State(deps): State<HumanAuthDeps<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>>,
    headers: HeaderMap,
    body: Result<ValidatedJson<SignupRaw>, ValidatedJsonRejection>,
) -> Response
where
    Ha: HandleNamespaceRead,
    El: UserEmailLookup,
    Hr: PasswordHasher,
    Reg: HumanRegistrationUnitOfWork,
    Mint: SessionTokenMint,
    Iss: SessionIssue,
    Rev: SessionRevoke,
    Cred: LoginCredentialLookup,
    Rd: UserProfileRead,
    Wr: UserProfileWriteUnitOfWork,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    // The typed 403 is decided BEFORE the body verdict is consulted: disabled
    // signup must not let a caller probe body shape. Wrapping the extractor in a
    // `Result` lets the body decode (native structural / garde semantic) resolve
    // during extraction WITHOUT failing the request, so this gate still answers
    // first for any body.
    if !deps.signup_enabled {
        return signup_disabled();
    }

    // Audit breadcrumbs come from the headers, extracted before the body.
    let audit = capture_audit(&headers);
    let raw = match body {
        Ok(ValidatedJson(raw)) => raw,
        // A structural fault keeps its native rejection; a garde-rule fault is the
        // minimal semantic 422 — both handled by the shared rejection renderer.
        Err(rejection) => return rejection.into_response(),
    };
    let Ok(command) = raw.into_command(audit) else {
        return wire_admission::invalid_input();
    };

    let now_ms = deps.clock.now_unix_millis();
    let ports = SignupPorts {
        handles: &deps.handles,
        emails: &deps.emails,
        hasher: &deps.hasher,
        registration: &deps.registration,
        mint: &deps.mint,
        sessions: &deps.sessions,
        user_changed: &deps.user_changed,
    };
    match sign_up(ports, command, now_ms).await {
        Ok(Signup::Registered {
            user,
            secret,
            announce,
        }) => {
            log_lost_announce(&announce, "signup");
            session_response(secret, &user)
        }
        Ok(Signup::HandleTaken) => handle_taken(),
        Ok(Signup::AuthFailed) => auth_failed(),
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "signup flow failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// POST /auth/login
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn login<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>(
    State(deps): State<HumanAuthDeps<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>>,
    headers: HeaderMap,
    ValidatedJson(raw): ValidatedJson<LoginRaw>,
) -> Response
where
    Ha: HandleNamespaceRead,
    El: UserEmailLookup,
    Hr: PasswordHasher,
    Reg: HumanRegistrationUnitOfWork,
    Mint: SessionTokenMint,
    Iss: SessionIssue,
    Rev: SessionRevoke,
    Cred: LoginCredentialLookup,
    Rd: UserProfileRead,
    Wr: UserProfileWriteUnitOfWork,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    let audit = capture_audit(&headers);
    let Ok(command) = raw.into_command(audit) else {
        return wire_admission::invalid_input();
    };

    let now_ms = deps.clock.now_unix_millis();
    let ports = LoginPorts {
        credentials: &deps.credentials,
        users: &deps.reads,
        hasher: &deps.hasher,
        mint: &deps.mint,
        sessions: &deps.sessions,
    };
    match log_in(ports, command, now_ms).await {
        Ok(Login::Authenticated { user, secret }) => session_response(secret, &user),
        Ok(Login::AuthFailed) => auth_failed(),
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "login flow failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// POST /auth/logout
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn logout<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>(
    State(deps): State<HumanAuthDeps<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>>,
    headers: HeaderMap,
) -> Response
where
    Ha: HandleNamespaceRead,
    El: UserEmailLookup,
    Hr: PasswordHasher,
    Reg: HumanRegistrationUnitOfWork,
    Mint: SessionTokenMint,
    Iss: SessionIssue,
    Rev: SessionRevoke,
    Cred: LoginCredentialLookup,
    Rd: UserProfileRead,
    Wr: UserProfileWriteUnitOfWork,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    // Logout reads the raw bearer itself (it lives outside the perimeter so it can
    // revoke unknown/expired bearers), through the ONE canonical grammar the
    // perimeter uses. A missing bearer and a non-revocable one both answer 401.
    let Some(bearer) =
        strict_bearer_credential(&headers).and_then(|token| SessionBearer::parse(token.to_owned()))
    else {
        return unauthorized();
    };

    let now_ms = deps.clock.now_unix_millis();
    match log_out(&deps.revoke, &bearer, now_ms).await {
        Ok(true) => logout_ok(),
        Ok(false) => unauthorized(),
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "session revoke failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// GET /api/me
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn me_get<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>(
    viewer: Option<Extension<UserId>>,
    State(deps): State<HumanAuthDeps<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>>,
) -> Response
where
    Ha: HandleNamespaceRead,
    El: UserEmailLookup,
    Hr: PasswordHasher,
    Reg: HumanRegistrationUnitOfWork,
    Mint: SessionTokenMint,
    Iss: SessionIssue,
    Rev: SessionRevoke,
    Cred: LoginCredentialLookup,
    Rd: UserProfileRead,
    Wr: UserProfileWriteUnitOfWork,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    let Some(Extension(user_id)) = viewer else {
        tracing::error!("GET /api/me reached without its human principal fact");
        return internal_error();
    };
    match read_self_profile(&deps.reads, &user_id).await {
        Ok(Some(user)) => profile_ok(&user),
        // A validated session resolving to no user row is a dangling pointer:
        // corruption, the safe-500 seam, never a product 404.
        Ok(None) => {
            tracing::error!(
                user_id = crate::foundation::safe_log_field::safe_display(&(user_id.as_str())),
                "an authenticated session resolves to no user row"
            );
            internal_error()
        }
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "read self profile failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// PATCH /api/me
// ===========================================================================

#[allow(clippy::type_complexity)]
async fn me_patch<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>(
    viewer: Option<Extension<UserId>>,
    State(deps): State<HumanAuthDeps<Ha, El, Hr, Reg, Mint, Iss, Rev, Cred, Rd, Wr, Sink, Clk>>,
    ValidatedJson(raw): ValidatedJson<MePatchRaw>,
) -> Response
where
    Ha: HandleNamespaceRead,
    El: UserEmailLookup,
    Hr: PasswordHasher,
    Reg: HumanRegistrationUnitOfWork,
    Mint: SessionTokenMint,
    Iss: SessionIssue,
    Rev: SessionRevoke,
    Cred: LoginCredentialLookup,
    Rd: UserProfileRead,
    Wr: UserProfileWriteUnitOfWork,
    Sink: UserChangedSink,
    Clk: SessionClock,
{
    let Some(Extension(user_id)) = viewer else {
        tracing::error!("PATCH /api/me reached without its human principal fact");
        return internal_error();
    };

    // An invalid `display_name` value AND the empty/no-fields object both collapse
    // to the one terminal semantic 422 (ratified divergence #1). A malformed or
    // wrong-typed body was already refused by the native `Json` decode.
    let Ok(display_name) = DisplayName::try_from(raw) else {
        return wire_admission::invalid_input();
    };

    let now_ms = deps.clock.now_unix_millis();
    match update_self_display_name(
        &deps.writes,
        &deps.user_changed,
        &user_id,
        display_name,
        now_ms,
    )
    .await
    {
        Ok(UserProfileUpdate::Committed(updated)) => {
            let (user, announce) = updated.into_parts();
            log_lost_announce(&announce, "profile edit");
            profile_ok(&user)
        }
        // As with GET, a dangling session pointer is corruption → safe-500.
        Ok(UserProfileUpdate::NoSuchUser) => {
            tracing::error!(
                user_id = crate::foundation::safe_log_field::safe_display(&(user_id.as_str())),
                "an authenticated session resolves to no user row on profile write"
            );
            internal_error()
        }
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "self profile write failed"
            );
            internal_error()
        }
    }
}

// ===========================================================================
// Request-boundary admission
// ===========================================================================

/// The `POST /auth/signup` body, decoded by the native `Json` extractor and then
/// bounded by garde. `email`/`password`/`name` are required strings — an absent
/// or wrong-typed one is a native decode fault. `display_name` is
/// `.optional()`: absent is fine, but a present `null` or non-string is a native
/// decode fault, exactly as Zod's `DisplayNameSchema.optional()` refuses a
/// present non-string. The password bounds are the only rule garde enforces
/// here; the identity fields normalize in [`SignupRaw::into_command`].
#[derive(Debug, Deserialize, Validate)]
struct SignupRaw {
    #[garde(skip)]
    email: String,
    #[garde(custom(password_within(SIGNUP_PASSWORD_MIN_UTF16)))]
    password: String,
    #[garde(skip)]
    name: String,
    #[serde(default)]
    #[garde(skip)]
    display_name: RequiredPatchField<String>,
}

impl SignupRaw {
    /// Normalize the admitted body into the domain command, injecting the audit
    /// captured from the headers. `Err(())` is a request-local normalization
    /// failure (bad email grammar, handle regex, or display-name charset) — the
    /// caller renders it as the minimal semantic 422.
    fn into_command(self, audit: SessionAudit) -> Result<SignupCommand, ()> {
        let email = EmailAddress::parse(&self.email).map_err(|_| ())?;
        let handle = ParticipantHandle::parse(&self.name).map_err(|_| ())?;
        let display_name = match self.display_name {
            // `.optional()` admits an absent key as "no display name".
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(raw) => Some(DisplayName::parse(&raw).map_err(|_| ())?),
        };
        Ok(SignupCommand {
            email,
            handle,
            password: self.password,
            display_name,
            audit,
        })
    }
}

/// The `POST /auth/login` body: `email` and `password`, both required strings.
/// The password floor is `min(1)` here (not signup's `8`); the max is shared.
#[derive(Debug, Deserialize, Validate)]
struct LoginRaw {
    #[garde(skip)]
    email: String,
    #[garde(custom(password_within(LOGIN_PASSWORD_MIN_UTF16)))]
    password: String,
}

impl LoginRaw {
    /// Normalize the admitted body into the login command, injecting the audit.
    fn into_command(self, audit: SessionAudit) -> Result<LoginCommand, ()> {
        let email = EmailAddress::parse(&self.email).map_err(|_| ())?;
        Ok(LoginCommand {
            email,
            password: self.password,
            audit,
        })
    }
}

/// The `PATCH /api/me` body: `{ display_name?: DisplayNameSchema }` with a
/// `.refine(keys > 0)`. `display_name` is a [`RequiredPatchField`] so an absent
/// key is the refine's "no fields" (a semantic 422 in [`TryFrom`]) while a
/// present `null` or non-string is a native decode fault.
#[derive(Debug, Deserialize, Validate)]
struct MePatchRaw {
    #[serde(default)]
    #[garde(skip)]
    display_name: RequiredPatchField<String>,
}

impl TryFrom<MePatchRaw> for DisplayName {
    type Error = ();

    fn try_from(raw: MePatchRaw) -> Result<Self, ()> {
        match raw.display_name {
            // The refine's "no fields to update" collapses to the terminal 422.
            RequiredPatchField::Missing => Err(()),
            RequiredPatchField::Value(value) => DisplayName::parse(&value).map_err(|_| ()),
        }
    }
}

/// A garde field rule bounding a raw password to `[min, PASSWORD_MAX_UTF16]`
/// UTF-16 code units, delegating the count to the route-local [`within_utf16`]
/// authority (JS `.length`). The domain deliberately takes an UNvalidated
/// password (`sign_up`/`log_in`), so these transport bounds live at the wire,
/// not in a value object.
pub(crate) fn password_within(min: usize) -> impl Fn(&String, &()) -> garde::Result {
    move |value, _| {
        if within_utf16(value, min, PASSWORD_MAX_UTF16) {
            Ok(())
        } else {
            Err(garde::Error::new("password length is out of bounds"))
        }
    }
}

/// Capture the forensic breadcrumbs a session records, mirroring the TS
/// `authSessionMetadata`: the `user-agent` header, and the first `x-forwarded-for`
/// hop else `x-real-ip`. Normalization (trim/drop-blank/512-unit cut) is the value
/// object's job.
fn capture_audit(headers: &HeaderMap) -> SessionAudit {
    let user_agent = header_value(headers, "user-agent");
    let ip_first_seen = first_forwarded_for(headers).or_else(|| header_value(headers, "x-real-ip"));
    SessionAudit::capture(user_agent.as_deref(), ip_first_seen.as_deref())
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// `value?.split(',')[0]?.trim()`, empty → `None`.
fn first_forwarded_for(headers: &HeaderMap) -> Option<String> {
    let raw = header_value(headers, "x-forwarded-for")?;
    let first = raw.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

/// `z.string().min(min).max(max)` on the UTF-16 code-unit length (JS `.length`).
fn within_utf16(value: &str, min: usize, max: usize) -> bool {
    let units = value.encode_utf16().count();
    (min..=max).contains(&units)
}

// ===========================================================================
// Wire projection
// ===========================================================================

/// The `UserDto` both `me` routes and the signup/login `user` field project.
/// snake_case on the wire; `email` is never part of it (only the self profile adds it). `avatar_url` is the
/// cache-busted `/api/users/:id/avatar?v=:updated_at` projected from the user's
/// stored avatar revision (the shared [`principal_avatar_url`] human arm), or
/// `null` when no avatar is stored — a freshly signed-up user has none.
#[derive(Serialize)]
pub(crate) struct UserDto {
    id: String,
    name: String,
    display_name: String,
    avatar_url: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl UserDto {
    pub(crate) fn from_record(user: &UserRecord) -> Self {
        Self {
            id: user.id.as_str().to_owned(),
            name: user.handle.clone(),
            display_name: user.display_name.clone(),
            avatar_url: principal_avatar_url(PrincipalKind::Human, user.id.as_str(), user.avatar),
            created_at: user.created_at,
            updated_at: user.updated_at,
        }
    }

    /// The cache-busted avatar URL this DTO projects, cloned for the avatar-upload
    /// response's top-level `avatar_url` mirror (which repeats `user.avatar_url`).
    pub(crate) fn avatar_url(&self) -> Option<String> {
        self.avatar_url.clone()
    }
}

/// `AuthSessionResponseSchema`: `{ user_token, user }`.
#[derive(Serialize)]
struct AuthSessionResponse {
    user_token: String,
    user: UserDto,
}

/// `AuthLogoutResponseSchema`: `{ ok: true }`.
#[derive(Serialize)]
struct LogoutResponse {
    ok: bool,
}

/// The flat `{ error }` refusal body.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// The `{ error_code }` refusal body (the uniform credential answer + taken handle).
#[derive(Serialize)]
struct ErrorCodeBody {
    error_code: &'static str,
}

/// The typed disabled-signup body, `{ error, code }`.
#[derive(Serialize)]
struct SignupDisabledBody {
    error: &'static str,
    code: &'static str,
}

/// `c.json({ user_token, user })` at `200`, consuming the one-time secret.
fn session_response(secret: IssuedSessionSecret, user: &UserRecord) -> Response {
    Json(AuthSessionResponse {
        user_token: secret.into_wire(),
        user: UserDto::from_record(user),
    })
    .into_response()
}

/// The viewer's own profile: the public `UserDto` plus the caller's own email,
/// which no other projection carries.
#[derive(Serialize)]
struct SelfProfileDto<'a> {
    #[serde(flatten)]
    user: UserDto,
    email: Option<&'a str>,
}

fn profile_ok(user: &UserRecord) -> Response {
    Json(SelfProfileDto {
        user: UserDto::from_record(user),
        email: user.email.as_deref(),
    })
    .into_response()
}

fn logout_ok() -> Response {
    Json(LogoutResponse { ok: true }).into_response()
}

fn handle_taken() -> Response {
    (
        StatusCode::CONFLICT,
        Json(ErrorCodeBody {
            error_code: "handle_taken",
        }),
    )
        .into_response()
}

fn auth_failed() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorCodeBody {
            error_code: "auth_failed",
        }),
    )
        .into_response()
}

fn signup_disabled() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(SignupDisabledBody {
            error: "signup is disabled on this backend",
            code: "signup_disabled",
        }),
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody {
            error: "unauthorized",
        }),
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

/// A committed identity change owes a best-effort `user_changed` hint; a lost hint
/// never downgrades the committed `200`, so it is only recorded.
pub(crate) fn log_lost_announce(announce: &UserChangedAnnounce, origin: &str) {
    if let UserChangedAnnounce::Failed(failure) = announce {
        tracing::error!(
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(failure))),
            origin,
            "user_changed announce could not be delivered; \
             affected readers stay stale until they refetch"
        );
    }
}
