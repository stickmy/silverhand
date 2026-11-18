//! Authenticated write face for `PATCH /api/agents/{id}`: the public agent-tuning
//! edit — profile text plus model/reasoning/service-tier — behind the visibility and
//! authority ladder.
//!
//! The route is a public MANAGEMENT-plane surface (creator-or-system on the human
//! side), so it composes the same two-authority ladder the sibling reads do, but with
//! an authority stage the read faces lack:
//!
//!  1. VISIBILITY ([`can_view_agent`], active-only) — may this caller see this agent
//!     at all? Absence, a caller outside the ACL, and a non-active stage all collapse
//!     to ONE `404 {"error":"agent not found"}`, so the surface is never an existence
//!     oracle for an agent the caller may not even see.
//!  2. AUTHORITY ([`can_edit_agent`]) — may this caller CHANGE it? Only the creator
//!     (or system) may; anyone else who could merely *see* it gets `403
//!     {"error":"forbidden"}`.
//!  3. WIRE ADMISSION — decode and validate the body through the shared
//!     [`ValidatedJson`](crate::foundation::wire_admission::ValidatedJson) extractor plus the
//!     `TryFrom<AgentPatchRaw>` cross-field step. Two failure planes, kept distinct:
//!     a STRUCTURAL fault (malformed JSON, a wrong-typed field, a `runtime` key, a
//!     missing `Content-Type`) keeps axum's native `Json` rejection unchanged
//!     (`400`/`422`/`415`); a SEMANTIC fault (a too-long description, a malformed
//!     reasoning token, a `display_name` the shared authority rejects, or a body naming
//!     no known field) collapses to ONE minimal `422 {"error":"invalid input"}`. The
//!     Zod `issues` array is deliberately never emitted (see the note below).
//!  4. WRITE + ANNOUNCE ([`update_agent_profile_and_announce`]) — hand the RAW request
//!     to the write unit of work, which admits tuning legality UNDER its own write
//!     lock against the runtime/model/effort as they stand THERE, then commits and
//!     fires the best-effort app-scope `agent_changed` hint. Deciding legality here
//!     against the pre-read snapshot would let two concurrent PATCHes compose an
//!     illegal final runtime; under the lock the fact judged is the fact committed. A
//!     tuning rejection is the product's typed `400 {"error":<message>,"code":<code>}`
//!     with zero write and zero announce; a row that vanished before the write is the
//!     same `404`.
//!  5. PROJECT — re-read through the canonical `agent_detail` projector and return the
//!     FULL `{ "agent": AgentDto }`, so the response (its `avatar_url` cache-buster
//!     included) reflects the just-committed `updated_at`. A concurrent delete that
//!     races the projection is honest post-commit residue: the write is durable, but
//!     no trustworthy DTO can be projected, so it takes the lane's safe `500`.
//!
//! Error ORDER is not a parity gate (shiki): the framework extracts and admits the body
//! (stage 3) before the handler runs stages 1–2, so a structural or semantic body fault
//! refuses regardless of the access verdict. The numbering is the logical decomposition,
//! not a runtime sequence, and no anti-enumeration contract depends on access preceding
//! admission — the shared extractors own extraction order.
//!
//! Like the read faces, the human principal is proof the authenticating perimeter
//! ran; its ABSENCE is a composition fault, not a product request, so it takes a
//! fail-closed `500` reading nothing. Mounting the real bearer perimeter is the
//! composition root's job, not this factory's — done by the backend-app #2234 CP2 mount.
//!
//! ## Two ratified wire divergences (flagged for review)
//!
//! 1. NO Zod `issues` array. The refusal body is terminally `{ "error": "invalid
//!    input" }` for every semantic fault (the `.refine` "no fields to update" case
//!    included), WITHOUT the per-issue array the TS route emits. This mirrors the
//!    ratified `PATCH /api/me` decision ([`crate::identity_access::human_auth`]): no shared response
//!    schema pins `issues`, no client branches on it, and reproducing the full Zod
//!    projection is a large, drift-prone surface with no reachable consumer. The typed
//!    TUNING `400` (which DOES carry an actionable `code`) is reproduced exactly.
//!
//! 2. STRUCTURAL faults take axum's native status, not a fused `400`. Where the TS
//!    route funnels every wire fault to `400`, this route lets the layer that rejects
//!    choose the status: malformed JSON is `400`, a wrong-typed or `runtime` key is the
//!    native `Json` `422`, a missing `Content-Type` is `415`; only SEMANTIC rules
//!    (garde + the cross-field `TryFrom`) use `422 {"error":"invalid input"}`. This is
//!    the anchor's deliberate, industry-standard classification (syntactic `400` vs
//!    semantic `422`), aligned with axum's own `Json` extractor rather than
//!    hand-massaged. Error text and ordering are not a parity gate; the observable
//!    `error` string on the semantic body is preserved.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::patch,
};
use garde::Validate;
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::agent_detail_read::AgentDetailReadModel,
    identity_access::agent_profile_write::{
        AgentChangedAnnounce, AgentChangedSink, AgentProfileUpdateOutcome, AgentProfileWriteError,
        AgentProfileWriteUnitOfWork, AgentTuningEdit, FieldEdit, RequiredEdit, TuningRejection,
        update_agent_profile_and_announce,
    },
    identity_access::agents::runtime_capability::{
        is_reasoning_effort_token, service_tier_from_fast_mode,
    },
    identity_access::capability::{
        agent_authority::{AgentAuthority, can_edit_agent},
        agent_view::{VisibleStages, can_view_agent},
    },
    identity_access::user_identity::DisplayName,
};

