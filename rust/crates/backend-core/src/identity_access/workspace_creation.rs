//! The workspace-creation domain: the pre-write verdict and the created result.
//!
//! `POST /api/workspaces` linearizes its WHOLE synchronous durable-setup phase into
//! ONE non-interleavable authority boundary: under a single write
//! lock the writer re-decides the name verdict, replays each requested agent's
//! existence + ownership, and writes the workspace + owner/user members + per-agent
//! membership/capabilities + the Onboarding channel with its founding roster — a
//! racing delete/ownership-change lands wholly before (filtered/refused, zero
//! workspace) or wholly after (complete), never a mixed residue. The name and
//! agent-ownership doors are decided FIRST inside that boundary (a name clash or an
//! unowned agent refuses with zero writes); the ids are minted and the clock sampled
//! only AFTER the gates pass. Onboarding MESSAGES are the one thing that stays a
//! SEPARATE per-agent step the route interleaves with deliver/start (each sampling
//! its own append instant), so a mid-fan-out deliver failure truncates leaving only
//! the channel + membership residue.
//!
//! The domain owns the RULES and the ORDER; the persistence adapter owns the SQL and
//! the single provisioning transaction; the composition owns the post-commit
//! per-agent deliver/start effects.

use crate::conversation_messaging::messaging::MessageId;
use crate::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};
use crate::foundation::persistence::PersistenceFailure;
use crate::identity_access::workspace_name::CanonicalWorkspaceName;
use std::future::Future;

/// One member agent's onboarding post-commit effect target, captured from the
/// committed saga so the composition can deliver + start each agent's runtime.
/// The reference awaits `deliverMessage` (critical) then fires `start`
/// (best-effort); both are remote daemon IO the route hands to the aftermath.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceOnboardingTarget {
    pub conversation: ConversationId,
    pub agent: AgentId,
}

/// The committed workspace face read back after the insert so the `201` body
/// reflects the owner-user pointer stamped in the same write. Mirrors
/// `ViewerWorkspaceDto` minus the DM fields (a regular workspace has none).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatedWorkspaceFace {
    pub id: WorkspaceId,
    pub name: String,
    pub description: Option<String>,
    pub owner_user_id: Option<UserId>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A committed per-agent onboarding message, for the post-commit deliver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedOnboardingMessage {
    pub agent: AgentId,
    pub message_id: MessageId,
}

/// The outcome of the FIRST accepted agent's onboarding-message attempt, made inside
/// the provisioning transaction under a SAVEPOINT. The contract:
/// whatever happens to the first message, the workspace/members/private-channel SETUP
/// residue is PRESERVED (the setup tx still commits) and the message/stream-head are
/// never half-written — a first-message fault rolls back ONLY to the savepoint, then
/// the setup commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FirstOnboardingOutcome {
    /// No member agents were accepted — no onboarding message is seeded at all.
    NoAgents,
    /// The first agent's message committed atomically with the setup.
    Committed(CommittedOnboardingMessage),
    /// The first message's mint/append/seal FAILED; the savepoint was rolled back so
    /// the message + stream head carry NO half-write, and the setup residue committed.
    /// The route maps this to a `500` (the create did not fully succeed) WITHOUT any
    /// onboarding deliver/start effect — effect count is exactly zero.
    Failed,
}

/// The committed onboarding-channel write: the channel id + the channel DTO facts
/// read back from the COMMITTED row, so the `201` body reflects
/// the stored truth (an agent-forked/onboarding channel is `private`) rather than a
/// hard-coded literal that could drift from the insert. The per-agent messages are
/// appended separately (interleaved with deliver).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnboardingChannelCommitted {
    pub channel: ConversationId,
    pub workspace: WorkspaceId,
    /// The stored `visibility` of the committed onboarding conversation (`private`).
    pub visibility: String,
    /// The stored `title` (`Onboarding`).
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The whole synchronous durable-setup request the writer linearizes into ONE
/// non-interleavable authority boundary: the freshly-parsed
/// name, owner, validated user members, and the requested member-agent ids (raw,
/// WITH TS array multiplicity preserved — #10). The workspace/channel ids and the
/// clock are NOT carried here: the writer mints the ids and samples the clock ITSELF,
/// only AFTER its in-lock name + agent-owner product gates pass (#31/#9), so a
/// failing entropy/clock can never pre-empt a `NameTaken`/`AgentNotOwned` product
/// refusal.
pub struct WorkspaceProvisionRequest<'a> {
    pub name: &'a CanonicalWorkspaceName,
    pub description: Option<&'a str>,
    pub owner_user: UserId,
    pub member_user_ids: &'a [UserId],
    /// Requested member-agent ids, in request order and WITH multiplicity: the
    /// writer verifies each one's existence+ownership UNDER the lock, silently drops
    /// the unknown, refuses the known-unowned, and keeps the accepted set (still with
    /// multiplicity) for the caller's per-agent onboarding fan-out.
    pub requested_agents: &'a [AgentId],
}

