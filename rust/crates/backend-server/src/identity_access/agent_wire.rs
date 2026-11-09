//! The single crate-private full-`AgentDto` authority for the Rust backend.
//!
//! `agentRowToDto` is ONE function in the TypeScript authority. Every face that
//! returns an agent — the list and detail reads, and the MUTATING create / PATCH /
//! avatar faces that echo the committed row — projects through this ONE place, so a
//! field added for one face cannot silently miss another. #task:2226's internal
//! avatar faces consume the SAME projector rather than copying a second DTO.
//!
//! It exposes two `pub(crate)` faces and nothing wider:
//!   - [`agent_dto`] — the pure projection of one [`AgentDetailFacts`] bundle to the
//!     wire DTO (field-for-field the TS `AgentDto`, with the two derived fields
//!     `host_label` and `avatar_url` computed here).
//!   - [`reproject_committed_agent`] — the post-commit read+project face every
//!     MUTATING face shares. After its write commits, it re-reads the row by its
//!     committed `AgentId` through the ACL-free read model and projects. It is
//!     ACL-free by design — the caller already passed the write-authority gate on the
//!     pre-write read — and SOURCE-BOUND: the re-read must resolve to the SAME id,
//!     else a different row came back and the answer is a safe `500`, never a
//!     cross-agent leak. The join-derived fields (`host_label`, materialization
//!     status) ride this read, never the write fact.
//!
//! It stays `pub(crate)`, never `pub`: shared across this crate's route modules, but
//! not a public API whose later refactor would become a compatibility obligation.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::AgentId,
    foundation::name_canonicalization::is_ecmascript_trim_whitespace,
    foundation::persistence::PersistenceFailure,
    identity_access::agent_detail_read::{AgentDetailFacts, AgentDetailReadModel},
};

