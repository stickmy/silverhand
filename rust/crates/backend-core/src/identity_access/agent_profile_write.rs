//! The canonical `agents`-row tuning write authority for the public `PATCH
//! /api/agents/{id}` — where the profile/tuning columns are admitted, sealed, and
//! committed, with the `agent_changed` announce they owe.
//!
//! ## What this authority owns, and what it does not
//!
//! This authority owns the `agents` columns the profile/tuning capability writes —
//! `display_name`, `description`, `model`, `reasoning_effort`, `fast_mode` — plus the
//! paired avatar pointer `avatar_storage_key` / `avatar_content_type`, whose SET/CLEAR
//! admissions ([`admit_avatar_assignment`] / [`admit_avatar_removal`]) live here even
//! though the avatar upload/clear ORCHESTRATION (blob put, orphan cleanup, announce)
//! lives in the sibling `crate::identity_access::agent_avatar_write` bounded context, which reuses THIS single
//! canonical row writer. Columns owned by OTHER capabilities are not written here: a
//! `runtime` transition, the deletion/materialization lifecycle's status columns, and
//! agent creation each have their own writers. The point is that a value legal for the
//! public tuning capability can never be smuggled through it into a column that
//! capability may not touch.
//!
//! That scoping does NOT come from a wide, freely constructible edit struct — that
//! would let any caller hand-craft a write that silently changes `model` alongside an
//! avatar. It comes from a per-capability REQUEST type admitted UNDER THE WRITE LOCK:
//! the canonical writer ([`AgentProfileWriteUnitOfWork::write_agent_row`]) reads the
//! row's before-image and hands it to a caller-supplied DECISION closure; the public
//! tuning route's decision admits its [`AgentTuningEdit`] (which can express only the
//! tuning columns) via [`admit_public_tuning_edit`], minting the opaque
//! [`AdmittedAgentProfileEdit`] — the only thing the `SET` builder reads — against the
//! locked before-context, never before. The writer names no capability's rule, so no
//! second capability-specific writer is needed: avatar and internal-profile supply
//! their own decisions to the SAME port. A caller cannot express a column its
//! capability may not touch (its request type has no such field). Separately, that the
//! production adapter writes ONLY the columns the sealed edit names — never a stray
//! `Leave` column — is a property of the adapter's `SET` builder, proven by the SQLite
//! discriminator tests; it is enforcement by construction, not type-level
//! unrepresentability.
//!
//! ## Legality is judged UNDER THE WRITE LOCK, against the row's CURRENT runtime
//!
//! PATCH cannot change `runtime` (the product's `z.never()`), so the tuning legality
//! ([`crate::identity_access::agents::runtime_capability`]) is judged against the runtime
//! read UNDER THE WRITE LOCK — not a pre-read snapshot. Judging it earlier would open a
//! TOCTOU window: two concurrent PATCHes, each legal against the same stale state,
//! could compose an illegal final row. Admitting
//! the raw request inside the lock, against the locked current values, closes that
//! window — and because the port never accepts a pre-sealed edit, there is nothing a
//! caller could admit against a stale snapshot in the first place. `reasoning_effort`
//! is judged first, then `fast_mode`, mirroring `agent-service.ts`'s
//! `normalizeRuntimeTuning`. A refusal is a product `400` carried in [`TuningRejection`]
//! with zero UPDATE and zero announce, never a write attempt.
//!
//! ## The committed fact is NARROW; the announce and DTO sit ABOVE it
//!
//! The committed fact ([`CommittedAgentRow`]) seals only what the write actually
//! committed — the agent id, the write instant, and the touched column values — and
//! ends there. It carries NO announce fate and NO host `LEFT JOIN` label or avatar
//! URL. The `agent_changed` announce is a post-commit effect whose fate lives in the
//! OUTER orchestration outcome ([`AnnouncedAgentProfileUpdate`]), above the durable
//! commit; the wire DTO is a projection the server layer composes by re-reading
//! through the canonical `agent_detail` read projector after the commit. A response
//! field is not a write fact, and an effect's fate is not a committed fact.

use std::future::Future;

use thiserror::Error;

use crate::{
    foundation::identity::AgentId,
    foundation::persistence::PersistenceFailure,
    identity_access::agent_avatar::{AgentAvatarAssignment, AgentAvatarRow},
    identity_access::agents::{
        runtime::RuntimeKind,
        runtime_capability::{reasoning_effort_valid_for_runtime, runtime_supports_fast_mode},
    },
};

// ===========================================================================
// The edit algebra: three-valued for nullable columns, two-valued for the
// non-nullable display name.
// ===========================================================================

/// A three-valued edit for a NULLABLE column: absent leaves it untouched, an
/// explicit null clears it, a value sets it — the product's `absent | null | value`
/// PATCH algebra for `description`, `model`, `reasoning_effort`, `fast_mode`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FieldEdit<T> {
    Leave,
    Clear,
    Set(T),
}

impl<T> FieldEdit<T> {
    /// Whether this edit names the column at all — `true` for `Clear`/`Set`. Used to
    /// decide when the tuning legality must be re-judged, mirroring the product's
    /// `reasoning_effort !== undefined || model !== undefined` guard.
    pub fn is_present(&self) -> bool {
        !matches!(self, FieldEdit::Leave)
    }
}

/// A two-valued edit for the NON-nullable `display_name`: absent leaves it, a value
/// sets it. It can never be cleared — the product models it `z.string().optional()`
/// with no null arm.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequiredEdit<T> {
    Leave,
    Set(T),
}

// ===========================================================================
// The public request and the current-state witness admission reads.
// ===========================================================================

/// The parsed, format-valid PATCH body — the columns the public tuning route may
/// touch. Format admission (trim, length, the `runtime: z.never()` refusal) is a
/// wire concern settled in the server layer before this is built; this struct
/// carries only the shape the domain admits for legality.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTuningEdit {
    pub display_name: RequiredEdit<String>,
    pub description: FieldEdit<String>,
    pub model: FieldEdit<String>,
    pub reasoning_effort: FieldEdit<String>,
    pub fast_mode: FieldEdit<bool>,
}