/// The clock the provisioning writer samples ONCE, sampled INSIDE the writer only
/// after the in-lock product gates pass (#9): a failing/late clock cannot pre-empt a
/// product refusal, and the one instant stamps the whole atomic boundary.
pub trait WorkspaceProvisionClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// One accepted member agent's committed post-image, captured IN the provisioning tx
/// right after its existence+ownership replay (#28): the handle (addressable
/// `@name`) and runtime the onboarding body needs come from the SAME lock that
/// admitted the agent, never a post-commit live reread. The handle is a CLOSED typed
/// `ParticipantHandle` parsed inside the writer lock: a malformed
/// stored handle for ANY accepted member is a typed fault that rolls the whole
/// provisioning tx back BEFORE any onboarding message is appended — never a partial
/// fan-out where an early member is delivered and a later malformed member is silently
/// dropped. Multiplicity is preserved (a repeated requested id yields a repeated
/// witness) for the per-agent fan-out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedMemberAgent {
    pub agent: AgentId,
    pub handle: crate::conversation_messaging::messaging::ParticipantHandle,
    pub runtime: String,
}

/// The committed result of the linearized durable-setup boundary: the workspace
/// face, the onboarding channel facts, the minted ids the writer chose, the ACCEPTED
/// member agents WITH their committed handle/runtime post-image (#28), and — when at
/// least one agent was accepted — the FIRST agent's onboarding message ALREADY made
/// durable INSIDE the same reserved-writer transaction as the setup, so
/// there is no await point / insertable state between the workspace commit and the
/// first onboarding message being durable, matching the TS synchronous durable
/// prefix. The route drives deliver(first)→start(first) then, for each SUBSEQUENT
/// agent, append→deliver→start (deliver failure truncates the fan-out).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisionedWorkspace {
    /// Human membership post-image captured in the creation transaction, including owner.
    pub member_users: Vec<UserId>,
    pub face: CreatedWorkspaceFace,
    pub onboarding_channel: OnboardingChannelCommitted,
    /// All accepted agents, request order + multiplicity preserved (#10). The FIRST
    /// element is the one whose onboarding message `first_onboarding` already carries;
    /// the route appends the remaining `member_agents[1..]` one at a time.
    pub member_agents: Vec<AcceptedMemberAgent>,
    /// The first accepted agent's onboarding message outcome, decided in the
    /// provisioning transaction under a savepoint: `NoAgents`,
    /// `Committed(message)`, or `Failed` (savepoint rolled back, setup residue kept).
    pub first_onboarding: FirstOnboardingOutcome,
}

/// The outcome of the ONE linearized durable-setup boundary. Every refusal wrote
/// NOTHING (the whole tx rolled back): a `NameTaken` and an `AgentNotOwned` are both
/// zero-reach, and no competing writer could interleave between the name verdict,
/// the agent-owner replay, the workspace/enrollment writes, and the onboarding
/// channel — so a delete/ownership-change racing the create can only land wholly
/// before (→ filtered/refused, zero workspace) or wholly after (→ complete
/// membership) this boundary, never the mixed state #41 describes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceProvisionOutcome {
    // Boxed: the provisioned payload dwarfs the two refusal variants, so box it to
    // keep the enum small (clippy large_enum_variant).
    Provisioned(Box<ProvisionedWorkspace>),
    NameTaken,
    AgentNotOwned { agent_ids: Vec<AgentId> },
}

