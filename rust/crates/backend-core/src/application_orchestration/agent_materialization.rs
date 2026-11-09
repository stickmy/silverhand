//! The durable agent-materialization lifecycle: reserve a pending row, fail it, reopen a
//! failed one for retry, and bulk-fail the ones a crash left pending.
//!
//! Creating an agent is not one write. A row is first *reserved* in
//! `pending_materialization`, then a separate step builds the dependent graph (the DM
//! channel, the onboarding message, …) and only then may the row become `active`. This
//! module owns the durable primitives of that lifecycle that stand on their own: the
//! creator-scoped idempotency lookup, the reserve INSERT, the two failure transitions,
//! and the crash-recovery bulk fail.
//!
//! This is a distinct write authority from the capability-neutral
//! [`crate::identity_access::agent_profile_write`] row writer and the [`crate::host_runtime::runtime_transition`] lock
//! lifecycle: the profile writer edits an existing row's columns from a before-image, and
//! runtime-transition is a lock state machine; this is the `materialization_status`
//! directed state machine plus the INSERT that seeds it.
//!
//! # There is deliberately no `pending → active` transition here
//!
//! The product invariant is not "the status column flipped to `active`" but "a committed
//! `active` row always carries its full dependent graph". Activation therefore must ride
//! the same transaction that writes the graph — a transaction-scoped capability delivered
//! together with the dependents, never a standalone primitive. A production store method
//! that flips a row to `active` on its own would be a permanent, legal entry point to an
//! `active`-but-graph-less row. So this authority has no activate method *by
//! construction*: the omission is the boundary, not an oversight. `failed → pending`
//! (reopen for retry) is present because it introduces no such hazard — it only returns a
//! row to the pending stage the reserve already established.
//!
//! # The adapter witnesses; the domain mints the verdict
//!
//! Every write projects the row it actually touched from its own `RETURNING` clause, and
//! the free functions here bind the subject and seal it field by field. A CAS that
//! matched no row surfaces as [`FailOutcome::Lost`] / [`ReopenOutcome::Lost`]: `Lost`
//! means only "the guarded compare-and-set changed no row". It does NOT interpret the
//! winner's status — a caller that needs to know whether the row is now `active`,
//! `failed`, still pending, or gone must authoritatively re-read; this module never forges
//! a product verdict out of a missing `RETURNING` row.
//!
//! The idempotency lookup is likewise sealed: `WHERE (created_by_user_id, idempotency_key)`
//! is this adapter's implementation, not the port contract, so the row it returns is bound
//! against the requested creator and key here before any caller reads its status or
//! metadata. A row that names a different creator or key is a broken adapter, a fault, not
//! a hit.

use std::future::Future;

use thiserror::Error;

use crate::{
    conversation_messaging::messaging::DirectPairParticipants,
    conversation_messaging::messaging::MessageId,
    conversation_messaging::messaging::PrincipalIdentity,
    conversation_messaging::messaging::agent_onboarding::{
        ONBOARDING_DM_TITLE, ONBOARDING_DM_TITLE_KEY, OnboardingSystemMessage,
        OnboardingWriteWitness, OnboardingWritten,
    },
    foundation::identity::AgentId,
    foundation::identity::ConversationId,
    foundation::identity::HostId,
    foundation::identity::UserId,
    foundation::identity::WorkspaceId,
    foundation::persistence::PersistenceFailure,
    identity_access::agent_lifecycle::MaterializationStatus,
    identity_access::agents::runtime::RuntimeKind,
    identity_access::agents::session_scope::AgentSessionScope,
    identity_access::user_identity::ParticipantHandle,
};

/// The canonical `materialization_failed_code` the crash-recovery sweep stamps on a row a
/// boot found still pending. Pinned here so the reconciler and any reader agree on the one
/// token, and it never drifts into an adapter literal.
pub const STALE_MATERIALIZATION_FAILED_CODE: &str = "backend_crash_during_materialize";

/// The canonical `materialization_failed_code` a provisioning attempt stamps when the row
/// won the activation flip but its dependent graph could not be written. The compensating
/// fail runs in a SEPARATE transaction after the provisioning transaction — flip and graph
/// together — rolled back, so this code names "activated then could not finish the graph",
/// distinct from the crash-recovery sweep's. Pinned here so the code never drifts into an
/// adapter literal.
pub const DEPENDENT_GRAPH_FAILED_CODE: &str = "dependent_row_insert_failed";

/// A client-supplied idempotency token for a create attempt. It is a dedup key, not an
/// identity coordinate, but it is not arbitrary: it carries the same envelope the create
/// route admits (`/^[A-Za-z0-9_-]{1,64}$/`), so an illegal key is unconstructable rather
/// than a value that reaches the per-creator unique index. The partial index keys on it
/// only `WHERE idempotency_key IS NOT NULL`; a sealed key is always non-null.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotencyKey(String);

/// A create request whose idempotency key is outside the admitted envelope. A request
/// classification failure at admission, never a persisted fact — the key never reaches a
/// write.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("idempotency key must be 1-64 characters of [A-Za-z0-9_-]")]
pub struct InvalidIdempotencyKey;

impl IdempotencyKey {
    /// Parse a client-supplied key against the admitted envelope. The bound is on the
    /// character class first, so any multi-byte input is rejected before the length check
    /// and byte length equals character length for every legal key.
    pub fn parse(raw: impl AsRef<str>) -> Result<Self, InvalidIdempotencyKey> {
        let raw = raw.as_ref();
        let legal_chars = raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if legal_chars && (1..=64).contains(&raw.len()) {
            Ok(Self(raw.to_string()))
        } else {
            Err(InvalidIdempotencyKey)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The already-validated inputs of a reserve INSERT, carried verbatim to the write. Legal
/// tuning against the runtime and handle availability are upstream admission's job; this
/// value is what the reserve persists and seals its committed identity against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReserveAgentInput {
    pub agent_id: AgentId,
    /// The `@`-handle, already validated at the request boundary; the reserve seeds
    /// `display_name` from it, mirroring the TypeScript. Typed so a raw string can
    /// never reopen an illegal-handle entry point into the write.
    pub name: ParticipantHandle,
    pub runtime: RuntimeKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub fast_mode: Option<bool>,
    pub host_id: HostId,
    pub description: Option<String>,
    pub session_scope: AgentSessionScope,
    pub created_by_user_id: UserId,
    pub idempotency_key: IdempotencyKey,
}

/// The raw witness the reserve INSERT returns, decoded from its `RETURNING` clause covering
/// every column the INSERT wrote. Identity/typed columns are carried raw (as the database
/// stored them) so the domain — not the adapter — parses and binds them against the
/// request; a column that disagrees with the input is a broken write, not a product answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservedAgentRow {
    /// Decoded from the `RETURNING id`, never the query parameter.
    pub agent_id: AgentId,
    pub name: String,
    pub display_name: String,
    pub runtime: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// The stored `0`/`1`/null boolean column, decoded by the domain.
    pub fast_mode: Option<i64>,
    pub host_id: String,
    pub description: Option<String>,
    pub session_scope: String,
    pub created_by_user_id: String,
    pub materialization_status: String,
    pub idempotency_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What a reserve observed. `ConstraintConflict` means the adapter's in-transaction
/// preflight — under the write reservation, before the INSERT — found the requested handle
/// already held in the shared @-namespace OR the `(creator, idempotency_key)` pair already
/// present. It says only "some uniqueness the reserve would collide with already holds",
/// never which one: the single consumer folds it into its `nameTaken` answer, and the
/// preflight covers exactly the two collisions the write could hit (the handle uniqueness —
/// including the cross-namespace user/agent trigger — and the per-creator idempotency
/// index).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReserveOutcome {
    /// Boxed to keep the enum small: the sealed reservation is wide, the conflict arm
    /// carries nothing.
    Reserved(Box<ReservedAgent>),
    ConstraintConflict,
}

/// A sealed reservation: the pending row proven, column by column, to carry exactly what
/// the reserve requested. Minted only by [`reserve_agent`], so holding one is proof the
/// reserve durably inserted a `pending_materialization` row whose every persisted column
/// replays the request. Every committed column it carries is read back from the write's own
/// `RETURNING` row (the database's truth) — including its timestamps — except the
/// idempotency key, which is the already-validated request key this seal proved equal to
/// the persisted one, kept as its typed newtype rather than re-parsed from the row string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservedAgent {
    agent_id: AgentId,
    name: String,
    display_name: String,
    runtime: RuntimeKind,
    model: Option<String>,
    reasoning_effort: Option<String>,
    fast_mode: Option<bool>,
    host_id: HostId,
    description: Option<String>,
    session_scope: AgentSessionScope,
    created_by_user_id: UserId,
    idempotency_key: IdempotencyKey,
    created_at: i64,
    updated_at: i64,
}

impl ReservedAgent {
    fn seal(
        input: &ReserveAgentInput,
        now_ms: i64,
        row: ReservedAgentRow,
    ) -> Result<Self, MaterializationFault> {
        if row.agent_id != input.agent_id {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness names a different agent than the request",
            });
        }
        let status = MaterializationStatus::parse(&row.materialization_status).map_err(|_| {
            MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness carries an unknown materialization status",
            }
        })?;
        if status != MaterializationStatus::PendingMaterialization {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness is not pending_materialization",
            });
        }
        if row.name != input.name.as_str() {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness names a different handle than the request",
            });
        }
        // The reserve seeds display_name from the handle; a divergence means the write
        // stopped mirroring it.
        if row.display_name != input.name.as_str() {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness did not seed display_name from the handle",
            });
        }
        let runtime = RuntimeKind::parse(&row.runtime).ok_or(
            MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness carries an unknown runtime",
            },
        )?;
        if runtime != input.runtime {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness runtime differs from the request",
            });
        }
        if row.model != input.model {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness model differs from the request",
            });
        }
        if row.reasoning_effort != input.reasoning_effort {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness reasoning_effort differs from the request",
            });
        }
        let fast_mode = decode_fast_mode(row.fast_mode).ok_or(
            MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness carries a non-boolean fast_mode column",
            },
        )?;
        if fast_mode != input.fast_mode {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness fast_mode differs from the request",
            });
        }
        let host_id = HostId::parse(row.host_id).map_err(|_| {
            MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness carries an empty host",
            }
        })?;
        if host_id != input.host_id {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness names a different host than the request",
            });
        }
        if row.description != input.description {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness description differs from the request",
            });
        }
        let session_scope = AgentSessionScope::parse(&row.session_scope).ok_or({
            MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness carries an unknown session scope",
            }
        })?;
        if session_scope != input.session_scope {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness session scope differs from the request",
            });
        }
        let created_by_user_id = UserId::parse(row.created_by_user_id).map_err(|_| {
            MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness carries an empty creator",
            }
        })?;
        if created_by_user_id != input.created_by_user_id {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness names a different creator than the request",
            });
        }
        if row.idempotency_key.as_deref() != Some(input.idempotency_key.as_str()) {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness idempotency key differs from the request",
            });
        }
        if row.created_at != now_ms || row.updated_at != now_ms {
            return Err(MaterializationFault::ReserveWitnessMismatch {
                reason: "reserve witness timestamps differ from the request clock",
            });
        }
        Ok(Self {
            agent_id: row.agent_id,
            name: row.name,
            display_name: row.display_name,
            runtime,
            model: row.model,
            reasoning_effort: row.reasoning_effort,
            fast_mode,
            host_id,
            description: row.description,
            session_scope,
            created_by_user_id,
            idempotency_key: input.idempotency_key.clone(),
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn runtime(&self) -> RuntimeKind {
        self.runtime
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    pub fn fast_mode(&self) -> Option<bool> {
        self.fast_mode
    }

    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn session_scope(&self) -> AgentSessionScope {
        self.session_scope
    }

    pub fn created_by_user_id(&self) -> &UserId {
        &self.created_by_user_id
    }

    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    pub fn updated_at(&self) -> i64 {
        self.updated_at
    }
}

/// Decode the stored `fast_mode` column: `0`/`1`/null → `Some(false)`/`Some(true)`/`None`,
/// matching the `fast_mode === undefined ? null : fast_mode ? 1 : 0` the reserve writes.
/// Any other integer is a corrupt column, surfaced to the caller as a witness mismatch.
fn decode_fast_mode(raw: Option<i64>) -> Option<Option<bool>> {
    match raw {
        None => Some(None),
        Some(0) => Some(Some(false)),
        Some(1) => Some(Some(true)),
        Some(_) => None,
    }
}

/// The raw witness a failure transition returns, decoded from its `RETURNING` clause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationFlipRow {
    pub agent_id: AgentId,
    pub materialization_status: String,
    pub materialization_failed_code: Option<String>,
    pub materialization_failed_at: Option<i64>,
    pub updated_at: i64,
}

/// What a `pending|failed → failed` observed. `Lost` = the guarded CAS changed no row
/// (absent, or already `active`); the caller re-reads to learn which.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FailOutcome {
    Failed(FailedMaterialization),
    Lost,
}