/// The stored state the tuning legality is judged against. `runtime` is fixed (PATCH
/// cannot change it); `model`/`reasoning_effort` are the current values a left-alone
/// column falls back to when computing the post-edit pair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTuningContext {
    pub runtime: RuntimeKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// Why a tuning edit was refused at admission — the product's two runtime-legality
/// `400`s. No blob or row was touched. Each arm carries the values its user-facing
/// message renders, so the refusal is a fully-formed domain fact (code + message)
/// exactly as `agent-service.ts` throws `AgentServiceError(code, message)` — the
/// server layer only copies the pair onto the wire, it composes no text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuningRejection {
    /// The post-edit `reasoning_effort` is not legal for the stored runtime/model —
    /// the product's `reasoning_effort_invalid_for_runtime`. Carries the offending
    /// post-edit token and the post-edit model (a left-alone field resolved to its
    /// stored value) so the message renders the same values the product does.
    ReasoningEffortInvalidForRuntime {
        runtime: RuntimeKind,
        reasoning_effort: String,
        model: Option<String>,
    },
    /// `fast_mode` was set on a runtime that does not support it — the product's
    /// `fast_mode_invalid_for_runtime`.
    FastModeInvalidForRuntime { runtime: RuntimeKind },
}

impl TuningRejection {
    /// The stable public error code, unchanged from the product's `AgentServiceError`.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ReasoningEffortInvalidForRuntime { .. } => "reasoning_effort_invalid_for_runtime",
            Self::FastModeInvalidForRuntime { .. } => "fast_mode_invalid_for_runtime",
        }
    }

    /// The byte-exact user-facing message, mirroring `normalizeRuntimeTuning`'s two
    /// `throw new AgentServiceError(...)` strings — including the `??  'driver default'`
    /// fallback for an absent model. This is a BEHAVIOR contract, verified against the
    /// literal product text.
    pub fn message(&self) -> String {
        match self {
            Self::ReasoningEffortInvalidForRuntime {
                runtime,
                reasoning_effort,
                model,
            } => format!(
                "reasoning_effort '{reasoning_effort}' is not valid for runtime '{}' and model '{}'",
                runtime.as_str(),
                model.as_deref().unwrap_or("driver default"),
            ),
            Self::FastModeInvalidForRuntime { runtime } => {
                format!("fast_mode is not valid for runtime '{}'", runtime.as_str())
            }
        }
    }
}

// ===========================================================================
// The sealed, capability-scoped edit the persistence port consumes.
// ===========================================================================

/// An admitted `agents`-row edit — the ONLY thing the persistence port accepts.
///
/// It has NO public constructor: a value exists only if a per-capability admission
/// fn ([`admit_public_tuning_edit`] for tuning, [`admit_avatar_assignment`] /
/// [`admit_avatar_removal`] for the avatar routes) minted it, so a caller cannot
/// fabricate a write that touches columns its capability may not. Its accessors are
/// public so the persistence adapter can read which columns to `SET`, but the shape is
/// otherwise opaque — the adapter learns the intended values, never gains the power to
/// construct an intent. Each admission mints an edit that names ONLY its own columns
/// (all others `Leave`): tuning leaves `avatar`, and the avatar admissions leave every
/// tuning column, so a write can never smuggle a column across capability boundaries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedAgentProfileEdit {
    display_name: RequiredEdit<String>,
    description: FieldEdit<String>,
    model: FieldEdit<String>,
    reasoning_effort: FieldEdit<String>,
    fast_mode: FieldEdit<bool>,
    avatar: FieldEdit<AgentAvatarAssignment>,
}

impl AdmittedAgentProfileEdit {
    pub fn display_name(&self) -> &RequiredEdit<String> {
        &self.display_name
    }
    pub fn description(&self) -> &FieldEdit<String> {
        &self.description
    }
    pub fn model(&self) -> &FieldEdit<String> {
        &self.model
    }
    pub fn reasoning_effort(&self) -> &FieldEdit<String> {
        &self.reasoning_effort
    }
    pub fn fast_mode(&self) -> &FieldEdit<bool> {
        &self.fast_mode
    }
    pub fn avatar(&self) -> &FieldEdit<AgentAvatarAssignment> {
        &self.avatar
    }
}

/// The edit that leaves every tuning column untouched — the shared base the avatar
/// admissions extend with their single avatar arm, keeping this the one place the
/// tuning columns default to `Leave`.
fn leave_tuning_columns() -> AdmittedAgentProfileEdit {
    AdmittedAgentProfileEdit {
        display_name: RequiredEdit::Leave,
        description: FieldEdit::Leave,
        model: FieldEdit::Leave,
        reasoning_effort: FieldEdit::Leave,
        fast_mode: FieldEdit::Leave,
        avatar: FieldEdit::Leave,
    }
}

/// Admit an avatar UPLOAD: set the paired avatar column to a freshly-minted blob
/// pointer + its validated content type, leaving every tuning column untouched. The
/// avatar capability judges no runtime legality — the payload was already validated
/// (empty/size/MIME) by [`crate::identity_access::avatar::validate_avatar`] before the blob was put —
/// so this admission is infallible and names ONLY the avatar column.
pub fn admit_avatar_assignment(assignment: &AgentAvatarAssignment) -> AdmittedAgentProfileEdit {
    AdmittedAgentProfileEdit {
        avatar: FieldEdit::Set(assignment.clone()),
        ..leave_tuning_columns()
    }
}

/// Admit an avatar REMOVAL: clear the paired avatar column (both `avatar_storage_key`
/// and `avatar_content_type` to NULL, written together), leaving every tuning column
/// untouched.
pub fn admit_avatar_removal() -> AdmittedAgentProfileEdit {
    AdmittedAgentProfileEdit {
        avatar: FieldEdit::Clear,
        ..leave_tuning_columns()
    }
}