use crate::foundation::clock::ProcessClock;
use crate::foundation::error;
use crate::foundation::wire_admission::{self, ValidatedJson, patch_within_utf16_max};
use crate::foundation::wire_patch::{Forbidden, PatchField, RequiredPatchField};

use crate::identity_access::agent_wire::{AgentDto, reproject_committed_agent};

/// The product route template, in axum's `{id}` capture syntax.
pub const AGENT_PATCH_PATH: &str = "/api/agents/{id}";

/// The anti-enumeration refusal string, pinned to the TypeScript handler.
const AGENT_NOT_FOUND: &str = "agent not found";
/// The authority-refusal string (`c.json({ error: 'forbidden' }, 403)`).
const FORBIDDEN: &str = "forbidden";
/// The unmapped-error body: an unexpected failure falls to the framework default —
/// a bare `text/plain` `500`, never a JSON envelope.
const INTERNAL_ERROR_BODY: &str = "Internal Server Error";

/// `z.string().max(2000)` on the description, measured in UTF-16 code units (JS
/// `.length`).
const DESCRIPTION_MAX_UTF16: usize = 2000;
/// `ReasoningEffortTokenSchema` bounds: `.min(1).max(32)` code units.
#[derive(Clone)]
struct PatchState<R, W, S, C> {
    reads: R,
    writes: W,
    sink: S,
    clock: C,
}

pub fn router<R, W, S, C>(reads: R, writes: W, sink: S, clock: C) -> Router
where
    R: AgentDetailReadModel + Clone + Send + Sync + 'static,
    W: AgentProfileWriteUnitOfWork,
    S: AgentChangedSink,
    C: ProcessClock,
{
    Router::new()
        .route(AGENT_PATCH_PATH, patch(agent_patch::<R, W, S, C>))
        .with_state(PatchState {
            reads,
            writes,
            sink,
            clock,
        })
}

async fn agent_patch<R, W, S, C>(
    State(state): State<PatchState<R, W, S, C>>,
    Extension(viewer): Extension<UserId>,
    Path(agent_id): Path<String>,
    ValidatedJson(raw): ValidatedJson<AgentPatchRaw>,
) -> Response
where
    R: AgentDetailReadModel + Send + Sync,
    W: AgentProfileWriteUnitOfWork + Send + Sync,
    S: AgentChangedSink + Send + Sync,
    C: ProcessClock + Send + Sync,
{
    // The framework owns extraction order: `State`/`Extension`/`Path` read the request
    // parts and `ValidatedJson` reads and admits the body, all BEFORE this body runs.
    // Error ordering is not a parity gate (shiki), so admission is plainly body-first —
    // a structural or semantic body fault refuses regardless of the access verdict, and
    // no anti-enumeration contract depends on access preceding admission. The human
    // principal arrives as `Extension<UserId>`: its absence is a composition fault the
    // extractor fails closed on before this body runs.
    let principal = Principal::human(viewer);

    // Finish wire admission: `ValidatedJson` already decoded the body and ran the
    // stateless garde rules; the cross-field `TryFrom` applies `display_name`
    // normalization and the "names a field" rule, its failure the same minimal `422`.
    let request = match AgentTuningEdit::try_from(raw) {
        Ok(edit) => edit,
        Err(()) => return wire_admission::invalid_input(),
    };

    // A malformed id names no agent — the same 404 an absent row gets.
    let Ok(agent) = AgentId::parse(agent_id) else {
        return not_found();
    };

    // ONE snapshot decides visibility AND supplies the stored runtime/model/effort the
    // tuning legality is judged against, read BEFORE either gate.
    let detail = match state.reads.read_agent_detail(&agent).await {
        Ok(detail) => detail,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "agent-patch pre-write read failed"
            );
            return internal_error();
        }
    };

    // Bind the resolved row to the request before either gate: a bundle decoded for
    // another agent is a resolver defect, the lane's safe 500.
    if let Some(detail) = detail.as_ref()
        && detail.source() != &agent
    {
        tracing::error!(
            requested = agent.as_str(),
            resolved = detail.source().as_str(),
            "agent-patch resolved for the wrong agent"
        );
        return internal_error();
    }

    // VISIBILITY (active-only): absence, non-membership, and a non-active stage all
    // collapse to one anti-enumeration 404.
    let decision = can_view_agent(
        &principal,
        &agent,
        detail
            .as_ref()
            .map(silverhand_backend_core::identity_access::agent_detail_read::AgentDetailFacts::visibility),
        &VisibleStages::active_only(),
    );
    let Some(detail) = detail.filter(|_| decision.is_allowed()) else {
        return not_found();
    };

    // AUTHORITY: only the creator (or system) may change it; a viewer who is not the
    // creator gets 403.
    let authority = AgentAuthority {
        created_by_user_id: Some(detail.created_by_user_id().clone()),
    };
    if !can_edit_agent(&principal, &agent, Some(&authority)).is_allowed() {
        return forbidden();
    }

    // WRITE + ANNOUNCE. Tuning legality is NOT decided here against the pre-read
    // snapshot — that verdict could go stale before the write. The write unit of work
    // re-admits the raw request UNDER its own lock against the current runtime, so the
    // fact it judges is the fact it commits (a typed rejection returns below with zero
    // UPDATE, zero announce).
    let now_ms = state.clock.now_unix_millis();
    let outcome = match update_agent_profile_and_announce(
        &state.writes,
        &state.sink,
        &agent,
        &request,
        now_ms,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            let leaked = match &err {
                AgentProfileWriteError::Persistence(cause) => error::describe(cause),
                AgentProfileWriteError::UntrustworthyWriteFact => err.to_string(),
            };
            tracing::error!(
                error_chain = crate::foundation::safe_log_field::safe_display(&(leaked)),
                "agent-patch write failed"
            );
            return internal_error();
        }
    };
    let committed = match outcome {
        // The row vanished between the read and the write — the same 404 an absent row
        // gets at the gate.
        AgentProfileUpdateOutcome::NoSuchAgent => return not_found(),
        // Re-admitted illegal under the write lock: the product's typed 400, judged
        // against the runtime as it actually stood when the write ran.
        AgentProfileUpdateOutcome::TuningRejected(rejection) => {
            return tuning_rejected(&rejection);
        }
        AgentProfileUpdateOutcome::Committed(committed) => committed,
    };

    // The write is durable regardless of the announce; a failed hint is logged, never
    // a refusal.
    let (id, announce) = committed.into_parts();
    if let AgentChangedAnnounce::Failed(failure) = announce {
        tracing::error!(
            agent_id = id.as_str(),
            error_chain =
                crate::foundation::safe_log_field::safe_display(&(error::describe(&failure))),
            "agent-patch committed but the agent_changed announce failed"
        );
    }

    // PROJECT: re-read the committed row through the shared canonical face so the wire
    // (avatar_url cache-buster included) reflects the just-stamped `updated_at`. Every
    // post-commit fault — a raced delete, a wrong-row resolve, a read failure — is the
    // safe 500 the face logs at the boundary.
    let dto = match reproject_committed_agent(&state.reads, &id).await {
        Ok(dto) => dto,
        Err(fault) => return fault.into_safe_500("agent-patch", &id),
    };

    Json(AgentUpdateResponse { agent: dto }).into_response()
}