/// The wire projection of one agent row. Field-for-field the TypeScript `AgentDto`:
/// every optional column serializes as an explicit `null` (never skipped),
/// `idempotency_key` is deliberately absent, and the two derived fields — `host_label`
/// and `avatar_url` — are computed here. Every face that returns an agent builds this
/// same DTO; they differ only in the wrapper that carries it.
#[derive(Serialize)]
pub(crate) struct AgentDto {
    id: String,
    name: String,
    display_name: String,
    /// The closed set's own spelling, like `materialization_status`: the projection
    /// carries a decoded [`silverhand_backend_core::identity_access::agents::runtime::RuntimeKind`], so
    /// this shared wire has no way to emit a token the set does not hold.
    runtime: &'static str,
    model: Option<String>,
    reasoning_effort: Option<String>,
    fast_mode: Option<bool>,
    host_id: String,
    host_label: Option<String>,
    description: Option<String>,
    session_scope: String,
    created_by_user_id: String,
    avatar_url: Option<String>,
    materialization_status: &'static str,
    materialization_failed_code: Option<String>,
    materialization_failed_at: Option<i64>,
    deletion_started_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl AgentDto {
    /// The derived cache-busted avatar URL (or `null`). Repeated at the TOP level of
    /// the avatar-upload response body (`{ avatar_url, agent }`) exactly as the product
    /// does, so both faces read the single derived value rather than recomputing it.
    pub(crate) fn avatar_url(&self) -> Option<String> {
        self.avatar_url.clone()
    }
}

/// The public tier of the by-id read: what any signed-in human may see. A separate
/// type rather than nulled fields, so a privileged column cannot leak by omission.
#[derive(Serialize)]
pub(crate) struct AgentPublicDto {
    id: String,
    name: String,
    display_name: String,
    runtime: &'static str,
    model: Option<String>,
    reasoning_effort: Option<String>,
    fast_mode: Option<bool>,
    host_id: String,
    host_label: Option<String>,
    description: Option<String>,
    created_by_user_id: String,
    avatar_url: Option<String>,
    created_at: i64,
}

impl From<AgentDto> for AgentPublicDto {
    fn from(dto: AgentDto) -> Self {
        Self {
            id: dto.id,
            name: dto.name,
            display_name: dto.display_name,
            runtime: dto.runtime,
            model: dto.model,
            reasoning_effort: dto.reasoning_effort,
            fast_mode: dto.fast_mode,
            host_id: dto.host_id,
            host_label: dto.host_label,
            description: dto.description,
            created_by_user_id: dto.created_by_user_id,
            avatar_url: dto.avatar_url,
            created_at: dto.created_at,
        }
    }
}

pub(crate) fn agent_dto(detail: &AgentDetailFacts) -> AgentDto {
    let projection = detail.projection();
    let id = detail.source().as_str().to_string();
    AgentDto {
        name: projection.name.clone(),
        display_name: projection.display_name.clone(),
        runtime: projection.runtime.as_str(),
        model: projection.model.clone(),
        reasoning_effort: projection.reasoning_effort.clone(),
        fast_mode: projection.fast_mode,
        host_id: projection.host_id.clone(),
        host_label: host_label(
            projection.host_display_label.as_deref(),
            projection.host_name.as_deref(),
        ),
        description: projection.description.clone(),
        session_scope: projection.session_scope.clone(),
        created_by_user_id: detail.created_by_user_id().as_str().to_string(),
        avatar_url: avatar_url(
            &id,
            projection.avatar_storage_key.as_deref(),
            projection.updated_at,
        ),
        materialization_status: detail.visibility().materialization_status.as_str(),
        materialization_failed_code: projection.materialization_failed_code.clone(),
        materialization_failed_at: projection.materialization_failed_at,
        deletion_started_at: projection.deletion_started_at,
        created_at: projection.created_at,
        updated_at: projection.updated_at,
        id,
    }
}

/// The post-commit read+project face every mutating agent face shares. It re-reads the
/// committed row through the ACL-free read model and projects the canonical DTO,
/// source-bound to the requested id. See the module doc for why it is ACL-free and why
/// source-binding is a safety property, not an optimisation.
///
/// The `Err` variant is a small typed fault, NOT an eagerly-built `Response`: keeping the
/// `Result` cheap avoids `clippy::result_large_err`, and the boundary projection
/// ([`ReprojectFault::into_safe_500`]) is a second, shared step rather than N inline copies.
pub(crate) async fn reproject_committed_agent<R>(
    reads: &R,
    agent: &AgentId,
) -> Result<AgentDto, ReprojectFault>
where
    R: AgentDetailReadModel,
{
    let detail = match reads.read_agent_detail(agent).await {
        Ok(Some(detail)) => detail,
        Ok(None) => return Err(ReprojectFault::Vanished),
        Err(cause) => return Err(ReprojectFault::ReadFailed(cause)),
    };
    if detail.source() != agent {
        return Err(ReprojectFault::SourceMismatch {
            resolved: detail.source().clone(),
        });
    }
    Ok(agent_dto(&detail))
}

/// A post-commit reprojection fault. Every arm is a server-side invariant break — the
/// write is already durable, but no trustworthy DTO can be projected — so all three
/// collapse to the lane's safe `500`, distinguished only by their boundary log.
pub(crate) enum ReprojectFault {
    /// The row was present at the gate read but gone before projection — a delete raced.
    Vanished,
    /// The read model resolved a DIFFERENT agent than the committed id.
    SourceMismatch { resolved: AgentId },
    /// The post-commit read itself failed at the driver boundary.
    ReadFailed(PersistenceFailure),
}

impl ReprojectFault {
    /// Log at the boundary under the calling `operation`'s label and collapse to the
    /// canonical safe `500` — byte-identical to every public-agent face's
    /// `internal_error()` (`Internal Server Error`).
    pub(crate) fn into_safe_500(self, operation: &'static str, agent: &AgentId) -> Response {
        match self {
            Self::Vanished => tracing::error!(
                operation,
                agent_id = agent.as_str(),
                "committed agent row was gone before it could be projected"
            ),
            Self::SourceMismatch { resolved } => tracing::error!(
                operation,
                requested = agent.as_str(),
                resolved = resolved.as_str(),
                "agent post-commit projection resolved for the wrong agent"
            ),
            Self::ReadFailed(cause) => tracing::error!(
                operation,
                error_chain = crate::foundation::safe_log_field::safe_display(
                    &(crate::foundation::error::describe(&cause))
                ),
                "agent post-commit projection read failed"
            ),
        }
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
    }
}

/// `row.host_display_label?.trim() || row.host_name || null`: the trimmed label when
/// it is non-empty after trimming, else the host name when non-empty, else null.
///
/// The trim mirrors `String.prototype.trim()`, NOT Rust's Unicode `White_Space`: it
/// folds against the SAME ECMA-262 whitespace set the name envelope uses
/// ([`is_ecmascript_trim_whitespace`]), so `U+FEFF` trims and `U+0085` does not —
/// the two diverge under a naive `str::trim()`.
fn host_label(display_label: Option<&str>, host_name: Option<&str>) -> Option<String> {
    if let Some(label) = display_label {
        let trimmed = label.trim_matches(is_ecmascript_trim_whitespace);
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    match host_name {
        Some(name) if !name.is_empty() => Some(name.to_string()),
        _ => None,
    }
}

/// `principalAvatarUrl`: a cache-busted URL when the row carries a non-empty avatar
/// storage key, else null. Truthiness, not `!= null`, so an empty key is no avatar.
fn avatar_url(id: &str, avatar_storage_key: Option<&str>, updated_at: i64) -> Option<String> {
    match avatar_storage_key {
        Some(key) if !key.is_empty() => Some(format!("/api/agents/{id}/avatar?v={updated_at}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use axum::body::to_bytes;
    use serde_json::json;
    use silverhand_backend_core::{
        foundation::identity::UserId, identity_access::agent_detail_read::AgentDetailProjection,
        identity_access::agent_lifecycle::MaterializationStatus,
        identity_access::agents::runtime::RuntimeKind,
    };

    use super::*;

    fn agent_id(id: &str) -> AgentId {
        AgentId::parse(id.to_owned()).unwrap()
    }

    fn user(id: &str) -> UserId {
        UserId::parse(id.to_owned()).unwrap()
    }

    /// A fully-populated projection: every optional column is `Some`, the avatar key is
    /// non-empty, and the host carries both a display label and a name so the coalesce
    /// has a first choice.
    fn full_projection() -> AgentDetailProjection {
        AgentDetailProjection {
            name: "agent-one".to_owned(),
            display_name: "Agent One".to_owned(),
            runtime: RuntimeKind::Codex,
            model: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            fast_mode: Some(true),
            host_id: "h1".to_owned(),
            host_display_label: Some("  Host One  ".to_owned()),
            host_name: Some("host-one".to_owned()),
            description: Some("a description".to_owned()),
            session_scope: "agent".to_owned(),
            avatar_storage_key: Some("avatars/a1.png".to_owned()),
            materialization_failed_code: Some("boom".to_owned()),
            materialization_failed_at: Some(4321),
            deletion_started_at: Some(9999),
            created_at: 1000,
            updated_at: 2000,
        }
    }

    fn facts(source: &str, creator: &str, projection: AgentDetailProjection) -> AgentDetailFacts {
        AgentDetailFacts::from_source(
            agent_id(source),
            user(creator),
            [],
            [],
            MaterializationStatus::Active,
            projection,
        )
    }

    /// The resident closed-set gate: the projector emits EVERY wire field, spelled
    /// exactly, with the derived `host_label` (trimmed label wins) and the cache-busted
    /// `avatar_url` reading the just-stamped `updated_at`. A field dropped or renamed in
    /// the projector fails this by an exact wire object comparison, not by review.
    #[tokio::test]
    async fn the_projector_emits_every_wire_field_including_the_derived_pair() {
        let dto = agent_dto(&facts("a1", "creator-1", full_projection()));
        let value = serde_json::to_value(&dto).unwrap();

        assert_eq!(
            value,
            json!({
                "id": "a1", "name": "agent-one", "display_name": "Agent One",
                "runtime": "codex", "model": "gpt-5.6-sol", "reasoning_effort": "high",
                "fast_mode": true, "host_id": "h1", "host_label": "Host One",
                "description": "a description", "session_scope": "agent",
                "created_by_user_id": "creator-1",
                "avatar_url": "/api/agents/a1/avatar?v=2000",
                "materialization_status": "active", "materialization_failed_code": "boom",
                "materialization_failed_at": 4321, "deletion_started_at": 9999,
                "created_at": 1000, "updated_at": 2000
            })
        );
    }

    /// The `host_label` coalesce trims against the ECMA-262 whitespace set, not Rust's
    /// Unicode `White_Space`. Bidirectional discriminator that a naive `str::trim()`
    /// fails BOTH ways: `U+FEFF` IS ECMAScript whitespace (JS trims it; Rust does not),
    /// and `U+0085` is NOT (JS keeps it; Rust would strip it).
    #[test]
    fn host_label_trims_the_ecmascript_whitespace_set_not_rusts() {
        // U+FEFF must be trimmed away, leaving the bare label — a `str::trim()` would
        // keep it, since FEFF is not Unicode White_Space.
        assert_eq!(
            host_label(Some("\u{FEFF}Host One\u{FEFF}"), Some("host-name")),
            Some("Host One".to_owned())
        );
        // U+0085 must survive — a `str::trim()` would strip it, since NEL is Unicode
        // White_Space, and the label would then differ from `String.prototype.trim()`.
        assert_eq!(
            host_label(Some("\u{0085}Host One\u{0085}"), Some("host-name")),
            Some("\u{0085}Host One\u{0085}".to_owned())
        );
        // A label that is only ECMAScript whitespace collapses to empty and falls
        // through to the host name.
        assert_eq!(
            host_label(Some("\u{FEFF}\u{3000}"), Some("host-name")),
            Some("host-name".to_owned())
        );
    }

    #[derive(Clone)]
    enum ReadOutcome {
        Present(Box<AgentDetailFacts>),
        Absent,
        Fails,
        /// A row resolved for a DIFFERENT agent than the one asked for.
        Mismatched(Box<AgentDetailFacts>),
    }

    struct FakeReads {
        outcome: ReadOutcome,
        calls: AtomicUsize,
    }

    impl FakeReads {
        fn new(outcome: ReadOutcome) -> Self {
            Self {
                outcome,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl AgentDetailReadModel for FakeReads {
        async fn read_agent_detail(
            &self,
            _agent: &AgentId,
        ) -> Result<Option<AgentDetailFacts>, PersistenceFailure> {
            self.calls.fetch_add(1, SeqCst);
            match &self.outcome {
                ReadOutcome::Present(facts) | ReadOutcome::Mismatched(facts) => {
                    Ok(Some(facts.as_ref().clone()))
                }
                ReadOutcome::Absent => Ok(None),
                ReadOutcome::Fails => {
                    Err(PersistenceFailure::from_source(io::Error::other("boom")))
                }
            }
        }
    }

    #[tokio::test]
    async fn a_present_source_bound_row_reprojects_to_the_full_dto() {
        let reads = FakeReads::new(ReadOutcome::Present(Box::new(facts(
            "a1",
            "creator-1",
            full_projection(),
        ))));
        let Ok(dto) = reproject_committed_agent(&reads, &agent_id("a1")).await else {
            panic!("a present, source-bound row projects");
        };
        let value = serde_json::to_value(&dto).unwrap();
        assert_eq!(value["id"], json!("a1"));
        assert_eq!(value["avatar_url"], json!("/api/agents/a1/avatar?v=2000"));
        assert_eq!(reads.calls.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn a_vanished_row_is_a_safe_500() {
        let reads = FakeReads::new(ReadOutcome::Absent);
        let Err(fault) = reproject_committed_agent(&reads, &agent_id("a1")).await else {
            panic!("an absent row is a fault");
        };
        assert!(matches!(fault, ReprojectFault::Vanished));
        let response = fault.into_safe_500("test-op", &agent_id("a1"));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"Internal Server Error");
    }

    #[tokio::test]
    async fn a_read_failure_is_a_safe_500() {
        let reads = FakeReads::new(ReadOutcome::Fails);
        let Err(fault) = reproject_committed_agent(&reads, &agent_id("a1")).await else {
            panic!("a read failure is a fault");
        };
        assert!(matches!(fault, ReprojectFault::ReadFailed(_)));
        let response = fault.into_safe_500("test-op", &agent_id("a1"));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A row that resolves for a DIFFERENT agent is a safe `500`, never a cross-agent
    /// leak: the reproject face refuses to project the mismatched row.
    #[tokio::test]
    async fn a_source_mismatched_row_is_a_safe_500_never_a_leak() {
        let reads = FakeReads::new(ReadOutcome::Mismatched(Box::new(facts(
            "other-agent",
            "creator-1",
            full_projection(),
        ))));
        let Err(fault) = reproject_committed_agent(&reads, &agent_id("a1")).await else {
            panic!("a mismatched row is a fault");
        };
        match &fault {
            ReprojectFault::SourceMismatch { resolved } => {
                assert_eq!(resolved.as_str(), "other-agent");
            }
            _ => panic!("expected a source mismatch"),
        }
        let response = fault.into_safe_500("test-op", &agent_id("a1"));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
