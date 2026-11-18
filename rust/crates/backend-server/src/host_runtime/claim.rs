//! Authenticated `POST /api/hosts/claim-or-create`: the desktop main process names
//! the installation it runs on and asks for the computer that installation owns, plus
//! the daemon token it needs to attach.
//!
//! Two authorities, in order:
//!
//!  1. VALIDATE — the request body is admitted through the frozen [`ValidatedJson`]
//!     extractor against [`RawClaim`]: `installation_id` a required string bounded by the
//!     route-local `is_uuid` predicate, `label` and `current_host_id` optional but
//!     NON-nullable strings; unknown keys are ignored. A malformed
//!     body, a wrong-typed or `null` field, a missing `installation_id`, and
//!     a non-object shape all keep axum's native `Json` classification (`400`/`422`/`415`)
//!     — this route publishes no validation-issue schema. `TryFrom<RawClaim>` then
//!     normalizes the two optional strings through their sole value authorities
//!     (`ComputerDisplayLabel::parse`, `HostId::parse`) and a bad uuid / empty / over-long
//!     value collapses to the minimal semantic `422 {"error":"invalid input"}`. Every
//!     admission fault stops before the claim authority is reached.
//!  2. CLAIM — [`ClaimLocalComputer`] owns the three-way reuse/adopt/register branch and
//!     the one credential mint; this factory only carries its refusals to the wire. A
//!     credential refused because the computer was revoked under the claim is the one
//!     typed race with a `409` of its own; every other refusal is a composition fault
//!     that takes a fail-closed `500`.
//!
//! The human principal is proof the authenticating perimeter ran; its ABSENCE is a
//! composition fault, not a product request, so it takes a fail-closed `500` and reaches
//! neither the claim authority nor the credential mint. Mounting the real bearer
//! perimeter is the composition root's job (#task:2197), not this factory's.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::{HostId, UserId},
    host_runtime::{
        claim::{
            ClaimLocalComputer, ClaimLocalComputerError, ClaimLocalComputerUnitOfWork,
            LocalClaimRequest,
        },
        computer::ComputerDisplayLabel,
        custody::{CredentialIssueRefusal, CredentialIssueUnitOfWork, HostCredentialInventory},
        environment::{Clock, EntropySource},
    },
};

use crate::foundation::error;
use crate::foundation::wire_admission::{ValidatedJson, invalid_input};
use crate::foundation::wire_patch::RequiredPatchField;

use super::{
    ErrorBody,
    host_wire::{HostDto, host_wire},
    internal_error,
};

pub const HOST_CLAIM_OR_CREATE_PATH: &str = "/api/hosts/claim-or-create";

/// The credential-revoked race, pinned to the TypeScript handler's defensive branch
/// (`{ error: 'host has been revoked', error_code: 'host_revoked' }`).
const HOST_REVOKED: &str = "host has been revoked";
const HOST_REVOKED_CODE: &str = "host_revoked";

struct ClaimState<U, I, W, R, C>(Arc<ClaimLocalComputer<U, I, W, R, C>>);

impl<U, I, W, R, C> Clone for ClaimState<U, I, W, R, C> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub fn router<U, I, W, R, C>(claim: ClaimLocalComputer<U, I, W, R, C>) -> Router
where
    U: ClaimLocalComputerUnitOfWork,
    I: HostCredentialInventory,
    W: CredentialIssueUnitOfWork,
    R: EntropySource + 'static,
    C: Clock + 'static,
{
    Router::new()
        .route(
            HOST_CLAIM_OR_CREATE_PATH,
            post(claim_or_create::<U, I, W, R, C>),
        )
        .with_state(ClaimState(Arc::new(claim)))
}