// ===========================================================================
// Wire admission — `UpdateAgentInputSchema`
// ===========================================================================

/// The raw PATCH body: decoded structurally by serde, validated statelessly by garde.
///
/// Every field is optional (`#[serde(default)]` → the absent/`Missing` arm when the key
/// is absent); nullable fields are three-valued via [`PatchField`]; the non-nullable
/// `display_name` uses [`RequiredPatchField`] (a present `null` is a decode error);
/// `runtime` is [`Forbidden`] (`z.never()` — a present key, any value, fails to decode).
/// Unknown keys are ignored, matching the product schema's strip.
///
/// The garde rules are the STATELESS field predicates only. `display_name`
/// normalization (trim) and the cross-field "names a field" rule are NOT here — they
/// live in `TryFrom<AgentPatchRaw>`, so garde validates without ever normalizing.
#[derive(Debug, Deserialize, Validate)]
struct AgentPatchRaw {
    /// `display_name: DisplayNameSchema.optional()`. No stateless rule — trim and bound
    /// are the shared `DisplayName::parse` authority, applied once in `TryFrom`.
    #[serde(default)]
    #[garde(skip)]
    display_name: RequiredPatchField<String>,

    /// `description: z.string().max(2000).nullable().optional()`, measured in UTF-16
    /// code units by the shared primitive the adapter delegates to.
    #[serde(default)]
    #[garde(custom(patch_within_utf16_max(DESCRIPTION_MAX_UTF16)))]
    description: PatchField<String>,

    /// `model: z.string().nullable().optional()` — any string, no bound.
    #[serde(default)]
    #[garde(skip)]
    model: PatchField<String>,

    /// `reasoning_effort: ReasoningEffortTokenSchema.nullable().optional()` — structural
    /// token format only; per-runtime legality is the later tuning gate.
    #[serde(default)]
    #[garde(custom(reasoning_token))]
    reasoning_effort: PatchField<String>,

    /// `service_tier: ServiceTierSchema.nullable().optional()`.
    #[serde(default)]
    #[garde(custom(reasoning_token))]
    service_tier: PatchField<String>,

    /// Legacy spelling from clients predating `service_tier`; never both.
    #[serde(default)]
    #[garde(skip)]
    fast_mode: PatchField<bool>,

    /// `runtime: z.never().optional()` — a present key (any value, `null` included) is a
    /// decode error, so this never carries a value past admission.
    #[serde(default)]
    #[garde(skip)]
    runtime: Forbidden,
}

/// A garde field rule for `reasoning_effort`, delegating to the sole token predicate
/// [`is_reasoning_token`]. The value-less arms carry no token and pass.
fn reasoning_token(field: &PatchField<String>, _: &()) -> garde::Result {
    match field {
        PatchField::Value(value) if !is_reasoning_effort_token(value) => {
            Err(garde::Error::new("not a reasoning-effort token"))
        }
        _ => Ok(()),
    }
}

impl TryFrom<AgentPatchRaw> for AgentTuningEdit {
    /// Semantic-admission failure; the caller maps it to the minimal `422`.
    type Error = ();

    fn try_from(raw: AgentPatchRaw) -> Result<Self, Self::Error> {
        let AgentPatchRaw {
            display_name,
            description,
            model,
            reasoning_effort,
            service_tier,
            fast_mode,
            // A present `runtime` key was already rejected at decode; matched here to
            // prove the field is consumed.
            runtime: Forbidden,
        } = raw;

        // `display_name` is trimmed and bounded by the shared authority — the one place
        // normalization happens, never in a garde rule.
        let display_name = match display_name {
            RequiredPatchField::Missing => RequiredEdit::Leave,
            RequiredPatchField::Value(value) => {
                RequiredEdit::Set(DisplayName::parse(&value).map_err(|_| ())?.into_inner())
            }
        };
        let description = field_edit(description);
        let model = field_edit(model);
        let reasoning_effort = field_edit(reasoning_effort);
        let service_tier = match (service_tier, fast_mode) {
            (tier, PatchField::Missing) => field_edit(tier),
            (PatchField::Missing, PatchField::Null) => FieldEdit::Clear,
            (PatchField::Missing, PatchField::Value(fast)) => {
                FieldEdit::Set(service_tier_from_fast_mode(fast).to_owned())
            }
            _ => return Err(()),
        };

        // `.refine(Object.keys(v).length > 0)` on the STRIPPED object: at least one KNOWN
        // field must be named (a null clear counts as named); unknown-only bodies, whose
        // keys are dropped at decode, count as empty.
        let names_a_field = !matches!(display_name, RequiredEdit::Leave)
            || description.is_present()
            || model.is_present()
            || reasoning_effort.is_present()
            || service_tier.is_present();
        if !names_a_field {
            return Err(());
        }

        Ok(AgentTuningEdit {
            display_name,
            description,
            model,
            reasoning_effort,
            service_tier,
        })
    }
}