/// Admit an INTERNAL self-service profile edit — the `POST /internal/agents/{aid}/profile`
/// capability (which also backs the internal avatar POST/DELETE) — into a sealed edit
/// naming ONLY `display_name`, `description`, and the paired avatar column. The
/// runtime-tuning columns (`model` / `reasoning_effort` / `fast_mode`) are forced `Leave`
/// by the shared base, so this capability can never smuggle a runtime column the way the
/// public tuning capability could. Because it names no runtime column it judges NO
/// runtime legality and is infallible: the display-name shape and description length were
/// validated by the internal-profile boundary, and an avatar `Set` carries a pointer whose
/// blob was already validated and put. `description` is a [`RequiredEdit`] (absent →
/// `Leave`, present incl. the empty string → `Set`) because this capability
/// updates-or-leaves the description and never clears it to NULL.
pub fn admit_internal_profile_edit(
    display_name: RequiredEdit<String>,
    description: RequiredEdit<String>,
    avatar: FieldEdit<AgentAvatarAssignment>,
) -> AdmittedAgentProfileEdit {
    AdmittedAgentProfileEdit {
        display_name,
        description: match description {
            RequiredEdit::Leave => FieldEdit::Leave,
            RequiredEdit::Set(value) => FieldEdit::Set(value),
        },
        avatar,
        ..leave_tuning_columns()
    }
}

/// Admit a PUBLIC tuning PATCH into a sealed edit, or refuse it. Invoked by the write
/// port INSIDE its write lock, against the runtime/model/effort just read from the
/// locked row — so the verdict is always against the row the write will land on, never
/// a stale pre-read snapshot.
///
/// Legality is judged exactly as `normalizeRuntimeTuning`: compute the post-edit
/// model and effort (a left-alone column keeps its stored value), and — only when
/// the request names `reasoning_effort` OR `model` — require the pair legal for the
/// stored runtime. Then, independently, refuse a `fast_mode` SET on a runtime that
/// does not support it. `reasoning_effort` is judged before `fast_mode`, so a body
/// that trips both surfaces the reasoning refusal, matching the product order.
pub fn admit_public_tuning_edit(
    context: &AgentTuningContext,
    request: AgentTuningEdit,
) -> Result<AdmittedAgentProfileEdit, TuningRejection> {
    // The post-edit pair: a left-alone column falls back to the stored value; a
    // clear becomes absent; a set becomes the new value.
    let next_model: Option<&str> = match &request.model {
        FieldEdit::Leave => context.model.as_deref(),
        FieldEdit::Clear => None,
        FieldEdit::Set(value) => Some(value.as_str()),
    };
    let next_reasoning: Option<&str> = match &request.reasoning_effort {
        FieldEdit::Leave => context.reasoning_effort.as_deref(),
        FieldEdit::Clear => None,
        FieldEdit::Set(value) => Some(value.as_str()),
    };

    // Only a named effort is judged; the model no longer constrains it.
    if request.reasoning_effort.is_present()
        && !reasoning_effort_valid_for_runtime(context.runtime, next_reasoning)
    {
        // `reasoning_effort_valid_for_runtime` only fails on a present token, so
        // `next_reasoning` is `Some` here; render exactly the offending pair.
        return Err(TuningRejection::ReasoningEffortInvalidForRuntime {
            runtime: context.runtime,
            reasoning_effort: next_reasoning.unwrap_or_default().to_owned(),
            model: next_model.map(str::to_owned),
        });
    }

    // A `fast_mode` SET (present, non-null) demands runtime support; a clear/leave
    // never does — the product's `!== undefined && !== null` guard.
    if matches!(request.fast_mode, FieldEdit::Set(_))
        && !runtime_supports_fast_mode(context.runtime)
    {
        return Err(TuningRejection::FastModeInvalidForRuntime {
            runtime: context.runtime,
        });
    }

    Ok(AdmittedAgentProfileEdit {
        display_name: request.display_name,
        description: request.description,
        model: request.model,
        reasoning_effort: request.reasoning_effort,
        fast_mode: request.fast_mode,
        // The tuning capability never touches the avatar column.
        avatar: FieldEdit::Leave,
    })
}

// ===========================================================================
// The persistence port and its raw outcome.
// ===========================================================================

/// The committed `agents`-row values a write returns for SEALING — the id, the write
/// instant, and every column this canonical writer can touch (the tuning columns AND
/// the paired avatar column state). This is NOT the wire DTO: it carries the avatar
/// column pair as the typed [`AgentAvatarRow`], never a rendered avatar URL, and no
/// host `LEFT JOIN` label. The adapter fills it from the `UPDATE ... RETURNING` under
/// its write lock, decoding the avatar pair through the SAME `decode_agent_avatar_row`
/// rule the before-image used, so a half-populated or malformed committed pair is an
/// integrity fault, never a silently-dropped column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedAgentRow {
    pub id: AgentId,
    pub display_name: String,
    pub description: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: Option<bool>,
    pub avatar: AgentAvatarRow,
    pub updated_at: i64,
}

/// The before-image a write judged against, bound to the agent it was read for, and
/// covering EVERY committed mutable column this canonical writer can touch — because
/// the full-column seal derives the expected after-image as `replay(before, edit)` and
/// a left-alone column must equal its before value. All fields are DB-observable
/// WITNESSES: `agent_id` and `avatar` come from the SELECTED row decoded as ONE typed
/// shape by `decode_agent_avatar_row` (never back-filled from a query parameter), and
/// `context` carries the current runtime/model/effort the tuning legality is judged
/// against. The use case asserts `agent_id == subject` before it trusts any of this to
/// replay the rule, so a cross-wire adapter cannot pass agent-B's current state as
/// agent-A's before-image (minting a refusal that is really B's, or laundering A's
/// write past B's legality).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRowBefore {
    pub agent_id: AgentId,
    pub display_name: String,
    pub description: Option<String>,
    pub fast_mode: Option<bool>,
    pub avatar: AgentAvatarRow,
    pub context: AgentTuningContext,
}

/// The raw outcome of a tuning write, decided under the adapter's write lock. Every
/// arm carries only DB-observable WITNESSES — the identity-bound before-image the
/// decision was read against, and the committed columns — never a product verdict and
/// never an adapter-minted admitted intent. The domain re-derives the verdict from the
/// witness by replaying the SAME pure rule (see [`update_agent_profile_and_announce`]),
/// binding the before-image's own `agent_id` to the request subject first, so a buggy
/// or lying adapter cannot mint a product refusal, seal an unwarranted write, or borrow
/// another agent's legality.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentProfileRowWrite {
    /// No such agent row — the route maps this to a product `404`.
    NoSuchAgent,
    /// No column was written. Carries the identity-bound before-image the port read
    /// under its lock — a WITNESS, not a verdict. For a tuning edit the only warranted
    /// reason to write nothing is a legality refusal, so the domain (after binding
    /// `before.agent_id` to the subject) replays the rule against `before.context`: a
    /// refusal mints the typed product `400`; anything the rule would ADMIT means a
    /// warranted write was withheld — an untrustworthy fact.
    Unchanged { before: AgentRowBefore },
    /// The row was updated and `updated_at` stamped to `now_ms`. Carries the
    /// identity-bound before-image the port judged against AND the committed columns —
    /// both WITNESSES. The domain binds `before.agent_id` to the subject, replays the
    /// rule against `before.context` to confirm the write was warranted, and RE-DERIVES
    /// the admitted intent it seals `row` against; the adapter never hands over an
    /// admitted intent of its own.
    Committed {
        before: AgentRowBefore,
        row: CommittedAgentRow,
    },
}