async fn claim_or_create<U, I, W, R, C>(
    State(state): State<ClaimState<U, I, W, R, C>>,
    principal: Option<Extension<UserId>>,
    ValidatedJson(raw): ValidatedJson<RawClaim>,
) -> Response
where
    U: ClaimLocalComputerUnitOfWork,
    I: HostCredentialInventory,
    W: CredentialIssueUnitOfWork,
    R: EntropySource + 'static,
    C: Clock + 'static,
{
    let Some(Extension(user_id)) = principal else {
        tracing::error!("claim-or-create route reached without its human principal fact");
        return internal_error();
    };

    let request = match LocalClaimRequest::try_from(raw) {
        Ok(request) => request,
        Err(()) => return invalid_input(),
    };

    match state.0.execute(user_id, request).await {
        Ok(claimed) => {
            let host_token = claimed.host_token.map(|token| token.reveal_once());
            Json(ClaimOrCreateResponse {
                host_id: claimed.computer.id.as_str().to_owned(),
                host: host_wire(&claimed.computer),
                host_token,
            })
            .into_response()
        }
        Err(ClaimLocalComputerError::CredentialRefused(CredentialIssueRefusal::HostRevoked)) => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: HOST_REVOKED,
                error_code: HOST_REVOKED_CODE,
            }),
        )
            .into_response(),
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "claim-or-create failed"
            );
            internal_error()
        }
    }
}

/// Success body: `{ host_id, host, host_token? }`. `host_token` is present only when a
/// token was minted, matching the TypeScript `...(hostToken ? { host_token } : {})`.
#[derive(Serialize)]
struct ClaimOrCreateResponse {
    host_id: String,
    host: HostDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_token: Option<String>,
}

/// The claim body: the shape the frozen [`ValidatedJson`] extractor decodes and validates.
///
/// Serde owns structure — a missing `installation_id`, a wrong-typed field, a present
/// `null` for either optional key (via [`RequiredPatchField`]) is a native decode fault. The one stateless value rule,
/// `installation_id`'s shape, delegates to the route-local [`is_uuid`] predicate; the two
/// optional strings carry no garde rule because their sole value authority is the
/// normalization in [`TryFrom`], which turns any refusal into the minimal semantic `422`.
#[derive(Debug, Deserialize, Validate)]
struct RawClaim {
    #[garde(custom(is_uuid_rule))]
    installation_id: String,
    #[serde(default)]
    #[garde(skip)]
    label: RequiredPatchField<String>,
    #[serde(default)]
    #[garde(skip)]
    current_host_id: RequiredPatchField<String>,
}

impl TryFrom<RawClaim> for LocalClaimRequest {
    type Error = ();

    /// Normalize the admitted body into the domain request. Both optional strings run
    /// through their sole value authorities; any refusal is an opaque `()` the handler
    /// renders as the minimal semantic refusal. A `Missing` arm carries no value and maps
    /// to `None`, never a decode error — a present `null` was already rejected upstream by
    /// [`RequiredPatchField`].
    fn try_from(raw: RawClaim) -> Result<Self, Self::Error> {
        let label = match raw.label {
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(value) => {
                Some(ComputerDisplayLabel::parse(&value).map_err(|_| ())?)
            }
        };

        let current_computer = match raw.current_host_id {
            RequiredPatchField::Missing => None,
            RequiredPatchField::Value(value) => Some(HostId::parse(value).map_err(|_| ())?),
        };

        Ok(LocalClaimRequest {
            installation_id: raw.installation_id,
            label,
            current_computer,
        })
    }
}

/// The stateless garde rule bounding `installation_id`: a THIN delegate to the route-local
/// [`is_uuid`] predicate, holding no shape logic of its own.
fn is_uuid_rule(value: &str, _: &()) -> garde::Result {
    if is_uuid(value) {
        Ok(())
    } else {
        Err(garde::Error::new("installation_id must be a uuid"))
    }
}