/// A sealed failure: the row proven to read `failed` with the requested code and stamp.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedMaterialization {
    agent_id: AgentId,
    failed_code: String,
    failed_at: i64,
    updated_at: i64,
}

impl FailedMaterialization {
    fn seal(
        agent: &AgentId,
        failed_code: &str,
        failed_at: i64,
        now_ms: i64,
        row: MaterializationFlipRow,
    ) -> Result<Self, MaterializationFault> {
        seal_flip_subject(agent, &row, MaterializationStatus::Failed, now_ms)?;
        if row.materialization_failed_code.as_deref() != Some(failed_code) {
            return Err(MaterializationFault::FlipWitnessMismatch {
                reason: "failed witness code differs from the request",
            });
        }
        if row.materialization_failed_at != Some(failed_at) {
            return Err(MaterializationFault::FlipWitnessMismatch {
                reason: "failed witness timestamp differs from the request",
            });
        }
        Ok(Self {
            agent_id: agent.clone(),
            failed_code: failed_code.to_string(),
            failed_at,
            updated_at: now_ms,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn failed_code(&self) -> &str {
        &self.failed_code
    }

    pub fn failed_at(&self) -> i64 {
        self.failed_at
    }

    pub fn updated_at(&self) -> i64 {
        self.updated_at
    }
}

/// What a `failed → pending` (reopen for retry) observed. `Lost` = the guarded CAS changed
/// no row (absent, or not `failed`); the caller re-reads to learn which.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReopenOutcome {
    Reopened(ReopenedReservation),
    Lost,
}

/// A sealed reopen: the row proven back in `pending_materialization` with the failure
/// columns cleared.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReopenedReservation {
    agent_id: AgentId,
    updated_at: i64,
}

impl ReopenedReservation {
    fn seal(
        agent: &AgentId,
        now_ms: i64,
        row: MaterializationFlipRow,
    ) -> Result<Self, MaterializationFault> {
        seal_flip_subject(
            agent,
            &row,
            MaterializationStatus::PendingMaterialization,
            now_ms,
        )?;
        if row.materialization_failed_code.is_some() || row.materialization_failed_at.is_some() {
            return Err(MaterializationFault::FlipWitnessMismatch {
                reason: "reopen witness did not clear the failure columns",
            });
        }
        Ok(Self {
            agent_id: agent.clone(),
            updated_at: now_ms,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn updated_at(&self) -> i64 {
        self.updated_at
    }
}

/// The raw witness the activation flip (`pending_materialization → active`) returns,
/// decoded from its `RETURNING` clause. It carries at least the columns the dependent
/// graph is provisioned against — the row's OWN id, handle, session scope, and creator —
/// so the graph names subjects the write proved, never values a caller recited. Typed
/// columns ride back raw for the domain to parse and bind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationFlipRow {
    /// Decoded from the `RETURNING id`, never the query parameter.
    pub agent_id: AgentId,
    pub name: String,
    pub materialization_status: String,
    pub session_scope: String,
    pub created_by_user_id: String,
    /// The row's OWN creation moment, decoded from `RETURNING created_at`. The direct
    /// line the dependent graph opens rides THIS clock, not the activation clock, so a
    /// retried activation resolves the same durable pair timestamp the first attempt
    /// wrote — matching the row that authored it.
    pub created_at: i64,
    pub updated_at: i64,
}

/// A sealed activation: the row proven to read `active`, bound to the requested agent, with
/// its handle, session scope, and creator parsed from the flip's own after-image. Minted
/// only by [`ActivatedAgent::seal`], so holding one is proof the guarded
/// `pending_materialization → active` flip changed the row and that every identity the
/// dependent graph needs was read from the write's `RETURNING` row — not recited by the
/// caller. It is the input the provisioning unit of work reads to name the direct line's
/// participants and read the creator's handle in the same transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivatedAgent {
    agent_id: AgentId,
    name: ParticipantHandle,
    session_scope: AgentSessionScope,
    created_by_user_id: UserId,
    created_at: i64,
    updated_at: i64,
}

impl ActivatedAgent {
    /// Bind the flip after-image against the requested agent and seal its identity columns.
    /// A row that names a different agent, is not `active`, or carries an unusable handle,
    /// scope, or creator is a fault — never a sealed activation the graph would build on.
    /// `pub(crate)` because only the provisioning use cases in this crate, holding the raw
    /// after-image the port returned, may mint a sealed activation — never the write adapter.
    pub(crate) fn seal(
        agent: &AgentId,
        now_ms: i64,
        row: ActivationFlipRow,
    ) -> Result<Self, MaterializationFault> {
        if &row.agent_id != agent {
            return Err(MaterializationFault::ActivationWitnessMismatch {
                reason: "activation witness names a different agent than the request",
            });
        }
        let status = MaterializationStatus::parse(&row.materialization_status).map_err(|_| {
            MaterializationFault::ActivationWitnessMismatch {
                reason: "activation witness carries an unknown materialization status",
            }
        })?;
        if status != MaterializationStatus::Active {
            return Err(MaterializationFault::ActivationWitnessMismatch {
                reason: "activation witness is not active",
            });
        }
        let name = ParticipantHandle::parse(&row.name).map_err(|_| {
            MaterializationFault::ActivationWitnessMismatch {
                reason: "activation witness carries an unusable handle",
            }
        })?;
        let session_scope = AgentSessionScope::parse(&row.session_scope).ok_or({
            MaterializationFault::ActivationWitnessMismatch {
                reason: "activation witness carries an unknown session scope",
            }
        })?;
        let created_by_user_id = UserId::parse(row.created_by_user_id).map_err(|_| {
            MaterializationFault::ActivationWitnessMismatch {
                reason: "activation witness carries an empty creator",
            }
        })?;
        if row.updated_at != now_ms {
            return Err(MaterializationFault::ActivationWitnessMismatch {
                reason: "activation witness updated_at differs from the request clock",
            });
        }
        Ok(Self {
            agent_id: row.agent_id,
            name,
            session_scope,
            created_by_user_id,
            created_at: row.created_at,
            updated_at: now_ms,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn name(&self) -> &ParticipantHandle {
        &self.name
    }

    pub fn session_scope(&self) -> AgentSessionScope {
        self.session_scope
    }

    pub fn created_by_user_id(&self) -> &UserId {
        &self.created_by_user_id
    }

    /// The row's own `created_at`, the clock the direct line is opened on.
    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    pub fn updated_at(&self) -> i64 {
        self.updated_at
    }
}

/// The committed fact of an agent-scoped provisioning: the row is `active` AND its dependent
/// graph — the direct line between the agent and its creator, and the onboarding system
/// message on that line — is written in the same transaction. It carries the minimal
/// post-commit ids that prove the graph, no more: a reader that needs the full agent
/// projection re-reads it through the projection authority. `pair_created` distinguishes a
/// freshly minted line from a get-or-create that resolved an existing one; it is derived
/// from whether the open announced, never from a second read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentArmProvisioned {
    agent_id: AgentId,
    direct_line: ConversationId,
    onboarding_message_id: MessageId,
    pair_created: bool,
}

impl AgentArmProvisioned {
    /// Seal the committed graph from its sealed sub-facts: the activation (which names the
    /// agent), the direct line the same transaction opened and whether that open minted it,
    /// and the onboarding message's own sealed after-image. This is the ONLY mint — a caller
    /// cannot recite an `active`-and-graph-complete verdict, because each sub-fact is itself
    /// a sealed witness the adapter could not forge, and this function re-checks that the
    /// onboarding message actually landed on the line the provisioning opened. The agent id
    /// and message id are read from those sealed facts, never from parameters. `pub(crate)`:
    /// only the use case in this crate mints it, never the write adapter.
    pub(crate) fn seal(
        activated: &ActivatedAgent,
        direct_line: ConversationId,
        pair_created: bool,
        onboarding: OnboardingWritten,
    ) -> Result<Self, MaterializationFault> {
        if onboarding.conversation() != &direct_line {
            return Err(MaterializationFault::OnboardingWitnessMismatch {
                reason: "onboarding message landed on a line other than the opened direct line",
            });
        }
        Ok(Self {
            agent_id: activated.agent_id().clone(),
            direct_line,
            onboarding_message_id: onboarding.id().clone(),
            pair_created,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn direct_line(&self) -> &ConversationId {
        &self.direct_line
    }

    pub fn onboarding_message_id(&self) -> &MessageId {
        &self.onboarding_message_id
    }

    pub fn pair_created(&self) -> bool {
        self.pair_created
    }
}

/// What an activation-and-provision observed once the row's OWN session scope — read inside
/// the transaction, never carried by the caller — selected the arm. `Lost` means the guarded
/// `pending_materialization → active` flip changed no row (the reservation was absent, already
/// `active`, or `failed`): NOTHING was written. Otherwise the flip won and the scope-matched
/// dependent graph committed in the same transaction, sealed into the matching arm here. A
/// dependent-write FAILURE is none of these — it surfaces as a [`MaterializationFault`], so a
/// win-then-fail is structurally distinct from a lost flip and only the former lets the caller
/// stamp the compensating fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvisionedActivation {
    Lost,
    Agent(AgentArmProvisioned),
    Conversation(ConversationArmProvisioned),
}

/// The committed fact of a conversation-scoped provisioning: the row is `active` AND its
/// dependent graph — the agent's personal home workspace, the `Onboarding` DM channel in it,
/// the roster edges that make the creator its owner and both parties members, and the
/// onboarding message on the DM — is written in the same transaction. Like
/// [`AgentArmProvisioned`] it carries only the post-commit ids that prove the graph. Minted
/// only by [`ConversationArmProvisioned::seal`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConversationArmProvisioned {
    agent_id: AgentId,
    home_workspace: WorkspaceId,
    dm_channel: ConversationId,
    onboarding_message_id: MessageId,
}

impl ConversationArmProvisioned {
    /// Seal the committed graph from its sealed sub-facts: the activation, the home workspace
    /// and DM the same transaction wrote (already bound to the agent and creator by the use
    /// case), and the onboarding message's own sealed after-image. The onboarding message must
    /// have landed on the home DM. `pub(crate)`: only the use case mints it, never the adapter.
    pub(crate) fn seal(
        activated: &ActivatedAgent,
        home_workspace: WorkspaceId,
        dm_channel: ConversationId,
        onboarding: OnboardingWritten,
    ) -> Result<Self, MaterializationFault> {
        if onboarding.conversation() != &dm_channel {
            return Err(MaterializationFault::OnboardingWitnessMismatch {
                reason: "onboarding message landed on a line other than the home DM",
            });
        }
        Ok(Self {
            agent_id: activated.agent_id().clone(),
            home_workspace,
            dm_channel,
            onboarding_message_id: onboarding.id().clone(),
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn home_workspace(&self) -> &WorkspaceId {
        &self.home_workspace
    }

    pub fn dm_channel(&self) -> &ConversationId {
        &self.dm_channel
    }

    pub fn onboarding_message_id(&self) -> &MessageId {
        &self.onboarding_message_id
    }
}

/// The deterministic id of a conversation-scoped agent's personal home workspace: the one
/// `wsah_<agent>` derivation both the provisioning write and the seal compute, so neither
/// recites the other's literal. Mirrors `agentHomeWorkspaceId` in the other implementation.
pub fn home_workspace_id_for(agent: &AgentId) -> WorkspaceId {
    WorkspaceId::parse(format!("wsah_{}", agent.as_str()))
        .expect("a non-empty agent id yields a well-formed wsah_<agent> workspace id")
}

/// A production adapter that runs the whole activation-and-provision under its OWN
/// transaction and hands back only RAW after-images — never a sealed verdict.
///
/// This is the authority boundary. There is ONE committing entry point: the caller does NOT
/// choose the arm. The adapter flips the reservation, reads the row's OWN `session_scope`
/// inside the transaction, and writes the graph that scope selects — the creator↔agent direct
/// line for an agent-scoped row, the home-workspace graph for a conversation-scoped one. A
/// scope it does not recognise rolls the transaction back with NOTHING written (the row stays
/// pending) and surfaces as [`RawProvisionOutcome::UnrecognizedScope`]. It CANNOT mint an
/// [`AgentArmProvisioned`] or a [`ConversationArmProvisioned`], because those seals are
/// `pub(crate)` and no method here returns a sealed fact. The core use case
/// [`provision_and_activate`] is the sole minter: it subject-binds, replays the minted
/// candidates, source-binds, and seals the raw after-images post-commit. A guard miss (the
/// pending-only flip matched no row) surfaces as `Lost`; a genuine dependent-write failure
/// rolls the transaction back and surfaces as `Err`, so the use case drives the separate
/// pending-only compensation — it never restamps a row a concurrent transition won.
pub trait AgentArmProvisioningPort: Send + Sync {
    /// Flip the reservation to `active` (pending-only) and, if it won, provision the dependent
    /// graph the flipped row's OWN scope selects — all in one transaction, committed on
    /// success. Both minted candidates (the agent-arm direct line and the conversation-arm DM)
    /// are supplied; the row's scope decides which is used and the other is discarded unwritten.
    /// Return the raw after-images of the arm that committed, `Lost` on a guard miss, or
    /// `UnrecognizedScope` when the flipped row's scope is one this provisioner cannot classify
    /// (rolled back, row still pending).
    fn provision_and_activate(
        &self,
        agent: &AgentId,
        candidate_direct_line: ConversationId,
        candidate_dm_channel: ConversationId,
        onboarding_message_id: MessageId,
        now_ms: i64,
    ) -> impl Future<Output = Result<RawProvisionOutcome, PersistenceFailure>> + Send;

    /// Pending-only compensating fail after a rolled-back provision attempt: guarded `WHERE
    /// materialization_status = 'pending_materialization'` ONLY (never `IN ('pending','failed')`).
    /// Returns the after-image, or `None` when the row is no longer pending — a concurrent
    /// transition won, and its terminal state must NEVER be clobbered. The stamped code is
    /// intrinsic ([`DEPENDENT_GRAPH_FAILED_CODE`]); the use case seals the returned row.
    fn fail_after_rolled_back_provision(
        &self,
        agent: &AgentId,
        failed_at: i64,
        now_ms: i64,
    ) -> impl Future<Output = Result<Option<MaterializationFlipRow>, PersistenceFailure>> + Send;
}

/// The raw after-images an activation-and-provision returns. No sealed fact rides across the
/// port — the use case mints the verdict from whichever arm committed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawProvisionOutcome {
    /// The pending-only flip matched no row; nothing was written.
    Lost,
    /// The flipped row carries a session scope this provisioner cannot classify. The
    /// transaction rolled back, the row is still pending, and nothing was written.
    UnrecognizedScope,
    /// The row is agent-scoped: the creator↔agent direct-line graph committed.
    Agent(Box<RawAgentArmGraph>),
    /// The row is conversation-scoped: the home-workspace graph committed.
    Conversation(Box<RawConversationArmGraph>),
}

/// The creator's identity read from its OWN `users` row inside the provisioning transaction:
/// both the row's `id` and its `@`-handle. The use case binds `source_user_id` against the
/// sealed creator BEFORE it renders the onboarding greeting, so a handle read from a different
/// user than the activation creator cannot slip into the message body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawCreatorIdentity {
    pub source_user_id: String,
    pub name: String,
}

/// The raw rows an agent-scoped provisioning transaction read back before committing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawAgentArmGraph {
    pub flip: ActivationFlipRow,
    /// The creator's own-row identity, source-bound by the use case against the sealed creator.
    pub creator: RawCreatorIdentity,
    /// The direct line the graph opened, and whether the open minted it fresh. The
    /// participants are the pair the adapter passed to the core direct-pair opener — which
    /// proved the selected row's endpoints equal them — so the use case re-derives the
    /// expected pair (creator + agent, from the sealed flip) and checks equality. When
    /// `pair_created` is set the line was freshly minted and MUST be the requested candidate;
    /// an existing-line resolve (`pair_created == false`) may legitimately differ.
    pub direct_line: ConversationId,
    pub direct_line_participants: DirectPairParticipants,
    pub pair_created: bool,
    pub onboarding: OnboardingWriteWitness,
}

/// The raw rows a conversation-scoped provisioning transaction read back before committing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawConversationArmGraph {
    pub flip: ActivationFlipRow,
    pub home_workspace: RawHomeWorkspaceRow,
    pub dm_channel: RawOnboardingDmRow,
    pub agent_on_dm: RawConversationAgentEdge,
    pub creator_workspace_owner: RawWorkspaceOwnerEdge,
    pub creator_on_dm: RawConversationUserEdge,
    pub onboarding: OnboardingWriteWitness,
}

/// Every column the conversation-arm writer touches on the agent's home-workspace row, read
/// back after the insert so the seal replays the committed row against its intent — not just
/// ownership but the derived name/description and the fixed nullability/clock the writer wrote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawHomeWorkspaceRow {
    pub id: String,
    pub name: String,
    pub name_key: Option<String>,
    pub description: Option<String>,
    pub owner_agent_id: Option<String>,
    pub owner_user_id: Option<String>,
    pub archived_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Every column the conversation-arm writer touches on the home `Onboarding` DM row: identity,
/// the fixed placement/roster/title, and the fixed visibility/nullability/clock a
/// freshly opened private channel carries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawOnboardingDmRow {
    pub id: String,
    pub workspace_id: Option<String>,
    pub placement_kind: String,
    pub roster_policy: String,
    pub title: String,
    pub title_key: Option<String>,
    pub visibility: String,
    pub spawned_by_agent_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The `conversation_agents` edge binding the agent to its home DM, with its join clock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawConversationAgentEdge {
    pub conversation_id: String,
    pub agent_id: String,
    pub joined_at: i64,
}

/// The `workspace_users` owner edge binding the creator as owner of the home workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawWorkspaceOwnerEdge {
    pub workspace_id: String,
    pub user_id: String,
    pub role: String,
    pub joined_at: i64,
}

/// The `conversation_users` edge binding the creator to the home DM, with its join clock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawConversationUserEdge {
    pub conversation_id: String,
    pub user_id: String,
    pub joined_at: i64,
}

/// The description stamped on an agent's personal home workspace, derived solely from the
/// agent's handle. This is the SOLE authority for the string: the conversation-arm writer
/// binds it and [`seal_agent_home_workspace`] replays it, so the two cannot drift.
pub fn agent_home_workspace_description(agent_handle: &str) -> String {
    format!("Personal home workspace for {agent_handle}")
}

/// Activate the reservation and provision the dependent graph the flipped row's OWN session
/// scope selects — the sole path that mints a [`ProvisionedActivation`]. There is ONE
/// committing entry point: the caller supplies BOTH candidate ids but does not choose the
/// arm; the port flips once, reads the row's scope inside the transaction, writes the matching
/// graph, and hands back only raw after-images. This use case is the sole minter — it seals
/// the activation, re-verifies the sealed scope selects the arm the adapter reported (defense
/// against a liar adapter), source-binds, replays the minted candidate, and seals.
///
/// A guard miss surfaces as `Lost` (nothing written). An unrecognized scope means the adapter
/// rolled the whole transaction back with nothing written — the row is still pending — and is
/// a broken invariant surfaced as a fault, NEVER stamped failed. A dependent-write failure
/// rolled the transaction back to pending, so this drives the pending-only compensation and
/// surfaces the original cause.
pub async fn provision_and_activate<P: AgentArmProvisioningPort>(
    port: &P,
    agent: &AgentId,
    candidate_direct_line: ConversationId,
    candidate_dm_channel: ConversationId,
    onboarding_message_id: MessageId,
    now_ms: i64,
) -> Result<ProvisionedActivation, MaterializationFault> {
    let raw = match port
        .provision_and_activate(
            agent,
            candidate_direct_line.clone(),
            candidate_dm_channel.clone(),
            onboarding_message_id.clone(),
            now_ms,
        )
        .await
    {
        Ok(raw) => raw,
        Err(cause) => {
            return Err(compensate_rolled_back_provision(port, agent, now_ms, cause).await);
        }
    };
    match raw {
        RawProvisionOutcome::Lost => Ok(ProvisionedActivation::Lost),
        // The flipped row's scope was neither agent nor conversation, so the adapter rolled the
        // whole transaction back with nothing written — the row is still pending. The reserve
        // authority only ever writes a scope this dispatch classifies, so reaching here is a
        // broken invariant: surface a fault, never a product answer, and NEVER stamp the row
        // failed — it stays pending exactly as the rollback left it.
        RawProvisionOutcome::UnrecognizedScope => {
            Err(MaterializationFault::ProvisioningInvariant {
                reason: "activation reached a reservation whose session scope is unrecognized",
            })
        }
        RawProvisionOutcome::Agent(graph) => Ok(ProvisionedActivation::Agent(seal_agent_arm(
            agent,
            *graph,
            candidate_direct_line,
            onboarding_message_id,
            now_ms,
        )?)),
        RawProvisionOutcome::Conversation(graph) => {
            Ok(ProvisionedActivation::Conversation(seal_conversation_arm(
                agent,
                *graph,
                candidate_dm_channel,
                onboarding_message_id,
                now_ms,
            )?))
        }
    }
}

/// Seal the agent-scoped arm from its raw after-images: bind the activation, re-verify the
/// sealed scope selects THIS arm, source-bind the creator identity before rendering its handle,
/// re-derive the expected direct-pair endpoints, replay the minted candidate, and seal.
fn seal_agent_arm(
    agent: &AgentId,
    graph: RawAgentArmGraph,
    candidate_direct_line: ConversationId,
    onboarding_message_id: MessageId,
    now_ms: i64,
) -> Result<AgentArmProvisioned, MaterializationFault> {
    let activated = ActivatedAgent::seal(agent, now_ms, graph.flip)?;
    if activated.session_scope() != AgentSessionScope::Agent {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "agent-arm provisioning sealed a reservation that is not agent-scoped",
        });
    }
    // Source-bind the creator's own-row identity against the activation creator BEFORE its
    // handle is rendered into the greeting, so a handle read from a different user than the
    // activation creator can never slip into the onboarding body.
    if graph.creator.source_user_id != activated.created_by_user_id().as_str() {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "agent-arm creator identity names a different user than the activation creator",
        });
    }
    let creator_handle = crate::conversation_messaging::messaging::ParticipantHandle::parse(
        graph.creator.name.as_str(),
    )
    .map_err(|_| MaterializationFault::ProvisioningInvariant {
        reason: "agent-arm creator handle read from the users row is unusable",
    })?;
    let agent_handle = crate::conversation_messaging::messaging::ParticipantHandle::parse(
        activated.name().as_str(),
    )
    .map_err(|_| MaterializationFault::ProvisioningInvariant {
        reason: "agent-arm agent handle is unusable as a messaging handle",
    })?;
    let expected = DirectPairParticipants::between(
        PrincipalIdentity::Human(activated.created_by_user_id().clone()),
        PrincipalIdentity::Agent(activated.agent_id().clone()),
    )
    .map_err(|_| MaterializationFault::ProvisioningInvariant {
        reason: "agent-arm direct line would pair the agent with itself",
    })?;
    if graph.direct_line_participants != expected {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "agent-arm direct line endpoints are not the creator and the agent",
        });
    }
    // Candidate replay: a freshly minted line MUST be the requested candidate; an existing line
    // the get-or-create resolved may legitimately differ.
    if graph.pair_created && graph.direct_line != candidate_direct_line {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "agent-arm freshly minted direct line is not the requested candidate",
        });
    }
    let intent = OnboardingSystemMessage::agent_direct_line(
        graph.direct_line.clone(),
        onboarding_message_id,
        &agent_handle,
        &creator_handle,
        now_ms,
    );
    let onboarding = intent.seal_written(graph.onboarding).map_err(|mismatch| {
        MaterializationFault::OnboardingWitnessMismatch {
            reason: mismatch.reason(),
        }
    })?;
    AgentArmProvisioned::seal(
        &activated,
        graph.direct_line,
        graph.pair_created,
        onboarding,
    )
}