/// The atomic, capability-NEUTRAL `agents`-row write. This is the SINGLE canonical
/// writer for the `agents` row: it names no capability's rule of its own. Under ONE
/// write lock it reads the row's current runtime/model/effort into an identity-bound
/// before-image, hands that before-image to the caller-supplied `decide` closure, and:
/// - `decide` returns `None` → the capability refuses this write → nothing is written
///   and [`AgentProfileRowWrite::Unchanged`] carries the before-image it read;
/// - `decide` returns `Some(edit)` → the sealed [`AdmittedAgentProfileEdit`] is applied
///   (only its named columns), `updated_at` is stamped to `now_ms`, and the committed
///   columns come back via `RETURNING` in [`AgentProfileRowWrite::Committed`].
///
/// Each capability supplies its own decision — public tuning replays
/// [`admit_public_tuning_edit`] against the locked before-context (today's only caller),
/// avatar/internal-profile will supply theirs — so no second capability-specific writer
/// is ever needed. The decision runs INSIDE the lock, against the row the write lands
/// on, closing the tuning TOCTOU window: there is no pre-read snapshot a caller could
/// have admitted against.
///
/// The port still reports only WITNESSES — never the product verdict and never an
/// admitted intent of its own; the sealed edit `decide` yields is the caller's, minted
/// by a core admission fn the adapter cannot invoke to fabricate one. The domain
/// re-derives both the verdict and the sealing intent by replaying the same decision
/// against the witnessed before-image (see [`update_agent_profile_and_announce`]), so a
/// buggy or lying adapter can neither mint a refusal nor seal an unwarranted write.
pub trait AgentProfileWriteUnitOfWork: Clone + Send + Sync + 'static {
    fn write_agent_row<D>(
        &self,
        agent: &AgentId,
        decide: D,
        now_ms: i64,
    ) -> impl Future<Output = Result<AgentProfileRowWrite, PersistenceFailure>> + Send
    where
        D: FnOnce(&AgentRowBefore) -> Option<AdmittedAgentProfileEdit> + Send;
}

// ===========================================================================
// The announce seam.
// ===========================================================================

/// A mutating agent write owes the app-scope `agent_changed` announce carrying
/// `{ agent_id }`. This module defines the port and drives it as a best-effort,
/// carried hint; its production adapter is a narrow hub-backed sink over
/// `ScopeEventHub::publish_agent_changed` (the accepted producer), constructible in
/// the server component — NOT a mount-time residual and NOT a test-only fake.
pub trait AgentChangedSink: Clone + Send + Sync + 'static {
    fn agent_changed(
        &self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<(), AgentChangedAnnounceFailure>> + Send;
}

/// A best-effort `agent_changed` announce could not be delivered. Opaque; a failed
/// hint never downgrades an already-committed write.
#[derive(Debug, Error)]
#[error("agent_changed announce failed")]
pub struct AgentChangedAnnounceFailure {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl AgentChangedAnnounceFailure {
    pub fn from_source<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self {
            source: Box::new(source),
        }
    }
}

/// The fate of the best-effort `agent_changed` announce fired after the row was
/// already committed — carried, never dropped, so the boundary can log a failed hint
/// without turning a committed write into a refusal.
#[derive(Debug)]
#[must_use = "a committed agent write owes an agent_changed announce whose fate must be handled"]
pub enum AgentChangedAnnounce {
    Delivered,
    Failed(AgentChangedAnnounceFailure),
}

// ===========================================================================
// The public write use case.
// ===========================================================================

/// The closed outcome of [`update_agent_profile_and_announce`].
#[derive(Debug)]
pub enum AgentProfileUpdateOutcome {
    /// No such agent — the route's product `404`.
    NoSuchAgent,
    /// The tuning was refused by the legality judged under the write lock — the
    /// route's typed product `400`. No row was written and no announce fired.
    TuningRejected(TuningRejection),
    /// The tuning was committed; carries the agent id (for the post-commit DTO
    /// re-read) and the announce fate.
    Committed(AnnouncedAgentProfileUpdate),
}

/// The OUTER orchestration outcome of a committed tuning: the committed agent id
/// paired with the fate of the `agent_changed` announce fired AFTER the row was
/// already durable. It is deliberately NOT the committed fact — the committed fact
/// ([`CommittedAgentRow`], the sealed write witness) ends before any effect and
/// carries no announce. This value exists only in the effect layer, above the
/// commit. Reachable only through [`into_parts`](Self::into_parts), so a boundary
/// taking the id for the wire re-read receives the announce residue in the same move
/// and cannot drop it. It carries only the id: the full/narrow DTO is composed by
/// re-reading through the canonical `agent_detail` projector, not from this outcome.
#[derive(Debug)]
#[must_use = "a committed agent tuning carries an announce fate that must be handled"]
pub struct AnnouncedAgentProfileUpdate {
    agent_id: AgentId,
    announce: AgentChangedAnnounce,
}

impl AnnouncedAgentProfileUpdate {
    pub fn into_parts(self) -> (AgentId, AgentChangedAnnounce) {
        (self.agent_id, self.announce)
    }
}

/// Why an agent tuning write could not yield a trustworthy answer — all safe-500s.
#[derive(Debug, Error)]
pub enum AgentProfileWriteError {
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
    #[error("agent profile write returned a fact that does not match the request")]
    UntrustworthyWriteFact,
}