/// The Zod uuid check reduced to its observable behavior: 8-4-4-4-12 hex with dashes at
/// the four fixed offsets. The regex's `\b` anchors sit between a hex digit and a dash —
/// always a word boundary — so they never reject anything the length-and-shape test
/// admits.
fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            *byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        convert::Infallible,
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::persistence::PersistenceFailure,
        host_runtime::{
            claim::{LocalClaimAttempt, LocalClaimWriteError},
            computer::{ComputerRecord, HostKind, ProvisioningSource},
            custody::{CredentialSweep, NewLongLivedCredential},
        },
    };
    use tower::ServiceExt;

    use super::*;

    const OWNER: &str = "u-owner";
    const CLAIMED_ID: &str = "computer-1";

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn claimed_record() -> ComputerRecord {
        ComputerRecord {
            id: HostId::parse(CLAIMED_ID).expect("host id"),
            name: "shiki-mac".to_owned(),
            kind: HostKind::Local,
            created_at: 1,
            owner: Some(user(OWNER)),
            installation_id: Some("11111111-1111-4111-8111-111111111111".to_owned()),
            last_seen_at: Some(9),
            activated_at: Some(2),
            finalized_at: Some(3),
            revoked_at: None,
            provisioning_source: ProvisioningSource::AppInstall,
            display_label: None,
        }
    }

    #[derive(Clone)]
    struct FakeClaim {
        computer: ComputerRecord,
        calls: Arc<AtomicUsize>,
    }

    impl ClaimLocalComputerUnitOfWork for FakeClaim {
        async fn claim_local_computer(
            &self,
            _attempt: LocalClaimAttempt,
        ) -> Result<ComputerRecord, LocalClaimWriteError> {
            self.calls.fetch_add(1, SeqCst);
            Ok(self.computer.clone())
        }
    }

    #[derive(Clone)]
    struct FakeInventory {
        has_active: bool,
    }

    impl HostCredentialInventory for FakeInventory {
        async fn has_active_long_lived_credential(
            &self,
            _computer: HostId,
            _now: i64,
        ) -> Result<bool, PersistenceFailure> {
            Ok(self.has_active)
        }
    }

    #[derive(Clone, Copy)]
    enum IssueOutcome {
        Ok,
        Refused(CredentialIssueRefusal),
        Fails,
    }

    #[derive(Clone)]
    struct FakeIssuer {
        outcome: IssueOutcome,
        issued: Arc<AtomicUsize>,
    }

    impl CredentialIssueUnitOfWork for FakeIssuer {
        async fn issue_credential(
            &self,
            _credential: NewLongLivedCredential,
            sweep: Option<CredentialSweep>,
        ) -> Result<Result<(), CredentialIssueRefusal>, PersistenceFailure> {
            assert!(
                sweep.is_none(),
                "a claim mints a first token; it never sweeps"
            );
            self.issued.fetch_add(1, SeqCst);
            match self.outcome {
                IssueOutcome::Ok => Ok(Ok(())),
                IssueOutcome::Refused(refusal) => Ok(Err(refusal)),
                IssueOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "issue failed",
                ))),
            }
        }
    }

    struct FixedEntropy(Mutex<VecDeque<u8>>);

    impl FixedEntropy {
        fn sequence() -> Self {
            Self(Mutex::new((0..1024).map(|index| index as u8).collect()))
        }
    }

    impl EntropySource for FixedEntropy {
        type Error = Infallible;
        fn fill(&self, destination: &mut [u8]) -> Result<(), Self::Error> {
            let mut bytes = self.0.lock().unwrap();
            for destination in destination {
                *destination = bytes.pop_front().unwrap();
            }
            Ok(())
        }
    }

    struct FixedClock;

    impl Clock for FixedClock {
        type Error = Infallible;
        fn unix_time_ms(&self) -> Result<i64, Self::Error> {
            Ok(1_700_000_000_123)
        }
    }

    struct Reply {
        status: StatusCode,
        body: String,
        claim_calls: usize,
        issued: usize,
    }

    async fn post(
        body: &str,
        with_principal: bool,
        inventory: FakeInventory,
        outcome: IssueOutcome,
    ) -> Reply {
        let claim_calls = Arc::new(AtomicUsize::new(0));
        let issued = Arc::new(AtomicUsize::new(0));
        let uow = FakeClaim {
            computer: claimed_record(),
            calls: claim_calls.clone(),
        };
        let issuer = FakeIssuer {
            outcome,
            issued: issued.clone(),
        };
        let app = router(ClaimLocalComputer::new(
            uow,
            inventory,
            issuer,
            FixedEntropy::sequence(),
            FixedClock,
        ));
        let mut request =
            Request::post(HOST_CLAIM_OR_CREATE_PATH).header("content-type", "application/json");
        if with_principal {
            request = request.extension(user(OWNER));
        }
        let response = app
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            claim_calls: claim_calls.load(SeqCst),
            issued: issued.load(SeqCst),
        }
    }

    /// The default: a principal is present, nothing is already attached, and issuance
    /// succeeds.
    async fn post_valid(body: &str) -> Reply {
        post(
            body,
            true,
            FakeInventory { has_active: false },
            IssueOutcome::Ok,
        )
        .await
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("fixtures/claim_or_create_parity.json")).unwrap()
    }

    const SEMANTIC_BODY: &str = r#"{"error":"invalid input"}"#;

    /// A value that decodes cleanly but fails its sole value authority — a bad uuid, an
    /// empty or over-long label, an empty `current_host_id` — collapses to the minimal
    /// semantic `422`, never an issue array, and never reaches the claim.
    #[tokio::test]
    async fn a_semantic_value_fault_is_the_minimal_422_before_the_claim() {
        let uuid = "11111111-1111-4111-8111-111111111111";
        let cases = [
            r#"{"installation_id":"not-a-uuid"}"#.to_owned(),
            format!(r#"{{"installation_id":"{uuid}","label":"   "}}"#),
            format!(
                r#"{{"installation_id":"{uuid}","label":"{}"}}"#,
                "x".repeat(65)
            ),
            format!(r#"{{"installation_id":"{uuid}","current_host_id":""}}"#),
        ];
        for body in cases {
            let reply = post_valid(&body).await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "for {body}");
            assert_eq!(reply.body, SEMANTIC_BODY, "for {body}");
            assert_eq!(reply.claim_calls, 0, "for {body}");
        }
    }

    /// A structural decode fault — a missing required field, a wrong-typed field, a
    /// present `null` for a non-nullable optional, or a non-object shape —
    /// keeps axum's native `Json` classification (`422` today), never our semantic
    /// envelope, and never reaches the claim.
    #[tokio::test]
    async fn a_structural_fault_keeps_the_native_rejection_before_the_claim() {
        let uuid = "11111111-1111-4111-8111-111111111111";
        let cases = [
            "{}".to_owned(),                                           // missing installation_id
            r#"{"installation_id":5}"#.to_owned(),                     // wrong type
            format!(r#"{{"installation_id":"{uuid}","label":5}}"#),    // wrong type
            format!(r#"{{"installation_id":"{uuid}","label":null}}"#), // present null
            format!(r#"{{"installation_id":"{uuid}","current_host_id":null}}"#), // present null
            "[]".to_owned(),                                           // non-object shapes
            "\"text\"".to_owned(),
            "42".to_owned(),
            "true".to_owned(),
            "null".to_owned(),
        ];
        for body in cases {
            let reply = post_valid(&body).await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "for {body}");
            assert_ne!(reply.body, SEMANTIC_BODY, "for {body}");
            assert_eq!(reply.claim_calls, 0, "for {body}");
        }
    }

    /// The exact-arity value-shaped arrays flagged: under a bare positional derive,
    /// `["<uuid>"]` would bind `installation_id` and `["<uuid>","label"]` would bind both by
    /// declaration order, decode cleanly, and REACH the claim authority. The shared
    /// object-only floor refuses the sequence at the top level (native `422`) before any
    /// positional bind, so the accepted set stays object-only and the claim is never reached.
    #[tokio::test]
    async fn a_value_shaped_array_body_is_refused_before_it_can_reach_the_claim() {
        let uuid = "11111111-1111-4111-8111-111111111111";
        let cases = [format!(r#"["{uuid}"]"#), format!(r#"["{uuid}","Studio"]"#)];
        for body in cases {
            let reply = post_valid(&body).await;
            assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "for {body}");
            assert_ne!(reply.body, SEMANTIC_BODY, "for {body}");
            assert_eq!(reply.claim_calls, 0, "for {body}");
        }
    }

    /// A body that is not JSON at all, and an empty body, are native syntax faults
    /// (`400`): the old `catch(() => null)` empty-object/`null` fallback is gone. Neither
    /// reaches the claim.
    #[tokio::test]
    async fn a_malformed_or_empty_body_is_a_native_400_before_the_claim() {
        for body in ["{not json", ""] {
            let reply = post_valid(body).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "for {body:?}");
            assert_eq!(reply.claim_calls, 0, "for {body:?}");
        }
    }

    /// Native `Json` requires the `application/json` content-type; its absence is a
    /// native `415` before the claim (the old handler accepted any content-type).
    #[tokio::test]
    async fn a_missing_content_type_is_a_native_415_before_the_claim() {
        let claim_calls = Arc::new(AtomicUsize::new(0));
        let uow = FakeClaim {
            computer: claimed_record(),
            calls: claim_calls.clone(),
        };
        let issuer = FakeIssuer {
            outcome: IssueOutcome::Ok,
            issued: Arc::new(AtomicUsize::new(0)),
        };
        let app = router(ClaimLocalComputer::new(
            uow,
            FakeInventory { has_active: false },
            issuer,
            FixedEntropy::sequence(),
            FixedClock,
        ));
        let request = Request::post(HOST_CLAIM_OR_CREATE_PATH)
            .extension(user(OWNER))
            .body(Body::from(
                r#"{"installation_id":"11111111-1111-4111-8111-111111111111"}"#,
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(claim_calls.load(SeqCst), 0);
    }

    /// Without the perimeter's principal the admitted body never reaches claim or issue.
    #[tokio::test]
    async fn a_missing_principal_never_claims_or_issues() {
        let reply = post(
            r#"{"installation_id":"11111111-1111-4111-8111-111111111111"}"#,
            false,
            FakeInventory { has_active: false },
            IssueOutcome::Ok,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.claim_calls, 0);
        assert_eq!(reply.issued, 0);
    }

    /// A well-formed claim projects the computer through the one wire and reveals the
    /// freshly minted token exactly once.
    #[tokio::test]
    async fn a_valid_claim_projects_the_computer_and_reveals_the_token() {
        let input = &fixture()["success_full"]["parsed"];
        let reply = post_valid(&serde_json::to_string(input).unwrap()).await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = parsed(&reply.body);
        assert_eq!(body["host_id"], CLAIMED_ID);
        assert_eq!(body["host"]["id"], CLAIMED_ID);
        assert_eq!(body["host"]["kind"], "local");
        assert_eq!(body["host"]["status"], "active");
        assert_eq!(body["host"]["creator_user_id"], OWNER);
        assert_eq!(body["host_token"].as_str().unwrap().len(), 43);
        assert_eq!(reply.claim_calls, 1);
        assert_eq!(reply.issued, 1);
    }

    /// A padded but non-empty label is accepted; trimming belongs to the value owner.
    #[tokio::test]
    async fn a_padded_label_is_accepted() {
        let reply = post_valid(
            r#"{"installation_id":"11111111-1111-4111-8111-111111111111","label":"  Foo  "}"#,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.issued, 1);
    }

    /// A caller already on this computer with a working credential is given no token:
    /// the response omits `host_token` and nothing is minted.
    #[tokio::test]
    async fn a_reattach_with_a_live_credential_omits_the_token() {
        let reply = post(
            &format!(
                r#"{{"installation_id":"11111111-1111-4111-8111-111111111111","current_host_id":"{CLAIMED_ID}"}}"#
            ),
            true,
            FakeInventory { has_active: true },
            IssueOutcome::Ok,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = parsed(&reply.body);
        assert_eq!(body["host_id"], CLAIMED_ID);
        assert!(body.get("host_token").is_none());
        assert_eq!(reply.issued, 0);
    }

    /// A credential refused because the computer was revoked under the claim is the one
    /// typed race with a `409` of its own.
    #[tokio::test]
    async fn a_revoked_credential_issue_verdict_maps_to_409() {
        let reply = post(
            r#"{"installation_id":"11111111-1111-4111-8111-111111111111"}"#,
            true,
            FakeInventory { has_active: false },
            IssueOutcome::Refused(CredentialIssueRefusal::HostRevoked),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        assert_eq!(
            parsed(&reply.body),
            json!({ "error": "host has been revoked", "error_code": "host_revoked" })
        );
        assert_eq!(reply.issued, 1);
    }

    /// Any other credential refusal is a composition fault, not a product answer, so it
    /// takes a fail-closed `500`.
    #[tokio::test]
    async fn a_non_revoked_refusal_is_a_fail_closed_500() {
        let reply = post(
            r#"{"installation_id":"11111111-1111-4111-8111-111111111111"}"#,
            true,
            FakeInventory { has_active: false },
            IssueOutcome::Refused(CredentialIssueRefusal::HostUnbound),
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
    }

    /// A persistence failure while issuing the credential is the framework's plain `500`.
    #[tokio::test]
    async fn a_credential_issue_failure_is_the_frameworks_plain_500() {
        let reply = post(
            r#"{"installation_id":"11111111-1111-4111-8111-111111111111"}"#,
            true,
            FakeInventory { has_active: false },
            IssueOutcome::Fails,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
    }
}