/// Map a wire tri-state onto the domain's PATCH algebra: absent leaves, null clears, a
/// value sets. The wire type stays pure; this mapping is the route's.
fn field_edit<T>(field: PatchField<T>) -> FieldEdit<T> {
    match field {
        PatchField::Missing => FieldEdit::Leave,
        PatchField::Null => FieldEdit::Clear,
        PatchField::Value(value) => FieldEdit::Set(value),
    }
}

// ===========================================================================
// Responses
// ===========================================================================

/// Success body: `{ "agent": {AgentDto} }`, the full DTO the canonical projector
/// builds — the same shape `GET /api/agents/{id}` returns.
#[derive(Serialize)]
struct AgentUpdateResponse {
    agent: AgentDto,
}

/// The flat `{ "error": ... }` refusal body for the static refusals.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// The typed tuning refusal body, `{ "error": <message>, "code": <code> }`, mirroring
/// `agentServiceErrorBody`.
#[derive(Serialize)]
struct TuningErrorBody {
    error: String,
    code: &'static str,
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: AGENT_NOT_FOUND,
        }),
    )
        .into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(ErrorBody { error: FORBIDDEN })).into_response()
}

fn tuning_rejected(rejection: &TuningRejection) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(TuningErrorBody {
            error: rejection.message(),
            code: rejection.code(),
        }),
    )
        .into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR_BODY).into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use silverhand_backend_core::{
        foundation::identity::{AgentId, UserId},
        foundation::persistence::PersistenceFailure,
        identity_access::agent_avatar::AgentAvatarRow,
        identity_access::agent_detail_read::{
            AgentDetailFacts, AgentDetailProjection, AgentDetailReadModel,
        },
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agent_profile_write::{
            AdmittedAgentProfileEdit, AgentProfileRowWrite, AgentProfileWriteUnitOfWork,
            AgentRowBefore, AgentTuningContext, CommittedAgentRow, FieldEdit, RequiredEdit,
        },
        identity_access::agents::runtime::RuntimeKind,
    };
    use tower::ServiceExt;

    use super::*;

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    // ---- fakes ----

    #[derive(Clone)]
    struct FixedClock(i64);
    impl ProcessClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }
    const NOW: i64 = 5000;

    /// A read model that answers the pre-write read with `first` and every later read
    /// (the post-commit projection) with `rest`, so a test can make the projection
    /// race a delete or return the updated row.
    #[derive(Clone)]
    struct FakeReads {
        first: ReadOutcome,
        rest: ReadOutcome,
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    enum ReadOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
    }

    impl FakeReads {
        fn new(first: ReadOutcome, rest: ReadOutcome) -> Self {
            Self {
                first,
                rest,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            let nth = self.calls.fetch_add(1, SeqCst);
            let outcome = if nth == 0 { &self.first } else { &self.rest };
            match outcome {
                ReadOutcome::Present(facts) => Ok(Some(facts.as_ref().clone())),
                ReadOutcome::Absent => Ok(None),
                ReadOutcome::Fails => {
                    Err(PersistenceFailure::from_source(io::Error::other("boom")))
                }
            }
        }
    }

    fn present(facts: AgentDetailFacts) -> ReadOutcome {
        ReadOutcome::Present(Box::new(facts))
    }

    /// A write unit of work that models the real adapter: under the lock it hands the
    /// configured current tuning `context` (defaulting to the codex/`gpt-5.6-sol` full
    /// state) to the core-supplied DECISION to choose the branch, then reports only the
    /// WITNESSES the real port does — the before-image it read, plus, on an admit, a base
    /// row sealed so every touched column, the id, and `updated_at = now` match the core
    /// replay's seal. It never returns a product verdict; the core re-derives that from
    /// the before-image. The authoritative stored-row enforcement lives in the sqlite adapter's tests; here we only need a faithful
    /// outcome to check the handler's response mapping.
    #[derive(Clone)]
    struct SealingWrites {
        outcome: WriteOutcome,
        context: AgentTuningContext,
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    enum WriteOutcome {
        Commits,
        NoSuchAgent,
        Fails,
    }

    impl SealingWrites {
        fn new(outcome: WriteOutcome) -> Self {
            Self {
                outcome,
                context: codex_context(),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// Set the locked current context the fake decides against — e.g. a claude agent
        /// so a `service_tier` edit is re-rejected under the lock.
        fn with_context(mut self, context: AgentTuningContext) -> Self {
            self.context = context;
            self
        }
    }

    impl AgentProfileWriteUnitOfWork for SealingWrites {
        async fn write_agent_row<D>(
            &self,
            agent: &AgentId,
            decide: D,
            now_ms: i64,
        ) -> Result<AgentProfileRowWrite, PersistenceFailure>
        where
            D: FnOnce(&AgentRowBefore) -> Option<AdmittedAgentProfileEdit> + Send,
        {
            self.calls.fetch_add(1, SeqCst);
            match self.outcome {
                WriteOutcome::NoSuchAgent => Ok(AgentProfileRowWrite::NoSuchAgent),
                WriteOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "write boom",
                ))),
                WriteOutcome::Commits => {
                    // The lock is held: hand the configured before-image to the
                    // core-supplied decision to choose the branch, then report only the
                    // WITNESS — the before-image, plus the committed row on an admit. The
                    // verdict itself is the core's to re-derive from `before`; this fake
                    // never returns it.
                    let before = AgentRowBefore {
                        agent_id: agent.clone(),
                        display_name: "Base Name".to_owned(),
                        description: Some("base desc".to_owned()),
                        service_tier: Some("priority".into()),
                        avatar: AgentAvatarRow::Absent,
                        context: self.context.clone(),
                    };
                    match decide(&before) {
                        None => Ok(AgentProfileRowWrite::Unchanged { before }),
                        Some(edit) => {
                            let display_name = match edit.display_name() {
                                RequiredEdit::Leave => "Base Name".to_owned(),
                                RequiredEdit::Set(value) => value.clone(),
                            };
                            let row = CommittedAgentRow {
                                id: agent.clone(),
                                display_name,
                                description: apply_str(edit.description(), Some("base desc")),
                                model: apply_str(edit.model(), Some("gpt-5.6-sol")),
                                reasoning_effort: apply_str(edit.reasoning_effort(), Some("ultra")),
                                service_tier: apply_str(edit.service_tier(), Some("priority")),
                                avatar: AgentAvatarRow::Absent,
                                updated_at: now_ms,
                            };
                            Ok(AgentProfileRowWrite::Committed { before, row })
                        }
                    }
                }
            }
        }
    }

    fn codex_context() -> AgentTuningContext {
        AgentTuningContext {
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("ultra".to_owned()),
        }
    }

    fn claude_context() -> AgentTuningContext {
        AgentTuningContext {
            runtime: RuntimeKind::Claude,
            model: None,
            reasoning_effort: None,
        }
    }

    fn apply_str(edit: &FieldEdit<String>, base: Option<&str>) -> Option<String> {
        match edit {
            FieldEdit::Leave => base.map(str::to_owned),
            FieldEdit::Clear => None,
            FieldEdit::Set(value) => Some(value.clone()),
        }
    }

    /// A `agent_changed` sink recording its calls; `fail` makes the announce fail (the
    /// commit must still succeed).
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
    }
    impl AgentChangedSink for FakeSink {
        async fn agent_changed(
            &self,
            _agent: &AgentId,
        ) -> Result<(), silverhand_backend_core::identity_access::agent_profile_write::AgentChangedAnnounceFailure>
{
            self.calls.fetch_add(1, SeqCst);
            if self.fail {
                Err(
                    silverhand_backend_core::identity_access::agent_profile_write::AgentChangedAnnounceFailure::from_source(
                        io::Error::other("bus down"),
                    ),
                )
            } else {
                Ok(())
            }
        }
    }

    // ---- fixtures ----

    /// A codex projection: model gpt-5.6-sol, reasoning ultra, service tier priority — a full
    /// tuning state the edits below stay legal against.
    fn codex_projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Base Name".to_owned(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("ultra".to_owned()),
            service_tier: Some("priority".into()),
            host_id: "h1".to_owned(),
            host_display_label: Some("Host One".to_owned()),
            host_name: Some("host-one".to_owned()),
            description: Some("base desc".to_owned()),
            session_scope: "agent".to_owned(),
            avatar_storage_key: Some("avatars/a1.png".to_owned()),
            materialization_failed_code: None,
            materialization_failed_at: None,
            deletion_started_at: None,
            created_at: 1000,
            updated_at: 2000,
        }
    }

    /// The creator arm admits `viewer-1`; active stage; codex projection.
    fn creator_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            codex_projection(),
        )
    }

    /// A workspace-member arm: `viewer-1` may SEE it but did not create it (creator is
    /// `creator-user`), so authority refuses.
    fn workspace_only_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("creator-user"),
            [],
            [user("viewer-1")],
            MaterializationStatus::Active,
            codex_projection(),
        )
    }

    /// The committed projection the post-write read returns: the renamed display name,
    /// `high` effort, and the bumped `updated_at` the wire's avatar cache-buster reads.
    fn committed_projection() -> AgentDetailProjection {
        let mut projection = codex_projection();
        projection.display_name = "Renamed".to_owned();
        projection.reasoning_effort = Some("high".to_owned());
        projection.updated_at = NOW;
        projection
    }

    fn committed_facts() -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            committed_projection(),
        )
    }

    struct Reply {
        status: StatusCode,
        body: String,
        reads: usize,
        writes: usize,
        announces: usize,
    }

    async fn patch_with(
        reads: FakeReads,
        writes: SealingWrites,
        sink: FakeSink,
        path_id: &str,
        body: &str,
        with_principal: bool,
    ) -> Reply {
        let read_calls = reads.calls.clone();
        let write_calls = writes.calls.clone();
        let announce_calls = sink.calls.clone();
        let uri = format!("/api/agents/{path_id}");
        let mut request = Request::patch(&uri).header("content-type", "application/json");
        if with_principal {
            request = request.extension(user("viewer-1"));
        }
        let response = router(reads, writes, sink, FixedClock(NOW))
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
            reads: read_calls.load(SeqCst),
            writes: write_calls.load(SeqCst),
            announces: announce_calls.load(SeqCst),
        }
    }

    /// The ordinary admitting run: creator sees the row, writes commit and re-project.
    async fn ok_run(body: &str) -> Reply {
        patch_with(
            FakeReads::new(present(creator_facts()), present(committed_facts())),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            body,
            true,
        )
        .await
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    // ---- the happy path ----

    #[tokio::test]
    async fn a_legal_edit_calls_the_writer_announces_and_returns_the_reprojected_dto() {
        let reply = ok_run(r#"{"display_name":"Renamed","reasoning_effort":"high"}"#).await;
        assert_eq!(reply.status, StatusCode::OK);
        // The response is the FULL DTO, re-read post-commit: the new name/effort and
        // the avatar cache-buster on the just-stamped updated_at.
        let dto = &parsed(&reply.body)["agent"];
        assert_eq!(dto["display_name"], "Renamed");
        assert_eq!(dto["reasoning_effort"], "high");
        assert_eq!(dto["updated_at"], NOW);
        assert_eq!(dto["avatar_url"], format!("/api/agents/a1/avatar?v={NOW}"));
        // One write, one announce, and two reads (pre-write gate + post-commit project).
        assert_eq!(reply.writes, 1);
        assert_eq!(reply.announces, 1);
        assert_eq!(reply.reads, 2);
    }

    #[tokio::test]
    async fn a_failed_announce_does_not_flip_the_committed_write() {
        let reply = patch_with(
            FakeReads::new(present(creator_facts()), present(committed_facts())),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(true),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.announces, 1);
    }

    // ---- the ladder ----

    #[tokio::test]
    async fn a_missing_principal_is_a_safe_500_that_never_reads() {
        // The principal arrives as `Extension<UserId>`; its absence is a composition
        // fault the extractor rejects with a bare `500` BEFORE the handler runs, so no
        // read or write is attempted. The exact rejection text is the framework's, not a
        // product contract — only that it is a bare text `500`, never a JSON envelope.
        let reply = patch_with(
            FakeReads::new(present(creator_facts()), present(committed_facts())),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            serde_json::from_str::<Value>(&reply.body).is_err(),
            "the composition-fault 500 is a bare text rejection, never a JSON envelope"
        );
        assert_eq!(reply.reads, 0);
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn an_absent_row_is_the_anti_enumeration_404_and_never_writes() {
        let reply = patch_with(
            FakeReads::new(ReadOutcome::Absent, ReadOutcome::Absent),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_viewer_who_cannot_see_it_is_the_same_404() {
        // A caller in no arm at all: creator is someone else, no pair, no shared ws.
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("someone-else"),
            [],
            [],
            MaterializationStatus::Active,
            codex_projection(),
        );
        let reply = patch_with(
            FakeReads::new(present(facts.clone()), present(facts)),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_non_active_stage_is_the_same_404() {
        let facts = AgentDetailFacts::from_source(
            agent("a1"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Failed,
            codex_projection(),
        );
        let reply = patch_with(
            FakeReads::new(present(facts.clone()), present(facts)),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_viewer_who_is_not_the_creator_is_forbidden_and_never_writes() {
        let reply = patch_with(
            FakeReads::new(present(workspace_only_facts()), present(committed_facts())),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(parsed(&reply.body), json!({ "error": "forbidden" }));
        assert_eq!(reply.writes, 0);
    }

    // ---- wire admission (native structural status vs semantic 422) ----

    #[tokio::test]
    async fn a_malformed_json_body_keeps_the_native_400() {
        // Invalid JSON syntax is the one structural fault axum classifies `400`.
        let reply = ok_run("not json").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_structurally_wrong_body_keeps_the_native_422() {
        // Valid JSON that cannot decode into the body type — a non-object top-level shape,
        // a wrong field type, a present `runtime`, or a present-null `display_name` —
        // surfaces axum's native `Json` 422 (a text body, NOT the semantic envelope),
        // never a fused 400. A top-level array is sealed structurally by the shared
        // object-only gate BEFORE any positional field bind: `["Renamed"]` would otherwise
        // bind `display_name` positionally and WRITE, and `[]` would decode to an
        // all-Missing struct and reach semantic validation — both now refuse structurally.
        for body in [
            "5",
            "[]",                       // empty seq — no longer decodes to an all-default struct
            r#"["Renamed"]"#,           // value-shaped array — the positional-bind write bypass
            r#"{"runtime":"codex"}"#,   // z.never() → Forbidden decode error
            r#"{"display_name":null}"#, // not nullable → RequiredPatchField error
            r#"{"display_name":123}"#,  // not a string
            r#"{"description":5}"#,     // not a string
            r#"{"model":5}"#,           // not a string
            r#"{"service_tier":true}"#, // not a string
            r#"{"fast_mode":"yes"}"#,   // not a boolean
        ] {
            let reply = ok_run(body).await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "body {body:?} must be the native 422"
            );
            // The native rejection is NOT the semantic envelope.
            assert_ne!(
                serde_json::from_str::<Value>(&reply.body).ok(),
                Some(json!({ "error": "invalid input" })),
                "body {body:?} must carry the native rejection, not the semantic envelope"
            );
            assert_eq!(reply.writes, 0, "body {body:?} must not write");
        }
    }

    #[tokio::test]
    async fn a_semantic_rule_fault_is_the_minimal_422_invalid_input() {
        // A well-formed body the STATELESS rules or the cross-field step reject: no
        // named field, a garde predicate, or the `display_name` authority. (A top-level
        // array is NOT here — the object-only gate refuses it structurally before it can
        // decode to an empty struct; see `a_structurally_wrong_body_keeps_the_native_422`.)
        for body in [
            "{}",                             // no fields (refine)
            r#"{"unknown":1}"#,               // unknown-only → stripped → empty
            r#"{"display_name":""}"#,         // trimmed to empty → min(1) fails
            r#"{"reasoning_effort":"BAD"}"#,  // uppercase → regex fails
            r#"{"reasoning_effort":"1bad"}"#, // leading digit → regex fails
        ] {
            let reply = ok_run(body).await;
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "body {body:?} must be the semantic 422"
            );
            assert_eq!(
                parsed(&reply.body),
                json!({ "error": "invalid input" }),
                "body {body:?} must be the minimal semantic envelope"
            );
            assert_eq!(reply.writes, 0, "body {body:?} must not write");
        }
    }

    #[tokio::test]
    async fn a_description_over_two_thousand_units_is_semantic_422_but_at_the_bound_is_ok() {
        let at_bound = "x".repeat(2000);
        let over = "x".repeat(2001);
        let ok = ok_run(&format!(r#"{{"description":"{at_bound}"}}"#)).await;
        assert_eq!(ok.status, StatusCode::OK);
        let bad = ok_run(&format!(r#"{{"description":"{over}"}}"#)).await;
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(parsed(&bad.body), json!({ "error": "invalid input" }));
    }

    #[tokio::test]
    async fn a_null_clear_is_admitted_for_a_nullable_field() {
        // description: null is a Clear, a valid edit → 200 (not invalid input).
        let reply = ok_run(r#"{"description":null}"#).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.writes, 1);
    }

    // ---- tuning legality (typed 400) ----

    #[tokio::test]
    async fn native_effort_tokens_are_committed_without_a_static_model_allowlist() {
        let reply = ok_run(r#"{"reasoning_effort":"future-tier"}"#).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.writes, 1);
        assert_eq!(reply.announces, 1);
    }

    #[tokio::test]
    async fn a_service_tier_set_on_a_claude_agent_is_the_typed_tuning_400() {
        // A claude agent has no service tier, so the legacy `fast_mode` request is
        // re-rejected under the lock.
        let reply = patch_with(
            FakeReads::new(present(creator_facts()), present(committed_facts())),
            SealingWrites::new(WriteOutcome::Commits).with_context(claude_context()),
            FakeSink::new(false),
            "a1",
            r#"{"fast_mode":true}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "error": "service_tier is not valid for runtime 'claude'",
                "code": "service_tier_invalid_for_runtime"
            })
        );
        assert_eq!(reply.writes, 1);
        assert_eq!(reply.announces, 0);
    }

    // ---- write-stage outcomes ----

    #[tokio::test]
    async fn a_row_deleted_between_read_and_write_is_a_404() {
        let reply = patch_with(
            FakeReads::new(present(creator_facts()), present(committed_facts())),
            SealingWrites::new(WriteOutcome::NoSuchAgent),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(parsed(&reply.body), json!({ "error": "agent not found" }));
        assert_eq!(reply.announces, 0);
    }

    #[tokio::test]
    async fn a_write_persistence_failure_is_a_safe_500() {
        let reply = patch_with(
            FakeReads::new(present(creator_facts()), present(committed_facts())),
            SealingWrites::new(WriteOutcome::Fails),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
    }

    #[tokio::test]
    async fn a_delete_that_races_the_projection_is_honest_post_commit_residue_500() {
        // The write commits, but the row is gone before the DTO can be re-read: the
        // write is durable, yet no trustworthy DTO exists → safe 500.
        let reply = patch_with(
            FakeReads::new(present(creator_facts()), ReadOutcome::Absent),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        // It DID write and announce; only the projection failed.
        assert_eq!(reply.writes, 1);
        assert_eq!(reply.announces, 1);
    }

    #[tokio::test]
    async fn a_pre_write_read_failure_is_a_safe_500_that_never_writes() {
        let reply = patch_with(
            FakeReads::new(ReadOutcome::Fails, ReadOutcome::Fails),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.writes, 0);
    }

    #[tokio::test]
    async fn a_bundle_resolved_for_another_agent_is_a_safe_500() {
        let spliced = AgentDetailFacts::from_source(
            agent("a2"),
            user("viewer-1"),
            [],
            [],
            MaterializationStatus::Active,
            codex_projection(),
        );
        let reply = patch_with(
            FakeReads::new(present(spliced.clone()), present(spliced)),
            SealingWrites::new(WriteOutcome::Commits),
            FakeSink::new(false),
            "a1",
            r#"{"display_name":"Renamed"}"#,
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.writes, 0);
    }
}

/// Cross-language admission corpus (anchor #2236).
///
/// The `verdict`/`canonical` in the checked-in fixture
/// `packages/shared/tests/fixtures/agent-patch-admission.json` are HAND-AUTHORED, not
/// generated. The TypeScript oracle
/// (`packages/shared/tests/agent-patch-admission-parity.test.ts`) SIGNS them: it runs
/// the real `UpdateAgentInputSchema` over every case and asserts the fixture's stated
/// verdict/canonical — it verifies the file, it never writes it. This half
/// drives the SAME cases through the PRODUCTION Rust path — `AgentPatchRaw` serde decode
/// → `garde::Validate` → `TryFrom<AgentTuningEdit>` — and asserts the same accept/reject
/// verdict and, on accept, the same normalized op-form.
///
/// Scope is the SEMANTIC value plane only: is a JSON object admitted, and to what
/// op-form. HTTP status classification (malformed-JSON `400` vs semantic `422` vs `415`)
/// is the extractor matrix in [`crate::foundation::wire_admission`], not this corpus — so a
/// wrong-typed field, which the extractor path would reject as a native decode fault,
/// folds here into the single "reject" verdict.
#[cfg(test)]
mod corpus {
    use garde::Validate;
    use serde::Serialize;
    use serde_json::{Value, json};
    use silverhand_backend_core::identity_access::agent_profile_write::{FieldEdit, RequiredEdit};

    use super::{AgentPatchRaw, AgentTuningEdit, DESCRIPTION_MAX_UTF16};
    use silverhand_backend_core::identity_access::agents::runtime_capability::REASONING_EFFORT_TOKEN_MAX;

    /// The shared fixture, embedded at compile time. The path climbs five levels from
    /// this source file (`rust/crates/backend-server/src/identity_access/agent_patch.rs`) to
    /// the repo root, then down into the shared package's fixtures.
    const CORPUS_JSON: &str =
        include_str!("../../../../../packages/shared/tests/fixtures/agent-patch-admission.json");

    /// Expand a fixture value: a `{"$repeat":{"unit","times"}}` spec becomes `unit`
    /// repeated `times`, every other value passes through verbatim. Mirrors the TS
    /// `materialize`, so both halves admit byte-identical bodies.
    fn materialize(value: &Value) -> Value {
        if let Some(spec) = value.get("$repeat") {
            let unit = spec["unit"].as_str().expect("$repeat.unit is a string");
            let times = spec["times"].as_u64().expect("$repeat.times is an integer");
            return Value::String(unit.repeat(times as usize));
        }
        value.clone()
    }

    /// Materialize every value of a JSON object (the case `body`).
    fn materialize_object(object: &Value) -> Value {
        let map = object.as_object().expect("body is a JSON object");
        Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), materialize(value)))
                .collect(),
        )
    }

    /// Materialize a canonical op-form: only a `set` op carries a value to expand.
    fn materialize_canonical(canonical: &Value) -> Value {
        let map = canonical.as_object().expect("canonical is a JSON object");
        Value::Object(
            map.iter()
                .map(|(field, op)| {
                    let mut op = op.clone();
                    if op["op"].as_str() == Some("set") {
                        let value = materialize(&op["value"]);
                        op.as_object_mut()
                            .expect("op is an object")
                            .insert("value".to_owned(), value);
                    }
                    (field.clone(), op)
                })
                .collect(),
        )
    }

    /// The PRODUCTION admission over one already-materialized body. `Ok` is the admitted
    /// edit; `Err(())` folds EVERY refusal plane the wire route has — serde decode (wrong
    /// type, a forbidden `runtime` key), garde `validate`, and the cross-field `TryFrom`
    /// (over-bound `display_name`, an empty patch) — into the single reject the corpus
    /// scores against, exactly as the extractor + handler compose them on the wire.
    fn admit(body: Value) -> Result<AgentTuningEdit, ()> {
        let raw: AgentPatchRaw = serde_json::from_value(body).map_err(|_| ())?;
        raw.validate().map_err(|_| ())?;
        AgentTuningEdit::try_from(raw)
    }

    /// Project an admitted edit to the shared op-form. `display_name` is non-nullable
    /// (leave/set); the other four are leave/clear/set. This is the Rust twin of the TS
    /// oracle's `project`.
    fn project(edit: &AgentTuningEdit) -> Value {
        fn required(edit: &RequiredEdit<String>) -> Value {
            match edit {
                RequiredEdit::Leave => json!({ "op": "leave" }),
                RequiredEdit::Set(value) => json!({ "op": "set", "value": value }),
            }
        }
        fn nullable<T: Serialize>(edit: &FieldEdit<T>) -> Value {
            match edit {
                FieldEdit::Leave => json!({ "op": "leave" }),
                FieldEdit::Clear => json!({ "op": "clear" }),
                FieldEdit::Set(value) => json!({ "op": "set", "value": value }),
            }
        }
        json!({
            "display_name": required(&edit.display_name),
            "description": nullable(&edit.description),
            "model": nullable(&edit.model),
            "reasoning_effort": nullable(&edit.reasoning_effort),
            "service_tier": nullable(&edit.service_tier),
        })
    }

    #[test]
    fn rust_path_matches_the_ts_signed_verdict_and_canonical_for_every_case() {
        let corpus: Value = serde_json::from_str(CORPUS_JSON).expect("corpus parses");

        // The fixture's declared maxima must equal the constants this route encodes, so a
        // silent drift on either side is caught here rather than masked by a matching pair
        // of wrong numbers.
        assert_eq!(corpus["displayNameMax"].as_u64(), Some(64));
        assert_eq!(
            corpus["descriptionMaxUtf16"].as_u64(),
            Some(DESCRIPTION_MAX_UTF16 as u64)
        );
        assert_eq!(
            corpus["reasoningTokenMax"].as_u64(),
            Some(REASONING_EFFORT_TOKEN_MAX as u64)
        );

        let cases = corpus["cases"].as_array().expect("cases is an array");
        assert!(!cases.is_empty(), "corpus carries cases to check");

        for case in cases {
            let name = case["name"].as_str().expect("case has a name");
            let verdict = case["verdict"].as_str().expect("case has a verdict");
            let body = materialize_object(&case["body"]);

            match (verdict, admit(body)) {
                ("accept", Ok(edit)) => {
                    let expected = materialize_canonical(&case["canonical"]);
                    assert_eq!(
                        project(&edit),
                        expected,
                        "case {name:?}: op-form disagrees with the TS-signed canonical"
                    );
                }
                ("accept", Err(())) => panic!("case {name:?}: expected ACCEPT, Rust rejected"),
                ("reject", Err(())) => {
                    assert!(
                        case.get("canonical").is_none(),
                        "case {name:?}: a reject case must carry no canonical"
                    );
                }
                ("reject", Ok(_)) => panic!("case {name:?}: expected REJECT, Rust admitted"),
                (other, _) => panic!("case {name:?}: unknown verdict {other:?}"),
            }
        }
    }
}