/// Seal the conversation-scoped arm from its raw after-images: bind the activation, re-verify
/// the sealed scope selects THIS arm, replay every touched column of the home workspace / DM /
/// roster against intent, replay the minted DM candidate, and seal.
fn seal_conversation_arm(
    agent: &AgentId,
    graph: RawConversationArmGraph,
    candidate_dm_channel: ConversationId,
    onboarding_message_id: MessageId,
    now_ms: i64,
) -> Result<ConversationArmProvisioned, MaterializationFault> {
    let activated = ActivatedAgent::seal(agent, now_ms, graph.flip)?;
    if activated.session_scope() != AgentSessionScope::Conversation {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm provisioning sealed a reservation that is not conversation-scoped",
        });
    }
    let home = seal_agent_home_workspace(&activated, graph.home_workspace, now_ms)?;
    let dm = seal_onboarding_dm(&home, graph.dm_channel, now_ms)?;
    // Candidate replay: the DM is a fresh INSERT (never a get-or-create), so the row the
    // transaction wrote MUST be the requested candidate.
    if dm != candidate_dm_channel {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM is not the requested candidate",
        });
    }
    seal_conversation_roster(
        &activated,
        &home,
        &dm,
        &graph.agent_on_dm,
        &graph.creator_workspace_owner,
        &graph.creator_on_dm,
        now_ms,
    )?;
    let agent_handle = crate::conversation_messaging::messaging::ParticipantHandle::parse(
        activated.name().as_str(),
    )
    .map_err(|_| MaterializationFault::ProvisioningInvariant {
        reason: "conversation-arm agent handle is unusable as a messaging handle",
    })?;
    let intent = OnboardingSystemMessage::conversation_dm(
        dm.clone(),
        onboarding_message_id,
        &agent_handle,
        now_ms,
    );
    let onboarding = intent.seal_written(graph.onboarding).map_err(|mismatch| {
        MaterializationFault::OnboardingWitnessMismatch {
            reason: mismatch.reason(),
        }
    })?;
    ConversationArmProvisioned::seal(&activated, home, dm, onboarding)
}