/// Apply a public tuning PATCH and, on a genuine commit, fire the `agent_changed`
/// announce. The RAW request is handed to the write port, which admits it against the
/// row's CURRENT runtime under its write lock (so two concurrent edits can never
/// compose an illegal final row from mutually-stale snapshots) and reports only
/// WITNESSES: the identity-bound before-image it read, plus the committed row on a
/// write. The domain, NOT the adapter, decides the product answer — after binding the
/// witness's own `before.agent_id` to the request subject (a mismatch is an
/// untrustworthy fact, safe-500) — by replaying the SAME pure
/// [`admit_public_tuning_edit`] against the witnessed before-context:
/// - [`AgentProfileRowWrite::Unchanged`] must replay to a refusal → the typed product
///   `400` ([`AgentProfileUpdateOutcome::TuningRejected`]), zero write and zero
///   announce. If the rule would instead ADMIT, a warranted write was withheld — an
///   untrustworthy fact (safe-500).
/// - [`AgentProfileRowWrite::Committed`] must replay to an admit → the admitted intent
///   the replay yields SEALS the committed row (same id, `updated_at` stamped to
///   `now_ms`, every touched column equal to the intent) before the announce fires. If
///   the rule would instead REFUSE, an unwarranted write happened — untrustworthy
///   (safe-500), never a silent success.
///
/// Neither the product verdict nor the sealing intent is ever taken from the adapter,
/// so a buggy or lying persistence layer cannot fabricate a refusal or launder an
/// illegal write past the domain.
pub async fn update_agent_profile_and_announce<W, S>(
    writes: &W,
    announce_sink: &S,
    subject: &AgentId,
    request: &AgentTuningEdit,
    now_ms: i64,
) -> Result<AgentProfileUpdateOutcome, AgentProfileWriteError>
where
    W: AgentProfileWriteUnitOfWork,
    S: AgentChangedSink,
{
    // The write is capability-neutral; PUBLIC TUNING is the decision we inject: replay
    // the canonical admission against the before-image the port reads UNDER its lock, so
    // the verdict is always against the row the write lands on. `.ok()` collapses a
    // refusal to `None` (write nothing); the domain re-derives the typed 400 below.
    let decision_request = request.clone();
    match writes
        .write_agent_row(
            subject,
            move |before| admit_public_tuning_edit(&before.context, decision_request).ok(),
            now_ms,
        )
        .await?
    {
        AgentProfileRowWrite::NoSuchAgent => Ok(AgentProfileUpdateOutcome::NoSuchAgent),
        AgentProfileRowWrite::Unchanged { before } => {
            // Bind the witness to the request subject BEFORE trusting its context: a
            // before-image read for another agent could otherwise mint a refusal that
            // is really that agent's. Then replay the rule against the bound context —
            // a refusal is the typed product 400; an admit means a warranted write was
            // silently withheld, which is untrustworthy.
            if before.agent_id != *subject {
                return Err(AgentProfileWriteError::UntrustworthyWriteFact);
            }
            match admit_public_tuning_edit(&before.context, request.clone()) {
                Err(rejection) => Ok(AgentProfileUpdateOutcome::TuningRejected(rejection)),
                Ok(_) => Err(AgentProfileWriteError::UntrustworthyWriteFact),
            }
        }
        AgentProfileRowWrite::Committed { before, row } => {
            // Bind the witness to the subject first, so another agent's legality can
            // never launder this write. A committed write MUST then have been warranted:
            // the rule replayed against the bound before-image has to ADMIT, and the
            // intent it yields — not any intent from the adapter — is what seals the row.
            if before.agent_id != *subject {
                return Err(AgentProfileWriteError::UntrustworthyWriteFact);
            }
            let admitted = admit_public_tuning_edit(&before.context, request.clone())
                .map_err(|_| AgentProfileWriteError::UntrustworthyWriteFact)?;
            seal_agent_row(subject, &before, &admitted, &row, now_ms)?;
            let announce = announce(announce_sink, subject).await;
            Ok(AgentProfileUpdateOutcome::Committed(
                AnnouncedAgentProfileUpdate {
                    agent_id: row.id,
                    announce,
                },
            ))
        }
    }
}

/// Seal a committed fact against the FULL expected after-image `replay(before, edit)`.
/// The row must be the one asked for, the write must have stamped `updated_at` to
/// `now_ms` (this writer always writes when it commits), and EVERY column must equal
/// what the admitted edit derives from the before-image: a `Set`/`Clear` names its new
/// value, and a `Leave` must equal the before value — so a commit that silently
/// mutated a column the capability did not name (an avatar write that also touched
/// `model`, a tuning write that dropped the avatar) is an untrustworthy fact. This is
/// the ONE seal both the tuning use case and the avatar use cases share, over the same
/// canonical row.
pub(crate) fn seal_agent_row(
    subject: &AgentId,
    before: &AgentRowBefore,
    edit: &AdmittedAgentProfileEdit,
    committed: &CommittedAgentRow,
    now_ms: i64,
) -> Result<(), AgentProfileWriteError> {
    let ok = committed.id == *subject
        && committed.updated_at == now_ms
        && required_matches_after(
            &edit.display_name,
            &before.display_name,
            &committed.display_name,
        )
        && nullable_matches_after(
            &edit.description,
            &before.description,
            &committed.description,
        )
        && nullable_matches_after(&edit.model, &before.context.model, &committed.model)
        && nullable_matches_after(
            &edit.reasoning_effort,
            &before.context.reasoning_effort,
            &committed.reasoning_effort,
        )
        && nullable_matches_after(&edit.fast_mode, &before.fast_mode, &committed.fast_mode)
        && avatar_matches_after(&edit.avatar, &before.avatar, &committed.avatar);
    if ok {
        Ok(())
    } else {
        Err(AgentProfileWriteError::UntrustworthyWriteFact)
    }
}

/// A non-nullable column's expected after-image: a `Set` names the new value, a
/// `Leave` must equal the before value.
fn required_matches_after<T: PartialEq>(edit: &RequiredEdit<T>, before: &T, committed: &T) -> bool {
    match edit {
        RequiredEdit::Leave => committed == before,
        RequiredEdit::Set(value) => committed == value,
    }
}