/// The durable workspace-create write authority — the two unit-of-work methods the
/// saga interleaves with the reused per-agent enrollment authority. Each is one
/// transaction over its own rows (the reference create is explicitly not a single
/// transaction).
pub trait WorkspaceCreationWrite: Send + Sync + 'static {
    /// Linearize the WHOLE synchronous durable-setup phase into ONE
    /// non-interleavable authority boundary: under a
    /// single write lock, in reference order, re-decide the name verdict (taken maps
    /// to `NameTaken`, zero writes); then per requested agent replay existence and
    /// `created_by_user_id == owner` (unknown is silently dropped; a known-unowned
    /// agent maps to `AgentNotOwned`, zero writes; both roll the whole tx back,
    /// honoring the #28/#11 name-before-agent order); then insert the workspace with
    /// its owner and validated user members; then enroll each accepted agent
    /// (membership plus default capability seed); then insert the Onboarding channel
    /// with its founding roster (users and agents). No competing writer can
    /// interleave, so a racing delete or ownership-change lands wholly before
    /// (filtered/refused) or wholly after (complete), never the mixed residue #41
    /// describes. The workspace + onboarding-channel ids are MINTED and the clock is
    /// SAMPLED by the writer ITSELF, only AFTER the name + agent-owner gates pass
    /// (#31/#9): a failing `entropy`/`clock` cannot pre-empt a `NameTaken`/
    /// `AgentNotOwned` product refusal. Each accepted agent's committed handle/runtime
    /// post-image is captured in the SAME lock and returned (#28) — no post-commit
    /// live reread, no missing→id/empty fallback. Accepted agents keep the TS array
    /// multiplicity (#10) for the caller's per-agent onboarding fan-out.
    ///
    /// The FIRST accepted agent's onboarding message is made durable INSIDE THIS SAME
    /// transaction: after the setup inserts and before the commit, the
    /// writer mints the message id, samples its own post-lock message clock, renders
    /// the onboarding body (recipient handle + the OTHER accepted members as peers),
    /// runs the canonical append primitive (reserve seq → 14-column system insert →
    /// stream-head bump) and seals it — so the workspace and the first onboarding
    /// message commit ATOMICALLY and there is NO await point / insertable state between
    /// the workspace being durable and its first onboarding message being durable,
    /// exactly as the TS synchronous durable prefix (the first concurrency point is the
    /// first deliver). The returned `first_onboarding` carries that committed message
    /// (`None` iff no agent was accepted). The SUBSEQUENT agents' messages stay a
    /// SEPARATE per-agent step (`append_agent_onboarding_message`) the route interleaves
    /// with deliver, so a mid-fan-out deliver failure leaves later agents' message rows
    /// non-existent (only the channel, membership, and prior agents' messages survive).
    fn provision_workspace<E, C>(
        &self,
        request: &WorkspaceProvisionRequest<'_>,
        entropy: &E,
        clock: &C,
    ) -> impl Future<Output = Result<WorkspaceProvisionOutcome, PersistenceFailure>> + Send
    where
        E: crate::conversation_messaging::messaging::environment::SendEntropy + Sync,
        C: WorkspaceProvisionClock;

    /// Append ONE SUBSEQUENT agent's onboarding system message (the first agent's is
    /// already durable inside `provision_workspace`) through the CANONICAL
    /// onboarding primitive (reserve seq → 14-column system insert → main-line
    /// stream-head bump) in its own committed transaction, so the channel's
    /// `latest_message_seq`/head advance exactly as a normal send's. Called per
    /// subsequent agent AFTER the prior agent's deliver succeeded, so a deliver failure
    /// stops the loop before this agent's row is ever written. The writer MINTS the
    /// message id and SAMPLES the `clock` ITSELF inside this UoW after the
    /// `BEGIN IMMEDIATE` lock is held and the stream facts are read:
    /// that single instant stamps both the inserted row and the sealed after-image, and
    /// the id is never a route-side pre-mint that could race the stream gates — the
    /// intent carries only the RENDERING INPUTS (recipient handle + peer
    /// list), NO id and NO timestamp, so the route performs ZERO entropy.
    fn append_agent_onboarding_message<E, C>(
        &self,
        intent: &AgentOnboardingIntent,
        entropy: &E,
        clock: &C,
    ) -> impl Future<Output = Result<CommittedOnboardingMessage, PersistenceFailure>> + Send
    where
        E: crate::conversation_messaging::messaging::environment::SendEntropy + Sync,
        C: WorkspaceProvisionClock;
}

/// One per-agent onboarding intent the writer renders + appends canonically. It
/// carries ONLY the message's RENDERING INPUTS — NEVER a message id: the
/// route does ZERO entropy, and the append writer mints the id AND samples the clock
/// itself in-lock, so no route-side pre-mint (nor a route-side entropy fault) can race
/// ahead of the writer's stream gates. The body is rendered from the recipient handle
/// + the OTHER members as peers inside the writer.
pub struct AgentOnboardingIntent {
    pub agent: AgentId,
    pub conversation: ConversationId,
    pub workspace_name: String,
    pub recipient_handle: crate::conversation_messaging::messaging::ParticipantHandle,
    pub others:
        Vec<crate::conversation_messaging::messaging::agent_onboarding::WorkspaceOnboardingPeer>,
}