/// Drive the pending-only compensation after a provisioning transaction rolled back on a
/// dependent-write failure, and fold the outcome into the fault to surface.
///
/// The flip won, then a dependent write failed, so the whole transaction rolled the row back
/// to `pending_materialization`. This stamps the compensating fail — but ONLY if the row is
/// still pending: a concurrent transition may have won between the rollback and here, and its
/// terminal state is authoritative and must never be overwritten. `None` from the port is
/// exactly that race, so the compensation is correctly a no-op. Either way the original
/// dependent-write failure (`cause`) is the fault the caller sees; a compensation that itself
/// faults supersedes it.
async fn compensate_rolled_back_provision<P: AgentArmProvisioningPort>(
    port: &P,
    agent: &AgentId,
    now_ms: i64,
    cause: PersistenceFailure,
) -> MaterializationFault {
    match port
        .fail_after_rolled_back_provision(agent, now_ms, now_ms)
        .await
    {
        Ok(Some(row)) => {
            match FailedMaterialization::seal(
                agent,
                DEPENDENT_GRAPH_FAILED_CODE,
                now_ms,
                now_ms,
                row,
            ) {
                Ok(_) => MaterializationFault::Persistence(cause),
                Err(fault) => fault,
            }
        }
        Ok(None) => MaterializationFault::Persistence(cause),
        Err(compensation) => MaterializationFault::Persistence(compensation),
    }
}

/// Replay every column the writer touched on the home-workspace after-image: its id is the
/// agent's derived `wsah_<agent>`, its name and description are the handle-derived authorities,
/// it is owned by the agent with the creator as owner-user pointer, its key/archived are NULL,
/// and both clocks are the request clock.
fn seal_agent_home_workspace(
    activated: &ActivatedAgent,
    row: RawHomeWorkspaceRow,
    now_ms: i64,
) -> Result<WorkspaceId, MaterializationFault> {
    let id =
        WorkspaceId::parse(row.id).map_err(|_| MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace row carries an empty id",
        })?;
    if id != home_workspace_id_for(activated.agent_id()) {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace id is not the agent's derived home id",
        });
    }
    if row.name != activated.name().as_str() {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace name is not the agent's handle",
        });
    }
    if row.name_key.is_some() {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace carries a name key, but an agent-owned home has none",
        });
    }
    if row.description.as_deref()
        != Some(agent_home_workspace_description(activated.name().as_str()).as_str())
    {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace description is not the derived home description",
        });
    }
    if row.owner_agent_id.as_deref() != Some(activated.agent_id().as_str()) {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace is not owned by the agent",
        });
    }
    if row.owner_user_id.as_deref() != Some(activated.created_by_user_id().as_str()) {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace owner-user pointer is not the creator",
        });
    }
    if row.archived_at.is_some() {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace is archived at creation",
        });
    }
    if row.created_at != now_ms || row.updated_at != now_ms {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home workspace clocks are not the request clock",
        });
    }
    Ok(id)
}

/// Replay every column the writer touched on the home DM after-image: it is a fixed-roster
/// workspace channel in the agent's home, carrying the canonical onboarding title and folded
/// key, `idle` and `private`, un-spawned and un-closed, on the request clock.
fn seal_onboarding_dm(
    home: &WorkspaceId,
    row: RawOnboardingDmRow,
    now_ms: i64,
) -> Result<ConversationId, MaterializationFault> {
    let id =
        ConversationId::parse(row.id).map_err(|_| MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM row carries an empty id",
        })?;
    if row.workspace_id.as_deref() != Some(home.as_str()) {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM is not in the agent's home workspace",
        });
    }
    // The schema's stored spellings for a fixed-roster workspace channel; the sqlite adapter
    // writes the same tokens and this seal binds the committed row against them.
    if row.placement_kind != "workspace" {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM placement is not a workspace channel",
        });
    }
    if row.roster_policy != "fixed" {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM roster policy is not fixed",
        });
    }
    if row.title != ONBOARDING_DM_TITLE {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM title is not the onboarding title",
        });
    }
    if row.title_key.as_deref() != Some(ONBOARDING_DM_TITLE_KEY) {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM title key is not the onboarding key",
        });
    }
    if row.visibility != "private" {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM visibility is not private",
        });
    }
    if row.spawned_by_agent_id.is_some() {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM was spawned by an agent",
        });
    }
    if row.created_at != now_ms || row.updated_at != now_ms {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm home DM clocks are not the request clock",
        });
    }
    Ok(id)
}

/// Bind the three roster edges: the agent is a member of its home DM, the creator owns the
/// home workspace, and the creator is a member of the home DM. Each must name the agent /
/// creator the sealed activation proved, the home / DM this graph just wrote, and join on the
/// request clock.
fn seal_conversation_roster(
    activated: &ActivatedAgent,
    home: &WorkspaceId,
    dm: &ConversationId,
    agent_on_dm: &RawConversationAgentEdge,
    creator_workspace_owner: &RawWorkspaceOwnerEdge,
    creator_on_dm: &RawConversationUserEdge,
    now_ms: i64,
) -> Result<(), MaterializationFault> {
    let agent = activated.agent_id().as_str();
    let creator = activated.created_by_user_id().as_str();
    if agent_on_dm.conversation_id != dm.as_str()
        || agent_on_dm.agent_id != agent
        || agent_on_dm.joined_at != now_ms
    {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm DM agent membership does not bind the agent to its home DM",
        });
    }
    if creator_workspace_owner.workspace_id != home.as_str()
        || creator_workspace_owner.user_id != creator
        || creator_workspace_owner.role != "owner"
        || creator_workspace_owner.joined_at != now_ms
    {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm workspace owner membership does not make the creator its owner",
        });
    }
    if creator_on_dm.conversation_id != dm.as_str()
        || creator_on_dm.user_id != creator
        || creator_on_dm.joined_at != now_ms
    {
        return Err(MaterializationFault::ProvisioningInvariant {
            reason: "conversation-arm DM user membership does not bind the creator to the home DM",
        });
    }
    Ok(())
}

/// The subject/status/stamp checks every flip after-image shares: same agent as the
/// request, the expected post-image status, and the request's clock on `updated_at`.
fn seal_flip_subject(
    agent: &AgentId,
    row: &MaterializationFlipRow,
    expected: MaterializationStatus,
    now_ms: i64,
) -> Result<(), MaterializationFault> {
    if &row.agent_id != agent {
        return Err(MaterializationFault::FlipWitnessMismatch {
            reason: "flip witness names a different agent than the request",
        });
    }
    let status = MaterializationStatus::parse(&row.materialization_status).map_err(|_| {
        MaterializationFault::FlipWitnessMismatch {
            reason: "flip witness carries an unknown materialization status",
        }
    })?;
    if status != expected {
        return Err(MaterializationFault::FlipWitnessMismatch {
            reason: "flip witness status is not the transition target",
        });
    }
    if row.updated_at != now_ms {
        return Err(MaterializationFault::FlipWitnessMismatch {
            reason: "flip witness updated_at differs from the request clock",
        });
    }
    Ok(())
}

/// The raw witness the idempotency lookup returns: the row read for
/// `(created_by_user_id, idempotency_key)`, decoded from the SELECT. The creator and key
/// come from the row's OWN columns so the domain can bind them against the request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotentReservationRow {
    pub agent_id: AgentId,
    pub created_by_user_id: String,
    pub idempotency_key: Option<String>,
    pub materialization_status: String,
    pub materialization_failed_code: Option<String>,
    pub materialization_failed_at: Option<i64>,
    pub name: String,
    pub runtime: String,
    pub model: Option<String>,
    pub description: Option<String>,
    pub host_id: String,
    pub session_scope: String,
}

/// A sealed idempotency hit: an existing reservation bound to the requested creator and
/// key, carrying exactly the persisted fields the create outcome decision reads. It holds
/// no `template_choice` — that is not a database column; the use case derives it from its
/// own single constant and must not mistake it for a committed fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotentReservation {
    agent_id: AgentId,
    created_by_user_id: UserId,
    idempotency_key: IdempotencyKey,
    materialization_status: MaterializationStatus,
    materialization_failed_code: Option<String>,
    materialization_failed_at: Option<i64>,
    name: String,
    runtime: RuntimeKind,
    model: Option<String>,
    description: Option<String>,
    host_id: HostId,
    session_scope: AgentSessionScope,
}