/// A nullable column's expected after-image: a `Set` names `Some(value)`, a `Clear`
/// names `None`, a `Leave` must equal the before value.
fn nullable_matches_after<T: PartialEq>(
    edit: &FieldEdit<T>,
    before: &Option<T>,
    committed: &Option<T>,
) -> bool {
    match edit {
        FieldEdit::Leave => committed == before,
        FieldEdit::Clear => committed.is_none(),
        FieldEdit::Set(value) => committed.as_ref() == Some(value),
    }
}

/// The paired avatar column's expected after-image: a `Set` names the assigned
/// pointer + content type as [`AgentAvatarRow::Present`], a `Clear` names
/// [`AgentAvatarRow::Absent`] (both columns NULL, together), a `Leave` must equal the
/// before row state.
fn avatar_matches_after(
    edit: &FieldEdit<AgentAvatarAssignment>,
    before: &AgentAvatarRow,
    committed: &AgentAvatarRow,
) -> bool {
    match edit {
        FieldEdit::Leave => committed == before,
        FieldEdit::Clear => *committed == AgentAvatarRow::Absent,
        FieldEdit::Set(assignment) => {
            *committed
                == AgentAvatarRow::Present {
                    pointer: assignment.pointer.clone(),
                    content_type: assignment.content_type,
                }
        }
    }
}