impl IdempotentReservation {
    fn seal(
        requested_creator: &UserId,
        requested_key: &IdempotencyKey,
        row: IdempotentReservationRow,
    ) -> Result<Self, MaterializationFault> {
        let created_by_user_id = UserId::parse(row.created_by_user_id).map_err(|_| {
            MaterializationFault::LookupWitnessMismatch {
                reason: "idempotency witness carries an empty creator",
            }
        })?;
        if &created_by_user_id != requested_creator {
            return Err(MaterializationFault::LookupWitnessMismatch {
                reason: "idempotency witness names a different creator than the request",
            });
        }
        if row.idempotency_key.as_deref() != Some(requested_key.as_str()) {
            return Err(MaterializationFault::LookupWitnessMismatch {
                reason: "idempotency witness key differs from the request",
            });
        }
        let materialization_status = MaterializationStatus::parse(&row.materialization_status)
            .map_err(|_| MaterializationFault::LookupWitnessMismatch {
                reason: "idempotency witness carries an unknown materialization status",
            })?;
        let runtime = RuntimeKind::parse(&row.runtime).ok_or({
            MaterializationFault::LookupWitnessMismatch {
                reason: "idempotency witness carries an unknown runtime",
            }
        })?;
        let host_id = HostId::parse(row.host_id).map_err(|_| {
            MaterializationFault::LookupWitnessMismatch {
                reason: "idempotency witness carries an empty host",
            }
        })?;
        let session_scope = AgentSessionScope::parse(&row.session_scope).ok_or({
            MaterializationFault::LookupWitnessMismatch {
                reason: "idempotency witness carries an unknown session scope",
            }
        })?;
        // The failure columns are a closed function of the status: `failed` carries a
        // non-empty code and a stamp; every other status carries neither. A row that breaks
        // this pairing is an incoherent witness, not a decision the use case defaults over.
        match materialization_status {
            MaterializationStatus::Failed => {
                if row
                    .materialization_failed_code
                    .as_deref()
                    .is_none_or(str::is_empty)
                {
                    return Err(MaterializationFault::LookupWitnessMismatch {
                        reason: "failed idempotency witness is missing its failure code",
                    });
                }
                if row.materialization_failed_at.is_none() {
                    return Err(MaterializationFault::LookupWitnessMismatch {
                        reason: "failed idempotency witness is missing its failure timestamp",
                    });
                }
            }
            MaterializationStatus::PendingMaterialization | MaterializationStatus::Active => {
                if row.materialization_failed_code.is_some()
                    || row.materialization_failed_at.is_some()
                {
                    return Err(MaterializationFault::LookupWitnessMismatch {
                        reason: "non-failed idempotency witness carries failure columns",
                    });
                }
            }
        }
        Ok(Self {
            agent_id: row.agent_id,
            created_by_user_id,
            idempotency_key: requested_key.clone(),
            materialization_status,
            materialization_failed_code: row.materialization_failed_code,
            materialization_failed_at: row.materialization_failed_at,
            name: row.name,
            runtime,
            model: row.model,
            description: row.description,
            host_id,
            session_scope,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn created_by_user_id(&self) -> &UserId {
        &self.created_by_user_id
    }

    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    pub fn materialization_status(&self) -> MaterializationStatus {
        self.materialization_status
    }

    pub fn materialization_failed_code(&self) -> Option<&str> {
        self.materialization_failed_code.as_deref()
    }

    pub fn materialization_failed_at(&self) -> Option<i64> {
        self.materialization_failed_at
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn runtime(&self) -> RuntimeKind {
        self.runtime
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    pub fn session_scope(&self) -> AgentSessionScope {
        self.session_scope
    }
}

/// A fault in the materialization lifecycle — a driver error, or a witness that disagrees
/// with the request. Every arm is an internal invariant break the boundary surfaces as a
/// safe `500`; none is a product answer. The `reason` is a static label for the boundary
/// log, small enough to keep the error variant off `clippy::result_large_err`.
#[derive(Debug, Error)]
pub enum MaterializationFault {
    #[error(transparent)]
    Persistence(#[from] PersistenceFailure),
    #[error("reserve after-image disagreed with the request: {reason}")]
    ReserveWitnessMismatch { reason: &'static str },
    #[error("materialization flip after-image disagreed with the request: {reason}")]
    FlipWitnessMismatch { reason: &'static str },
    #[error("activation flip after-image disagreed with the request: {reason}")]
    ActivationWitnessMismatch { reason: &'static str },
    /// A provisioning-stage invariant the dependent graph depends on was broken: the sealed
    /// reservation's creator names no user row, or the arm reached a reservation of the wrong
    /// session scope. Neither is a caller mistake — the reservation is the store's own prior
    /// fact — so it surfaces as a safe `500`, never a product answer.
    #[error("agent provisioning invariant broken: {reason}")]
    ProvisioningInvariant { reason: &'static str },
    #[error("idempotency lookup witness disagreed with the request: {reason}")]
    LookupWitnessMismatch { reason: &'static str },
    /// The onboarding message's sealed after-image landed on a conversation other than the
    /// direct line the same unit of work opened. The graph is internally inconsistent — the
    /// write proved a message on a line the provisioning did not name — so it surfaces as a
    /// safe `500`, never a product answer.
    #[error("agent onboarding witness disagreed with the provisioned graph: {reason}")]
    OnboardingWitnessMismatch { reason: &'static str },
}

/// The creator-scoped idempotency lookup. A read, not a write, so it takes no writer
/// reservation; the port returns the row it read for the `(creator, key)` pair, and the
/// caller ([`find_idempotent_reservation`]) binds it against the request.
pub trait AgentMaterializationLookup: Send + Sync {
    fn find_by_idempotency(
        &self,
        creator: &UserId,
        key: &IdempotencyKey,
    ) -> impl Future<Output = Result<Option<IdempotentReservationRow>, PersistenceFailure>> + Send;
}

/// The durable materialization writes. Each is short and holds no remote IO: the reserve
/// is one INSERT; the flips are one guarded compare-and-set each; the sweep is one bulk
/// update. Each returns a DB witness the free functions in this module seal. There is no
/// activate method — see the module docs.
pub trait AgentMaterializationWrite: Send + Sync {
    /// Insert a `pending_materialization` row, returning the row it wrote or a unique
    /// constraint conflict.
    fn reserve(
        &self,
        input: &ReserveAgentInput,
        now_ms: i64,
    ) -> impl Future<Output = Result<ReserveRecord, PersistenceFailure>> + Send;

    /// `pending|failed → failed`, stamping the code and timestamp; returns the after-image
    /// or `None` when the guarded CAS changed no row.
    fn fail(
        &self,
        agent: &AgentId,
        failed_code: &str,
        failed_at: i64,
        now_ms: i64,
    ) -> impl Future<Output = Result<Option<MaterializationFlipRow>, PersistenceFailure>> + Send;

    /// `failed → pending`, clearing the failure columns; returns the after-image or `None`
    /// when the guarded CAS changed no row.
    fn reopen_failed(
        &self,
        agent: &AgentId,
        now_ms: i64,
    ) -> impl Future<Output = Result<Option<MaterializationFlipRow>, PersistenceFailure>> + Send;

    /// Crash recovery: fail every row still `pending_materialization` and older than the
    /// cutoff, returning the ids it transitioned.
    fn mark_stale_failed(
        &self,
        cutoff_created_at: i64,
        failed_at: i64,
        now_ms: i64,
    ) -> impl Future<Output = Result<Vec<MaterializationFlipRow>, PersistenceFailure>> + Send;
}

/// What a reserve write observed at the adapter, before the domain seals it: either the
/// inserted row or the conflict the adapter's in-transaction preflight detected. The
/// inserted arm is boxed to keep the enum small against the empty conflict arm.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReserveRecord {
    Inserted(Box<ReservedAgentRow>),
    ConstraintConflict,
}

/// Look up an existing reservation for `(creator, key)` and bind it against the request.
/// A row whose own creator/key columns disagree with the request is a broken adapter — a
/// fault, never a hit an outcome would be decided from.
pub async fn find_idempotent_reservation<L: AgentMaterializationLookup>(
    port: &L,
    creator: &UserId,
    key: &IdempotencyKey,
) -> Result<Option<IdempotentReservation>, MaterializationFault> {
    match port.find_by_idempotency(creator, key).await? {
        Some(row) => Ok(Some(IdempotentReservation::seal(creator, key, row)?)),
        None => Ok(None),
    }
}

/// Reserve a pending row and seal the witness. A conflict passes through as
/// [`ReserveOutcome::ConstraintConflict`]; a persisted row that disagrees with the request
/// on subject, status, creator, key, or clock is a fault, never a sealed reservation.
pub async fn reserve_agent<W: AgentMaterializationWrite>(
    port: &W,
    input: &ReserveAgentInput,
    now_ms: i64,
) -> Result<ReserveOutcome, MaterializationFault> {
    Ok(match port.reserve(input, now_ms).await? {
        ReserveRecord::Inserted(row) => {
            ReserveOutcome::Reserved(Box::new(ReservedAgent::seal(input, now_ms, *row)?))
        }
        ReserveRecord::ConstraintConflict => ReserveOutcome::ConstraintConflict,
    })
}

/// Fail a pending-or-failed row and seal the after-image against the requested code and
/// stamp. A missing after-image is [`FailOutcome::Lost`] (the caller re-reads); an
/// after-image that disagrees is a fault.
pub async fn fail_materialization<W: AgentMaterializationWrite>(
    port: &W,
    agent: &AgentId,
    failed_code: &str,
    failed_at: i64,
    now_ms: i64,
) -> Result<FailOutcome, MaterializationFault> {
    Ok(
        match port.fail(agent, failed_code, failed_at, now_ms).await? {
            Some(row) => FailOutcome::Failed(FailedMaterialization::seal(
                agent,
                failed_code,
                failed_at,
                now_ms,
                row,
            )?),
            None => FailOutcome::Lost,
        },
    )
}

/// Reopen a failed row for retry and seal the after-image. A missing after-image is
/// [`ReopenOutcome::Lost`] (the caller re-reads); an after-image that disagrees is a fault.
pub async fn reopen_failed_materialization<W: AgentMaterializationWrite>(
    port: &W,
    agent: &AgentId,
    now_ms: i64,
) -> Result<ReopenOutcome, MaterializationFault> {
    Ok(match port.reopen_failed(agent, now_ms).await? {
        Some(row) => ReopenOutcome::Reopened(ReopenedReservation::seal(agent, now_ms, row)?),
        None => ReopenOutcome::Lost,
    })
}

/// Sweep the rows a crash left pending and seal each after-image as `failed` with the
/// canonical code and stamp, returning the ids it transitioned. A swept row whose
/// after-image is not the requested failure is a fault, not a silently dropped id.
pub async fn mark_stale_materializations_failed<W: AgentMaterializationWrite>(
    port: &W,
    cutoff_created_at: i64,
    failed_at: i64,
    now_ms: i64,
) -> Result<Vec<AgentId>, MaterializationFault> {
    let rows = port
        .mark_stale_failed(cutoff_created_at, failed_at, now_ms)
        .await?;
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        let agent = row.agent_id.clone();
        FailedMaterialization::seal(
            &agent,
            STALE_MATERIALIZATION_FAILED_CODE,
            failed_at,
            now_ms,
            row,
        )?;
        ids.push(agent);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_messaging::messaging::agent_onboarding::OnboardingMetadataDigest;

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).expect("agent id")
    }

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }

    fn key() -> IdempotencyKey {
        IdempotencyKey::parse("idem-1").expect("idempotency key")
    }

    fn input() -> ReserveAgentInput {
        ReserveAgentInput {
            agent_id: agent("a1"),
            name: ParticipantHandle::parse("a1").unwrap(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt".to_string()),
            reasoning_effort: Some("high".to_string()),
            fast_mode: Some(true),
            host_id: host("h1"),
            description: Some("d".to_string()),
            session_scope: AgentSessionScope::Conversation,
            created_by_user_id: user("u1"),
            idempotency_key: key(),
        }
    }

    fn faithful_reserved_row(now_ms: i64) -> ReservedAgentRow {
        ReservedAgentRow {
            agent_id: agent("a1"),
            name: "a1".to_string(),
            display_name: "a1".to_string(),
            runtime: "codex".to_string(),
            model: Some("gpt".to_string()),
            reasoning_effort: Some("high".to_string()),
            fast_mode: Some(1),
            host_id: "h1".to_string(),
            description: Some("d".to_string()),
            session_scope: "conversation".to_string(),
            created_by_user_id: "u1".to_string(),
            materialization_status: "pending_materialization".to_string(),
            idempotency_key: Some("idem-1".to_string()),
            created_at: now_ms,
            updated_at: now_ms,
        }
    }

    #[derive(Default)]
    struct ScriptedWrite {
        reserve: Option<ReserveRecord>,
        fail: Option<Option<MaterializationFlipRow>>,
        reopen: Option<Option<MaterializationFlipRow>>,
        stale: Vec<MaterializationFlipRow>,
    }

    impl AgentMaterializationWrite for ScriptedWrite {
        async fn reserve(
            &self,
            _input: &ReserveAgentInput,
            _now_ms: i64,
        ) -> Result<ReserveRecord, PersistenceFailure> {
            Ok(self.reserve.clone().expect("reserve scripted"))
        }

        async fn fail(
            &self,
            _agent: &AgentId,
            _failed_code: &str,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            Ok(self.fail.clone().expect("fail scripted"))
        }

        async fn reopen_failed(
            &self,
            _agent: &AgentId,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            Ok(self.reopen.clone().expect("reopen scripted"))
        }

        async fn mark_stale_failed(
            &self,
            _cutoff_created_at: i64,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Vec<MaterializationFlipRow>, PersistenceFailure> {
            Ok(self.stale.clone())
        }
    }

    struct ScriptedLookup(Option<IdempotentReservationRow>);

    impl AgentMaterializationLookup for ScriptedLookup {
        async fn find_by_idempotency(
            &self,
            _creator: &UserId,
            _key: &IdempotencyKey,
        ) -> Result<Option<IdempotentReservationRow>, PersistenceFailure> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn reserve_seals_a_faithful_pending_row() {
        let port = ScriptedWrite {
            reserve: Some(ReserveRecord::Inserted(Box::new(faithful_reserved_row(7)))),
            ..Default::default()
        };
        let ReserveOutcome::Reserved(reserved) = reserve_agent(&port, &input(), 7).await.unwrap()
        else {
            panic!("expected a reserved agent");
        };
        assert_eq!(reserved.agent_id(), &agent("a1"));
        assert_eq!(reserved.name(), "a1");
        assert_eq!(reserved.display_name(), "a1");
        assert_eq!(reserved.runtime(), RuntimeKind::Codex);
        assert_eq!(reserved.model(), Some("gpt"));
        assert_eq!(reserved.reasoning_effort(), Some("high"));
        assert_eq!(reserved.fast_mode(), Some(true));
        assert_eq!(reserved.host_id(), &host("h1"));
        assert_eq!(reserved.description(), Some("d"));
        assert_eq!(reserved.session_scope(), AgentSessionScope::Conversation);
        assert_eq!(reserved.created_by_user_id(), &user("u1"));
        assert_eq!(reserved.idempotency_key(), &key());
        assert_eq!(reserved.created_at(), 7);
        assert_eq!(reserved.updated_at(), 7);
    }

    #[tokio::test]
    async fn reserve_passes_through_a_constraint_conflict() {
        let port = ScriptedWrite {
            reserve: Some(ReserveRecord::ConstraintConflict),
            ..Default::default()
        };
        assert_eq!(
            reserve_agent(&port, &input(), 7).await.unwrap(),
            ReserveOutcome::ConstraintConflict
        );
    }

    #[tokio::test]
    async fn reserve_rejects_a_witness_that_disagrees() {
        // Every load-bearing column the reserve wrote must replay against the request; a
        // liar-writer that drifts on any one of them is a fault, not a sealed reservation.
        let mut wrong_agent = faithful_reserved_row(7);
        wrong_agent.agent_id = agent("other");

        let mut not_pending = faithful_reserved_row(7);
        not_pending.materialization_status = "active".to_string();

        let mut unknown_status = faithful_reserved_row(7);
        unknown_status.materialization_status = "smuggled".to_string();

        let mut wrong_name = faithful_reserved_row(7);
        wrong_name.name = "other".to_string();

        let mut unmirrored_display = faithful_reserved_row(7);
        unmirrored_display.display_name = "Other".to_string();

        let mut unknown_runtime = faithful_reserved_row(7);
        unknown_runtime.runtime = "smuggled".to_string();

        let mut wrong_runtime = faithful_reserved_row(7);
        wrong_runtime.runtime = "claude".to_string();

        let mut wrong_model = faithful_reserved_row(7);
        wrong_model.model = Some("other".to_string());

        let mut wrong_reasoning = faithful_reserved_row(7);
        wrong_reasoning.reasoning_effort = Some("low".to_string());

        let mut corrupt_fast_mode = faithful_reserved_row(7);
        corrupt_fast_mode.fast_mode = Some(2);

        let mut wrong_fast_mode = faithful_reserved_row(7);
        wrong_fast_mode.fast_mode = Some(0);

        let mut empty_host = faithful_reserved_row(7);
        empty_host.host_id = String::new();

        let mut wrong_host = faithful_reserved_row(7);
        wrong_host.host_id = "h2".to_string();

        let mut wrong_description = faithful_reserved_row(7);
        wrong_description.description = None;

        let mut unknown_scope = faithful_reserved_row(7);
        unknown_scope.session_scope = "smuggled".to_string();

        let mut wrong_scope = faithful_reserved_row(7);
        wrong_scope.session_scope = "agent".to_string();

        let mut empty_creator = faithful_reserved_row(7);
        empty_creator.created_by_user_id = String::new();

        let mut wrong_creator = faithful_reserved_row(7);
        wrong_creator.created_by_user_id = "u2".to_string();

        let mut wrong_key = faithful_reserved_row(7);
        wrong_key.idempotency_key = Some("other".to_string());

        let mut missing_key = faithful_reserved_row(7);
        missing_key.idempotency_key = None;

        let mut wrong_created_at = faithful_reserved_row(7);
        wrong_created_at.created_at = 8;

        let mut wrong_updated_at = faithful_reserved_row(7);
        wrong_updated_at.updated_at = 8;

        for row in [
            wrong_agent,
            not_pending,
            unknown_status,
            wrong_name,
            unmirrored_display,
            unknown_runtime,
            wrong_runtime,
            wrong_model,
            wrong_reasoning,
            corrupt_fast_mode,
            wrong_fast_mode,
            empty_host,
            wrong_host,
            wrong_description,
            unknown_scope,
            wrong_scope,
            empty_creator,
            wrong_creator,
            wrong_key,
            missing_key,
            wrong_created_at,
            wrong_updated_at,
        ] {
            let port = ScriptedWrite {
                reserve: Some(ReserveRecord::Inserted(Box::new(row))),
                ..Default::default()
            };
            assert!(matches!(
                reserve_agent(&port, &input(), 7).await.unwrap_err(),
                MaterializationFault::ReserveWitnessMismatch { .. }
            ));
        }
    }

    #[test]
    fn idempotency_key_admits_only_the_route_envelope() {
        assert!(IdempotencyKey::parse("idem-1").is_ok());
        assert!(IdempotencyKey::parse("A_z0-9").is_ok());
        assert!(IdempotencyKey::parse("a".repeat(64)).is_ok());
        // Empty, over-length, illegal punctuation, whitespace, and multi-byte are all out
        // of the `/^[A-Za-z0-9_-]{1,64}$/` envelope.
        assert!(IdempotencyKey::parse("").is_err());
        assert!(IdempotencyKey::parse("a".repeat(65)).is_err());
        assert!(IdempotencyKey::parse("has space").is_err());
        assert!(IdempotencyKey::parse("dots.not.ok").is_err());
        assert!(IdempotencyKey::parse("café").is_err());
    }

    #[tokio::test]
    async fn fail_seals_the_after_image() {
        let port = ScriptedWrite {
            fail: Some(Some(MaterializationFlipRow {
                agent_id: agent("a1"),
                materialization_status: "failed".to_string(),
                materialization_failed_code: Some("boom".to_string()),
                materialization_failed_at: Some(11),
                updated_at: 12,
            })),
            ..Default::default()
        };
        let FailOutcome::Failed(failed) = fail_materialization(&port, &agent("a1"), "boom", 11, 12)
            .await
            .unwrap()
        else {
            panic!("expected a failed transition");
        };
        assert_eq!(failed.agent_id(), &agent("a1"));
        assert_eq!(failed.failed_code(), "boom");
        assert_eq!(failed.failed_at(), 11);
        assert_eq!(failed.updated_at(), 12);
    }

    #[tokio::test]
    async fn fail_passes_through_a_lost_guard() {
        let port = ScriptedWrite {
            fail: Some(None),
            ..Default::default()
        };
        assert_eq!(
            fail_materialization(&port, &agent("a1"), "boom", 11, 12)
                .await
                .unwrap(),
            FailOutcome::Lost
        );
    }

    #[tokio::test]
    async fn fail_rejects_a_disagreeing_after_image() {
        let base = MaterializationFlipRow {
            agent_id: agent("a1"),
            materialization_status: "failed".to_string(),
            materialization_failed_code: Some("boom".to_string()),
            materialization_failed_at: Some(11),
            updated_at: 12,
        };

        let mut wrong_agent = base.clone();
        wrong_agent.agent_id = agent("other");

        let mut not_failed = base.clone();
        not_failed.materialization_status = "pending_materialization".to_string();

        let mut wrong_code = base.clone();
        wrong_code.materialization_failed_code = Some("other".to_string());

        let mut wrong_failed_at = base.clone();
        wrong_failed_at.materialization_failed_at = Some(99);

        let mut wrong_stamp = base.clone();
        wrong_stamp.updated_at = 13;

        for row in [
            wrong_agent,
            not_failed,
            wrong_code,
            wrong_failed_at,
            wrong_stamp,
        ] {
            let port = ScriptedWrite {
                fail: Some(Some(row)),
                ..Default::default()
            };
            assert!(matches!(
                fail_materialization(&port, &agent("a1"), "boom", 11, 12)
                    .await
                    .unwrap_err(),
                MaterializationFault::FlipWitnessMismatch { .. }
            ));
        }
    }

    #[tokio::test]
    async fn reopen_seals_a_cleared_pending_row() {
        let port = ScriptedWrite {
            reopen: Some(Some(MaterializationFlipRow {
                agent_id: agent("a1"),
                materialization_status: "pending_materialization".to_string(),
                materialization_failed_code: None,
                materialization_failed_at: None,
                updated_at: 20,
            })),
            ..Default::default()
        };
        let ReopenOutcome::Reopened(reopened) =
            reopen_failed_materialization(&port, &agent("a1"), 20)
                .await
                .unwrap()
        else {
            panic!("expected a reopened reservation");
        };
        assert_eq!(reopened.agent_id(), &agent("a1"));
        assert_eq!(reopened.updated_at(), 20);
    }

    #[tokio::test]
    async fn reopen_rejects_an_uncleared_failure() {
        for (code, stamp) in [(Some("stale"), None), (None, Some(9))] {
            let port = ScriptedWrite {
                reopen: Some(Some(MaterializationFlipRow {
                    agent_id: agent("a1"),
                    materialization_status: "pending_materialization".to_string(),
                    materialization_failed_code: code.map(str::to_owned),
                    materialization_failed_at: stamp,
                    updated_at: 20,
                })),
                ..Default::default()
            };
            assert!(matches!(
                reopen_failed_materialization(&port, &agent("a1"), 20)
                    .await
                    .unwrap_err(),
                MaterializationFault::FlipWitnessMismatch { .. }
            ));
        }
    }

    #[tokio::test]
    async fn reopen_passes_through_a_lost_guard() {
        let port = ScriptedWrite {
            reopen: Some(None),
            ..Default::default()
        };
        assert_eq!(
            reopen_failed_materialization(&port, &agent("a1"), 20)
                .await
                .unwrap(),
            ReopenOutcome::Lost
        );
    }

    #[tokio::test]
    async fn stale_sweep_returns_each_transitioned_id() {
        let port = ScriptedWrite {
            stale: vec![
                MaterializationFlipRow {
                    agent_id: agent("a1"),
                    materialization_status: "failed".to_string(),
                    materialization_failed_code: Some(
                        "backend_crash_during_materialize".to_string(),
                    ),
                    materialization_failed_at: Some(30),
                    updated_at: 31,
                },
                MaterializationFlipRow {
                    agent_id: agent("a2"),
                    materialization_status: "failed".to_string(),
                    materialization_failed_code: Some(
                        "backend_crash_during_materialize".to_string(),
                    ),
                    materialization_failed_at: Some(30),
                    updated_at: 31,
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            mark_stale_materializations_failed(&port, 25, 30, 31)
                .await
                .unwrap(),
            vec![agent("a1"), agent("a2")]
        );
    }

    #[tokio::test]
    async fn stale_sweep_faults_on_a_non_canonical_after_image() {
        let port = ScriptedWrite {
            stale: vec![MaterializationFlipRow {
                agent_id: agent("a1"),
                materialization_status: "failed".to_string(),
                materialization_failed_code: Some("not-canonical".to_string()),
                materialization_failed_at: Some(30),
                updated_at: 31,
            }],
            ..Default::default()
        };
        assert!(matches!(
            mark_stale_materializations_failed(&port, 25, 30, 31)
                .await
                .unwrap_err(),
            MaterializationFault::FlipWitnessMismatch { .. }
        ));
    }

    fn faithful_activation_row(now_ms: i64) -> ActivationFlipRow {
        ActivationFlipRow {
            agent_id: agent("a1"),
            name: "a1".to_string(),
            materialization_status: "active".to_string(),
            session_scope: "agent".to_string(),
            created_by_user_id: "u1".to_string(),
            created_at: 10,
            updated_at: now_ms,
        }
    }

    #[test]
    fn activation_seals_a_bound_active_row() {
        let sealed = ActivatedAgent::seal(&agent("a1"), 7, faithful_activation_row(7)).unwrap();
        assert_eq!(sealed.agent_id(), &agent("a1"));
        assert_eq!(sealed.name(), &ParticipantHandle::parse("a1").unwrap());
        assert_eq!(sealed.session_scope(), AgentSessionScope::Agent);
        assert_eq!(sealed.created_by_user_id(), &user("u1"));
        assert_eq!(sealed.updated_at(), 7);
        // The row's own created_at (10) rides the seal independent of the activation clock (7),
        // so the dependent direct line can be opened on the row's durable timestamp.
        assert_eq!(sealed.created_at(), 10);
    }

    #[test]
    fn activation_rejects_a_witness_that_disagrees() {
        // The activation is the ONLY reader that binds the graph's subjects; a flip
        // after-image that drifts on the agent, status, handle, scope, creator, or clock is
        // a fault, never a sealed activation the dependent graph would build on.
        let mut wrong_agent = faithful_activation_row(7);
        wrong_agent.agent_id = agent("other");

        let mut not_active = faithful_activation_row(7);
        not_active.materialization_status = "pending_materialization".to_string();

        let mut unknown_status = faithful_activation_row(7);
        unknown_status.materialization_status = "smuggled".to_string();

        let mut unusable_handle = faithful_activation_row(7);
        unusable_handle.name = String::new();

        let mut unknown_scope = faithful_activation_row(7);
        unknown_scope.session_scope = "smuggled".to_string();

        let mut empty_creator = faithful_activation_row(7);
        empty_creator.created_by_user_id = String::new();

        let mut wrong_stamp = faithful_activation_row(7);
        wrong_stamp.updated_at = 8;

        for row in [
            wrong_agent,
            not_active,
            unknown_status,
            unusable_handle,
            unknown_scope,
            empty_creator,
            wrong_stamp,
        ] {
            assert!(matches!(
                ActivatedAgent::seal(&agent("a1"), 7, row).unwrap_err(),
                MaterializationFault::ActivationWitnessMismatch { .. }
            ));
        }
    }

    fn onboarding_written_on(
        line: &ConversationId,
        id: &str,
    ) -> crate::conversation_messaging::messaging::agent_onboarding::OnboardingWritten {
        use crate::conversation_messaging::messaging::agent_onboarding::{
            OnboardingSystemMessage, OnboardingWriteWitness,
        };
        let intent = OnboardingSystemMessage::agent_direct_line(
            line.clone(),
            crate::conversation_messaging::messaging::MessageId::parse(id).unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("a1").unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("u1").unwrap(),
            7,
        );
        intent
            .seal_written(OnboardingWriteWitness {
                id: id.to_string(),
                conversation: line.as_str().to_string(),
                content: intent.content().to_string(),
                created_at_ms: 7,
                from_kind: "system".to_string(),
                from_agent_id: None,
                from_user_id: None,
                author_id: None,
                author_name: None,
                author_display_name: None,
                thread_root_id: None,
                cc_source_message_id: None,
                metadata: OnboardingMetadataDigest {
                    subtype: Some("onboarding".to_string()),
                    has_extra_keys: false,
                },
                seq: 1,
                head_latest_seq: 1,
            })
            .unwrap()
    }

    #[test]
    fn provisioned_seal_rejects_an_onboarding_message_on_a_foreign_line() {
        let activated = ActivatedAgent::seal(&agent("a1"), 7, faithful_activation_row(7)).unwrap();
        // The message was sealed as written on `pair-1`, but the pair the provisioning opened is
        // `pair-2`: the graph is internally inconsistent and must not mint a verdict.
        let onboarding = onboarding_written_on(&ConversationId::parse("pair-1").unwrap(), "msg-1");
        assert!(matches!(
            AgentArmProvisioned::seal(
                &activated,
                ConversationId::parse("pair-2").unwrap(),
                true,
                onboarding,
            )
            .unwrap_err(),
            MaterializationFault::OnboardingWitnessMismatch { .. }
        ));
    }

    fn faithful_lookup_row() -> IdempotentReservationRow {
        IdempotentReservationRow {
            agent_id: agent("a1"),
            created_by_user_id: "u1".to_string(),
            idempotency_key: Some("idem-1".to_string()),
            materialization_status: "active".to_string(),
            materialization_failed_code: None,
            materialization_failed_at: None,
            name: "a1".to_string(),
            runtime: "codex".to_string(),
            model: Some("gpt".to_string()),
            description: Some("d".to_string()),
            host_id: "h1".to_string(),
            session_scope: "conversation".to_string(),
        }
    }

    #[tokio::test]
    async fn lookup_seals_a_bound_hit() {
        let port = ScriptedLookup(Some(faithful_lookup_row()));
        let hit = find_idempotent_reservation(&port, &user("u1"), &key())
            .await
            .unwrap()
            .expect("a hit");
        assert_eq!(hit.agent_id(), &agent("a1"));
        assert_eq!(hit.created_by_user_id(), &user("u1"));
        assert_eq!(hit.materialization_status(), MaterializationStatus::Active);
        assert_eq!(hit.runtime(), RuntimeKind::Codex);
        assert_eq!(hit.session_scope(), AgentSessionScope::Conversation);
        assert_eq!(hit.host_id(), &host("h1"));
    }

    #[tokio::test]
    async fn lookup_returns_none_for_a_miss() {
        let port = ScriptedLookup(None);
        assert!(
            find_idempotent_reservation(&port, &user("u1"), &key())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn lookup_rejects_a_witness_bound_to_another_request() {
        let mut wrong_creator = faithful_lookup_row();
        wrong_creator.created_by_user_id = "u2".to_string();

        let mut wrong_key = faithful_lookup_row();
        wrong_key.idempotency_key = Some("other".to_string());

        let mut unknown_runtime = faithful_lookup_row();
        unknown_runtime.runtime = "smuggled".to_string();

        let mut empty_host = faithful_lookup_row();
        empty_host.host_id = String::new();

        let mut unknown_scope = faithful_lookup_row();
        unknown_scope.session_scope = "smuggled".to_string();

        for row in [
            wrong_creator,
            wrong_key,
            unknown_runtime,
            empty_host,
            unknown_scope,
        ] {
            let port = ScriptedLookup(Some(row));
            assert!(matches!(
                find_idempotent_reservation(&port, &user("u1"), &key())
                    .await
                    .unwrap_err(),
                MaterializationFault::LookupWitnessMismatch { .. }
            ));
        }
    }

    // ---- Provisioning authority boundary (P0-1): the core use cases are the SOLE minters. ----
    // The port can only hand back RAW after-images (or a raw failure); it cannot construct a
    // sealed verdict, because every seal is `pub(crate)` and no port method returns one. These
    // tests drive the use cases through a fake port that returns deliberately forged raw rows
    // and prove the seal — not the adapter — is what rejects them.

    #[derive(Debug, thiserror::Error)]
    #[error("scripted dependent-write failure")]
    struct ScriptedProvisionFault;

    #[derive(Default)]
    struct ScriptedProvision {
        provision: Option<Result<RawProvisionOutcome, ()>>,
        fail: Option<Option<MaterializationFlipRow>>,
    }

    impl AgentArmProvisioningPort for ScriptedProvision {
        async fn provision_and_activate(
            &self,
            _agent: &AgentId,
            _candidate_direct_line: ConversationId,
            _candidate_dm_channel: ConversationId,
            _onboarding_message_id: MessageId,
            _now_ms: i64,
        ) -> Result<RawProvisionOutcome, PersistenceFailure> {
            match self.provision.clone().expect("provision scripted") {
                Ok(raw) => Ok(raw),
                Err(()) => Err(PersistenceFailure::from_source(ScriptedProvisionFault)),
            }
        }

        async fn fail_after_rolled_back_provision(
            &self,
            _agent: &AgentId,
            _failed_at: i64,
            _now_ms: i64,
        ) -> Result<Option<MaterializationFlipRow>, PersistenceFailure> {
            Ok(self.fail.clone().expect("compensating fail scripted"))
        }
    }

    fn active_flip(scope: &str, created_at: i64, now_ms: i64) -> ActivationFlipRow {
        ActivationFlipRow {
            agent_id: agent("a1"),
            name: "a1".to_string(),
            materialization_status: "active".to_string(),
            session_scope: scope.to_string(),
            created_by_user_id: "u1".to_string(),
            created_at,
            updated_at: now_ms,
        }
    }

    /// A faithful onboarding after-image for `intent`: the head insert, system-kind, with none
    /// of the participant / author / thread / cc columns and exactly the onboarding subtype.
    fn faithful_onboarding_witness(intent: &OnboardingSystemMessage) -> OnboardingWriteWitness {
        OnboardingWriteWitness {
            id: intent.id().as_str().to_string(),
            conversation: intent.conversation().as_str().to_string(),
            content: intent.content().to_string(),
            created_at_ms: intent.created_at_ms(),
            from_kind: "system".to_string(),
            from_agent_id: None,
            from_user_id: None,
            author_id: None,
            author_name: None,
            author_display_name: None,
            thread_root_id: None,
            cc_source_message_id: None,
            metadata: OnboardingMetadataDigest {
                subtype: Some("onboarding".to_string()),
                has_extra_keys: false,
            },
            seq: 1,
            head_latest_seq: 1,
        }
    }

    fn faithful_agent_graph(now_ms: i64) -> RawAgentArmGraph {
        let direct_line = ConversationId::parse("pair-1").unwrap();
        let intent = OnboardingSystemMessage::agent_direct_line(
            direct_line.clone(),
            MessageId::parse("msg-1").unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("a1").unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("u1").unwrap(),
            now_ms,
        );
        RawAgentArmGraph {
            flip: active_flip("agent", 10, now_ms),
            creator: RawCreatorIdentity {
                source_user_id: "u1".to_string(),
                name: "u1".to_string(),
            },
            direct_line: direct_line.clone(),
            direct_line_participants: DirectPairParticipants::between(
                PrincipalIdentity::Human(user("u1")),
                PrincipalIdentity::Agent(agent("a1")),
            )
            .unwrap(),
            pair_created: true,
            onboarding: faithful_onboarding_witness(&intent),
        }
    }

    fn faithful_conversation_graph(now_ms: i64) -> RawConversationArmGraph {
        let home = home_workspace_id_for(&agent("a1"));
        let dm = ConversationId::parse("dm-1").unwrap();
        let intent = OnboardingSystemMessage::conversation_dm(
            dm.clone(),
            MessageId::parse("msg-1").unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("a1").unwrap(),
            now_ms,
        );
        RawConversationArmGraph {
            flip: active_flip("conversation", 10, now_ms),
            home_workspace: RawHomeWorkspaceRow {
                id: home.as_str().to_string(),
                name: "a1".to_string(),
                name_key: None,
                description: Some(agent_home_workspace_description("a1")),
                owner_agent_id: Some("a1".to_string()),
                owner_user_id: Some("u1".to_string()),
                archived_at: None,
                created_at: now_ms,
                updated_at: now_ms,
            },
            dm_channel: RawOnboardingDmRow {
                id: "dm-1".to_string(),
                workspace_id: Some(home.as_str().to_string()),
                placement_kind: "workspace".to_string(),
                roster_policy: "fixed".to_string(),
                title: ONBOARDING_DM_TITLE.to_string(),
                title_key: Some(ONBOARDING_DM_TITLE_KEY.to_string()),
                visibility: "private".to_string(),
                spawned_by_agent_id: None,
                created_at: now_ms,
                updated_at: now_ms,
            },
            agent_on_dm: RawConversationAgentEdge {
                conversation_id: "dm-1".to_string(),
                agent_id: "a1".to_string(),
                joined_at: now_ms,
            },
            creator_workspace_owner: RawWorkspaceOwnerEdge {
                workspace_id: home.as_str().to_string(),
                user_id: "u1".to_string(),
                role: "owner".to_string(),
                joined_at: now_ms,
            },
            creator_on_dm: RawConversationUserEdge {
                conversation_id: "dm-1".to_string(),
                user_id: "u1".to_string(),
                joined_at: now_ms,
            },
            onboarding: faithful_onboarding_witness(&intent),
        }
    }

    /// Drive the single use case through the fake port with a scripted outcome. The candidates
    /// are the faithful graphs' ids, so a faithful outcome replays clean.
    async fn provision(
        outcome: RawProvisionOutcome,
    ) -> Result<ProvisionedActivation, MaterializationFault> {
        let port = ScriptedProvision {
            provision: Some(Ok(outcome)),
            ..Default::default()
        };
        provision_and_activate(
            &port,
            &agent("a1"),
            ConversationId::parse("pair-1").unwrap(),
            ConversationId::parse("dm-1").unwrap(),
            MessageId::parse("msg-1").unwrap(),
            99,
        )
        .await
    }

    #[tokio::test]
    async fn agent_arm_seals_from_faithful_raw_after_images() {
        let outcome = provision(RawProvisionOutcome::Agent(Box::new(faithful_agent_graph(
            99,
        ))))
        .await
        .unwrap();
        let ProvisionedActivation::Agent(fact) = outcome else {
            panic!("expected a provisioned agent arm");
        };
        assert_eq!(fact.agent_id(), &agent("a1"));
        assert_eq!(
            fact.direct_line(),
            &ConversationId::parse("pair-1").unwrap()
        );
        assert_eq!(
            fact.onboarding_message_id(),
            &MessageId::parse("msg-1").unwrap()
        );
        assert!(fact.pair_created());
    }

    #[tokio::test]
    async fn conversation_arm_seals_from_faithful_raw_after_images() {
        let outcome = provision(RawProvisionOutcome::Conversation(Box::new(
            faithful_conversation_graph(99),
        )))
        .await
        .unwrap();
        let ProvisionedActivation::Conversation(fact) = outcome else {
            panic!("expected a provisioned conversation arm");
        };
        assert_eq!(fact.agent_id(), &agent("a1"));
        assert_eq!(fact.home_workspace(), &home_workspace_id_for(&agent("a1")));
        assert_eq!(fact.dm_channel(), &ConversationId::parse("dm-1").unwrap());
        assert_eq!(
            fact.onboarding_message_id(),
            &MessageId::parse("msg-1").unwrap()
        );
    }

    #[tokio::test]
    async fn provision_is_lost_when_the_flip_matched_no_row() {
        assert_eq!(
            provision(RawProvisionOutcome::Lost).await.unwrap(),
            ProvisionedActivation::Lost
        );
    }

    #[tokio::test]
    async fn provision_faults_on_an_unrecognized_scope_without_stamping_the_row() {
        // The adapter rolled the transaction back with nothing written (the row stays pending)
        // and reported the scope it could not classify. The use case surfaces a fault and NEVER
        // drives the compensating fail — `fail` is unscripted, so a stray call would panic.
        assert!(matches!(
            provision(RawProvisionOutcome::UnrecognizedScope)
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn agent_arm_rejects_a_reservation_that_is_not_agent_scoped() {
        // A liar adapter reports the agent arm for a conversation-scoped row; the core re-checks
        // the sealed scope selects THIS arm.
        let mut graph = faithful_agent_graph(99);
        graph.flip.session_scope = "conversation".to_string();
        assert!(matches!(
            provision(RawProvisionOutcome::Agent(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn agent_arm_rejects_a_creator_identity_from_another_user() {
        // The creator handle rides its OWN users row; the seal source-binds that row's id against
        // the activation creator BEFORE rendering the handle, so a handle read from a different
        // user than the flip proved can never reach the greeting.
        let mut graph = faithful_agent_graph(99);
        graph.creator.source_user_id = "u2".to_string();
        assert!(matches!(
            provision(RawProvisionOutcome::Agent(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn agent_arm_rejects_a_freshly_minted_line_that_is_not_the_candidate() {
        // A freshly minted line MUST be the requested candidate. Here the open announced a mint
        // (`pair_created`) but returned some OTHER id, so the candidate replay refuses it.
        let mut graph = faithful_agent_graph(99);
        graph.direct_line = ConversationId::parse("pair-elsewhere").unwrap();
        assert!(matches!(
            provision(RawProvisionOutcome::Agent(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn agent_arm_allows_a_resolved_existing_line_that_differs_from_the_candidate() {
        // An existing line the get-or-create resolved (no announced mint) may legitimately differ
        // from the candidate — a retried activation resolves the durable pair the first attempt
        // opened. The seal accepts it precisely because `pair_created` is unset.
        let mut graph = faithful_agent_graph(99);
        graph.pair_created = false;
        graph.direct_line = ConversationId::parse("pair-existing").unwrap();
        let intent = OnboardingSystemMessage::agent_direct_line(
            ConversationId::parse("pair-existing").unwrap(),
            MessageId::parse("msg-1").unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("a1").unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("u1").unwrap(),
            99,
        );
        graph.onboarding = faithful_onboarding_witness(&intent);
        let outcome = provision(RawProvisionOutcome::Agent(Box::new(graph)))
            .await
            .unwrap();
        let ProvisionedActivation::Agent(fact) = outcome else {
            panic!("expected a provisioned agent arm");
        };
        assert_eq!(
            fact.direct_line(),
            &ConversationId::parse("pair-existing").unwrap()
        );
        assert!(!fact.pair_created());
    }

    #[tokio::test]
    async fn agent_arm_rejects_a_direct_line_not_between_the_creator_and_the_agent() {
        let mut graph = faithful_agent_graph(99);
        // A line the adapter opened between the agent and SOME OTHER human — not the creator the
        // flip proved. The use case re-derives the expected pair and refuses the forgery.
        graph.direct_line_participants = DirectPairParticipants::between(
            PrincipalIdentity::Human(user("u2")),
            PrincipalIdentity::Agent(agent("a1")),
        )
        .unwrap();
        assert!(matches!(
            provision(RawProvisionOutcome::Agent(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn agent_arm_rejects_an_onboarding_message_that_did_not_land_faithfully() {
        let mut graph = faithful_agent_graph(99);
        // The adapter's word that the onboarding message is a clean system post is not trusted:
        // a stray from-participant makes the seal reject it.
        graph.onboarding.from_agent_id = Some("a1".to_string());
        assert!(matches!(
            provision(RawProvisionOutcome::Agent(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::OnboardingWitnessMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn conversation_arm_rejects_a_reservation_that_is_not_conversation_scoped() {
        let mut graph = faithful_conversation_graph(99);
        graph.flip.session_scope = "agent".to_string();
        assert!(matches!(
            provision(RawProvisionOutcome::Conversation(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn conversation_arm_rejects_a_home_workspace_not_owned_by_the_agent() {
        let mut graph = faithful_conversation_graph(99);
        graph.home_workspace.owner_agent_id = Some("someone-else".to_string());
        assert!(matches!(
            provision(RawProvisionOutcome::Conversation(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn conversation_arm_rejects_a_dm_without_the_onboarding_title() {
        let mut graph = faithful_conversation_graph(99);
        graph.dm_channel.title = "Not Onboarding".to_string();
        assert!(matches!(
            provision(RawProvisionOutcome::Conversation(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn conversation_arm_rejects_a_dm_that_is_not_the_candidate() {
        // The DM is a fresh INSERT, so the row the transaction wrote MUST be the requested
        // candidate. A different id — even a well-formed one, in the right workspace — is refused.
        let mut graph = faithful_conversation_graph(99);
        graph.dm_channel.id = "dm-elsewhere".to_string();
        let intent = OnboardingSystemMessage::conversation_dm(
            ConversationId::parse("dm-elsewhere").unwrap(),
            MessageId::parse("msg-1").unwrap(),
            &crate::conversation_messaging::messaging::ParticipantHandle::parse("a1").unwrap(),
            99,
        );
        graph.onboarding = faithful_onboarding_witness(&intent);
        graph.agent_on_dm.conversation_id = "dm-elsewhere".to_string();
        graph.creator_on_dm.conversation_id = "dm-elsewhere".to_string();
        assert!(matches!(
            provision(RawProvisionOutcome::Conversation(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn conversation_arm_rejects_a_creator_edge_that_is_not_an_owner() {
        let mut graph = faithful_conversation_graph(99);
        graph.creator_workspace_owner.role = "member".to_string();
        assert!(matches!(
            provision(RawProvisionOutcome::Conversation(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::ProvisioningInvariant { .. }
        ));
    }

    #[tokio::test]
    async fn conversation_arm_rejects_fixed_column_drift() {
        // Every fixed column the writer stamps is replayed: a liar port that flips any one of
        // them away from its committed constant is refused. Each case mutates exactly one column
        // off the otherwise-faithful graph.
        let mutate: Vec<fn(&mut RawConversationArmGraph)> = vec![
            |g| g.dm_channel.visibility = "public".to_string(),
            |g| g.dm_channel.spawned_by_agent_id = Some("a1".to_string()),
            |g| g.dm_channel.created_at = 1,
            |g| g.dm_channel.updated_at = 1,
            |g| g.home_workspace.name = "not-a1".to_string(),
            |g| g.home_workspace.name_key = Some("a1".to_string()),
            |g| g.home_workspace.description = Some("wrong".to_string()),
            |g| g.home_workspace.archived_at = Some(1),
            |g| g.home_workspace.created_at = 1,
            |g| g.home_workspace.updated_at = 1,
            |g| g.agent_on_dm.joined_at = 1,
            |g| g.creator_workspace_owner.joined_at = 1,
            |g| g.creator_on_dm.joined_at = 1,
        ];
        for apply in mutate {
            let mut graph = faithful_conversation_graph(99);
            apply(&mut graph);
            assert!(matches!(
                provision(RawProvisionOutcome::Conversation(Box::new(graph)))
                    .await
                    .unwrap_err(),
                MaterializationFault::ProvisioningInvariant { .. }
            ));
        }
    }

    #[tokio::test]
    async fn conversation_arm_rejects_an_onboarding_message_that_did_not_land_faithfully() {
        let mut graph = faithful_conversation_graph(99);
        graph.onboarding.thread_root_id = Some("thread-1".to_string());
        assert!(matches!(
            provision(RawProvisionOutcome::Conversation(Box::new(graph)))
                .await
                .unwrap_err(),
            MaterializationFault::OnboardingWitnessMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn rolled_back_provision_preserves_the_error_for_both_compensation_outcomes() {
        // The port faulted (its transaction rolled back). The use case drives the pending-only
        // compensation and surfaces the original dependent-write cause as a persistence fault.
        // Direction 1 — the row is still pending, so the compensating fail stamps it.
        let port = ScriptedProvision {
            provision: Some(Err(())),
            fail: Some(Some(MaterializationFlipRow {
                agent_id: agent("a1"),
                materialization_status: "failed".to_string(),
                materialization_failed_code: Some(DEPENDENT_GRAPH_FAILED_CODE.to_string()),
                materialization_failed_at: Some(99),
                updated_at: 99,
            })),
        };
        let fault = provision_and_activate(
            &port,
            &agent("a1"),
            ConversationId::parse("pair-1").unwrap(),
            ConversationId::parse("dm-1").unwrap(),
            MessageId::parse("msg-1").unwrap(),
            99,
        )
        .await
        .unwrap_err();
        assert!(matches!(fault, MaterializationFault::Persistence(_)));

        // Direction 2 — a concurrent transition already won the row, so the pending-only CAS
        // matched nothing (`None`); the compensation is a no-op and the original cause stands.
        let port = ScriptedProvision {
            provision: Some(Err(())),
            fail: Some(None),
        };
        let fault = provision_and_activate(
            &port,
            &agent("a1"),
            ConversationId::parse("pair-1").unwrap(),
            ConversationId::parse("dm-1").unwrap(),
            MessageId::parse("msg-1").unwrap(),
            99,
        )
        .await
        .unwrap_err();
        assert!(matches!(fault, MaterializationFault::Persistence(_)));
    }
}