/// Fire the changed-only `agent_changed` announce and fold its result into the
/// carried value (a failed hint never downgrades the committed write).
async fn announce<S>(sink: &S, agent: &AgentId) -> AgentChangedAnnounce
where
    S: AgentChangedSink,
{
    match sink.agent_changed(agent).await {
        Ok(()) => AgentChangedAnnounce::Delivered,
        Err(failure) => AgentChangedAnnounce::Failed(failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    const NOW: i64 = 9_000;

    fn agent() -> AgentId {
        AgentId::parse("agent-1").unwrap()
    }

    fn ctx(runtime: RuntimeKind) -> AgentTuningContext {
        AgentTuningContext {
            runtime,
            model: None,
            reasoning_effort: None,
        }
    }

    fn leave_all() -> AgentTuningEdit {
        AgentTuningEdit {
            display_name: RequiredEdit::Leave,
            description: FieldEdit::Leave,
            model: FieldEdit::Leave,
            reasoning_effort: FieldEdit::Leave,
            fast_mode: FieldEdit::Leave,
        }
    }

    // ---- admission legality ----

    #[test]
    fn a_reasoning_effort_illegal_for_the_stored_runtime_is_refused() {
        // OpenCode exposes no reasoning control, so any concrete effort is out of range.
        let request = AgentTuningEdit {
            reasoning_effort: FieldEdit::Set("high".to_owned()),
            ..leave_all()
        };
        assert_eq!(
            admit_public_tuning_edit(&ctx(RuntimeKind::Opencode), request),
            Err(TuningRejection::ReasoningEffortInvalidForRuntime {
                runtime: RuntimeKind::Opencode,
                reasoning_effort: "high".to_owned(),
                model: None,
            })
        );
    }

    #[test]
    fn a_native_model_change_preserves_the_explicit_effort() {
        for runtime in [RuntimeKind::Claude, RuntimeKind::Codex] {
            let context = AgentTuningContext {
                runtime,
                model: Some("custom-model".to_owned()),
                reasoning_effort: Some("future-tier".to_owned()),
            };
            let request = AgentTuningEdit {
                model: FieldEdit::Clear,
                ..leave_all()
            };
            assert!(admit_public_tuning_edit(&context, request).is_ok());
            let request = AgentTuningEdit {
                reasoning_effort: FieldEdit::Set("BAD".to_owned()),
                ..leave_all()
            };
            assert!(admit_public_tuning_edit(&context, request).is_err());
        }
    }

    #[test]
    fn leaving_both_reasoning_inputs_absent_never_re_judges_a_stored_illegal_pair() {
        // A stored pair that would be illegal today is NOT re-judged when neither
        // reasoning_effort nor model is named — matching the product's guard. Only a
        // fast_mode edit is present, and codex supports it.
        let context = AgentTuningContext {
            runtime: RuntimeKind::Codex,
            model: None,
            reasoning_effort: Some("INVALID".to_owned()), // old malformed value, but untouched
        };
        let request = AgentTuningEdit {
            fast_mode: FieldEdit::Set(true),
            ..leave_all()
        };
        assert!(admit_public_tuning_edit(&context, request).is_ok());
    }

    #[test]
    fn fast_mode_set_on_an_unsupported_runtime_is_refused() {
        let request = AgentTuningEdit {
            fast_mode: FieldEdit::Set(false),
            ..leave_all()
        };
        // Qoder does not support fast mode; even a `false` set is refused (present,
        // non-null).
        assert_eq!(
            admit_public_tuning_edit(&ctx(RuntimeKind::Qoder), request),
            Err(TuningRejection::FastModeInvalidForRuntime {
                runtime: RuntimeKind::Qoder,
            })
        );
    }

    #[test]
    fn clearing_fast_mode_is_allowed_even_on_an_unsupported_runtime() {
        let request = AgentTuningEdit {
            fast_mode: FieldEdit::Clear,
            ..leave_all()
        };
        assert!(admit_public_tuning_edit(&ctx(RuntimeKind::Opencode), request).is_ok());
    }

    #[test]
    fn reasoning_is_judged_before_fast_mode_when_a_body_trips_both() {
        // OpenCode: reasoning `high` is illegal AND fast_mode is unsupported. The
        // reasoning refusal wins, matching the product's order.
        let request = AgentTuningEdit {
            reasoning_effort: FieldEdit::Set("high".to_owned()),
            fast_mode: FieldEdit::Set(true),
            ..leave_all()
        };
        assert_eq!(
            admit_public_tuning_edit(&ctx(RuntimeKind::Opencode), request),
            Err(TuningRejection::ReasoningEffortInvalidForRuntime {
                runtime: RuntimeKind::Opencode,
                reasoning_effort: "high".to_owned(),
                model: None,
            })
        );
    }

    #[test]
    fn the_two_rejection_messages_render_the_product_text_byte_for_byte() {
        // reasoning: absent model renders the `?? 'driver default'` fallback.
        let reasoning = TuningRejection::ReasoningEffortInvalidForRuntime {
            runtime: RuntimeKind::Codex,
            reasoning_effort: "ultra".to_owned(),
            model: None,
        };
        assert_eq!(reasoning.code(), "reasoning_effort_invalid_for_runtime");
        assert_eq!(
            reasoning.message(),
            "reasoning_effort 'ultra' is not valid for runtime 'codex' and model 'driver default'"
        );
        // reasoning: a present model is rendered verbatim.
        let reasoning_with_model = TuningRejection::ReasoningEffortInvalidForRuntime {
            runtime: RuntimeKind::Codex,
            reasoning_effort: "max".to_owned(),
            model: Some("gpt-5.6-nova".to_owned()),
        };
        assert_eq!(
            reasoning_with_model.message(),
            "reasoning_effort 'max' is not valid for runtime 'codex' and model 'gpt-5.6-nova'"
        );
        // fast_mode: just the runtime.
        let fast = TuningRejection::FastModeInvalidForRuntime {
            runtime: RuntimeKind::Qoder,
        };
        assert_eq!(fast.code(), "fast_mode_invalid_for_runtime");
        assert_eq!(fast.message(), "fast_mode is not valid for runtime 'qoder'");
    }

    #[test]
    fn a_legal_tuning_admits_and_preserves_the_column_intents() {
        let request = AgentTuningEdit {
            display_name: RequiredEdit::Set("New Name".to_owned()),
            description: FieldEdit::Clear,
            model: FieldEdit::Set("gpt-5.6-sol".to_owned()),
            reasoning_effort: FieldEdit::Set("ultra".to_owned()),
            fast_mode: FieldEdit::Set(true),
        };
        let admitted = admit_public_tuning_edit(&ctx(RuntimeKind::Codex), request).unwrap();
        assert_eq!(
            admitted.display_name(),
            &RequiredEdit::Set("New Name".to_owned())
        );
        assert_eq!(admitted.description(), &FieldEdit::Clear);
        assert_eq!(admitted.model(), &FieldEdit::Set("gpt-5.6-sol".to_owned()));
        assert_eq!(
            admitted.reasoning_effort(),
            &FieldEdit::Set("ultra".to_owned())
        );
        assert_eq!(admitted.fast_mode(), &FieldEdit::Set(true));
    }

    // ---- write orchestration ----

    #[derive(Clone)]
    struct FakeWrites(AgentProfileRowWrite);
    impl AgentProfileWriteUnitOfWork for FakeWrites {
        async fn write_agent_row<D>(
            &self,
            _agent: &AgentId,
            _decide: D,
            _now_ms: i64,
        ) -> Result<AgentProfileRowWrite, PersistenceFailure>
        where
            D: FnOnce(&AgentRowBefore) -> Option<AdmittedAgentProfileEdit> + Send,
        {
            // The liar/witness tests supply the raw outcome directly; the decision is
            // irrelevant here because the DOMAIN replays it against the witnessed
            // before-image regardless of what the adapter chose.
            Ok(self.0.clone())
        }
    }

    #[derive(Clone)]
    struct FakeSink {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }
    impl FakeSink {
        fn new(fail: bool) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                fail,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl AgentChangedSink for FakeSink {
        async fn agent_changed(&self, _agent: &AgentId) -> Result<(), AgentChangedAnnounceFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                #[derive(Debug, Error)]
                #[error("bus down")]
                struct BusDown;
                Err(AgentChangedAnnounceFailure::from_source(BusDown))
            } else {
                Ok(())
            }
        }
    }

    fn committed_row(now: i64) -> CommittedAgentRow {
        CommittedAgentRow {
            id: agent(),
            display_name: "New Name".to_owned(),
            description: None,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("ultra".to_owned()),
            fast_mode: Some(true),
            avatar: AgentAvatarRow::Absent,
            updated_at: now,
        }
    }

    fn request() -> AgentTuningEdit {
        AgentTuningEdit {
            display_name: RequiredEdit::Set("New Name".to_owned()),
            description: FieldEdit::Clear,
            model: FieldEdit::Set("gpt-5.6-sol".to_owned()),
            reasoning_effort: FieldEdit::Set("ultra".to_owned()),
            fast_mode: FieldEdit::Set(true),
        }
    }

    /// A before-image witness bound to `agent()` (the default subject). The mutable
    /// column state is arbitrary except `avatar` (Absent): [`request`] SETs every
    /// tuning column, so only the left-alone avatar is sealed against the before value.
    /// The mismatch tests build one bound to a different id directly.
    fn before(runtime: RuntimeKind) -> AgentRowBefore {
        AgentRowBefore {
            agent_id: agent(),
            display_name: "Old Name".to_owned(),
            description: None,
            fast_mode: None,
            avatar: AgentAvatarRow::Absent,
            context: ctx(runtime),
        }
    }

    /// The `Committed` arm the port returns: the before-image it judged against (codex
    /// baseline, bound to `agent()`, against which [`request`] is legal) paired with the
    /// committed columns. The domain replays the rule against `before.context` to
    /// re-derive the sealing intent.
    fn committed_outcome(row: CommittedAgentRow) -> AgentProfileRowWrite {
        AgentProfileRowWrite::Committed {
            before: before(RuntimeKind::Codex),
            row,
        }
    }

    #[tokio::test]
    async fn a_missing_row_is_no_such_agent_and_never_announces() {
        let writes = FakeWrites(AgentProfileRowWrite::NoSuchAgent);
        let sink = FakeSink::new(false);
        let outcome = update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW)
            .await
            .unwrap();
        assert!(matches!(outcome, AgentProfileUpdateOutcome::NoSuchAgent));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn an_unchanged_witness_whose_before_refuses_is_a_typed_rejection_and_never_announces() {
        // The port wrote nothing and reported the before-image it read under the lock.
        // The DOMAIN — not the adapter — replays the pure rule against that witness to
        // mint the product refusal: OpenCode exposes no reasoning control, so `high` is
        // out of range. Zero announce.
        let before = before(RuntimeKind::Opencode);
        let request = AgentTuningEdit {
            reasoning_effort: FieldEdit::Set("high".to_owned()),
            ..leave_all()
        };
        let writes = FakeWrites(AgentProfileRowWrite::Unchanged { before });
        let sink = FakeSink::new(false);
        let outcome = update_agent_profile_and_announce(&writes, &sink, &agent(), &request, NOW)
            .await
            .unwrap();
        let AgentProfileUpdateOutcome::TuningRejected(carried) = outcome else {
            panic!("expected TuningRejected");
        };
        assert_eq!(
            carried,
            TuningRejection::ReasoningEffortInvalidForRuntime {
                runtime: RuntimeKind::Opencode,
                reasoning_effort: "high".to_owned(),
                model: None,
            }
        );
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn an_unchanged_witness_whose_before_would_admit_is_untrustworthy() {
        // Liar adapter: it wrote nothing, but the rule replayed against the witnessed
        // before-image ADMITS the request — a warranted write was silently withheld.
        // The domain refuses to launder that into a product refusal. Deleting the core
        // replay would let this pass wrongly.
        let writes = FakeWrites(AgentProfileRowWrite::Unchanged {
            before: before(RuntimeKind::Codex), // request() is legal here
        });
        let sink = FakeSink::new(false);
        let result =
            update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_committed_witness_whose_before_refuses_is_untrustworthy() {
        // Liar adapter: it claims a commit, but the rule replayed against the witnessed
        // before-image REFUSES the request — no legal write could have happened. The
        // domain rejects the write and fires no announce. Deleting the core replay would
        // let this pass wrongly.
        let request = AgentTuningEdit {
            reasoning_effort: FieldEdit::Set("high".to_owned()),
            ..leave_all()
        };
        let writes = FakeWrites(AgentProfileRowWrite::Committed {
            before: before(RuntimeKind::Opencode),
            // Match every requested/untouched column, so only the illegal effort
            // rejects this fact; an unrelated seal mismatch must not mask a bypass.
            row: CommittedAgentRow {
                id: agent(),
                display_name: "Old Name".to_owned(),
                description: None,
                model: None,
                reasoning_effort: Some("high".to_owned()),
                fast_mode: None,
                avatar: AgentAvatarRow::Absent,
                updated_at: NOW,
            },
        });
        let sink = FakeSink::new(false);
        let result =
            update_agent_profile_and_announce(&writes, &sink, &agent(), &request, NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn an_unchanged_witness_bound_to_another_agent_is_untrustworthy() {
        // Cross-wire liar: the before-image is a REAL, legality-refusing context — but
        // it was read for a DIFFERENT agent. Replaying it would mint a 400 that is
        // really the other agent's. The subject bind fires first, so the domain refuses
        // to attribute it. Zero announce. Deleting the id bind would let this mint a
        // spurious 400.
        let before = AgentRowBefore {
            agent_id: AgentId::parse("agent-other").unwrap(),
            display_name: "Old Name".to_owned(),
            description: None,
            fast_mode: None,
            avatar: AgentAvatarRow::Absent,
            context: ctx(RuntimeKind::Opencode),
        };
        let request = AgentTuningEdit {
            reasoning_effort: FieldEdit::Set("high".to_owned()),
            ..leave_all()
        };
        let writes = FakeWrites(AgentProfileRowWrite::Unchanged { before });
        let sink = FakeSink::new(false);
        let result =
            update_agent_profile_and_announce(&writes, &sink, &agent(), &request, NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_committed_witness_bound_to_another_agent_is_untrustworthy() {
        // Cross-wire liar: the committed row.id IS the subject and the request is legal
        // against the before-context — but that before-image was read for a DIFFERENT
        // agent, so its legality is not the subject's to borrow. The subject bind fires
        // before the seal, so the write cannot be laundered. Deleting the id bind would
        // let this seal and succeed wrongly.
        let before = AgentRowBefore {
            agent_id: AgentId::parse("agent-other").unwrap(),
            display_name: "Old Name".to_owned(),
            description: None,
            fast_mode: None,
            avatar: AgentAvatarRow::Absent,
            context: ctx(RuntimeKind::Codex), // request() is legal here
        };
        let writes = FakeWrites(AgentProfileRowWrite::Committed {
            before,
            row: committed_row(NOW), // row.id == subject, would otherwise seal cleanly
        });
        let sink = FakeSink::new(false);
        let result =
            update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_committed_write_seals_and_announces_once() {
        let writes = FakeWrites(committed_outcome(committed_row(NOW)));
        let sink = FakeSink::new(false);
        let outcome = update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW)
            .await
            .unwrap();
        let AgentProfileUpdateOutcome::Committed(committed) = outcome else {
            panic!("expected Committed");
        };
        let (id, ann) = committed.into_parts();
        assert_eq!(id, agent());
        assert!(matches!(ann, AgentChangedAnnounce::Delivered));
        assert_eq!(sink.calls(), 1);
    }

    #[tokio::test]
    async fn a_failed_announce_is_carried_without_flipping_the_commit() {
        let writes = FakeWrites(committed_outcome(committed_row(NOW)));
        let sink = FakeSink::new(true);
        let outcome = update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW)
            .await
            .unwrap();
        let AgentProfileUpdateOutcome::Committed(committed) = outcome else {
            panic!("a failed announce must not turn the commit into a refusal");
        };
        let (_id, ann) = committed.into_parts();
        assert!(matches!(ann, AgentChangedAnnounce::Failed(_)));
    }

    #[tokio::test]
    async fn a_write_that_did_not_stamp_now_is_untrustworthy() {
        let writes = FakeWrites(committed_outcome(committed_row(NOW - 1)));
        let sink = FakeSink::new(false);
        let result =
            update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_write_whose_touched_column_disagrees_is_untrustworthy() {
        // The row came back with a display_name other than the one we set.
        let mut row = committed_row(NOW);
        row.display_name = "Someone Else".to_owned();
        let writes = FakeWrites(committed_outcome(row));
        let sink = FakeSink::new(false);
        let result =
            update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
        assert_eq!(sink.calls(), 0);
    }

    #[tokio::test]
    async fn a_write_for_a_different_agent_is_untrustworthy() {
        let mut row = committed_row(NOW);
        row.id = AgentId::parse("agent-other").unwrap();
        let writes = FakeWrites(committed_outcome(row));
        let sink = FakeSink::new(false);
        let result =
            update_agent_profile_and_announce(&writes, &sink, &agent(), &request(), NOW).await;
        assert!(matches!(
            result,
            Err(AgentProfileWriteError::UntrustworthyWriteFact)
        ));
    }
}
