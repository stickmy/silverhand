//! Authenticated by-id read for `GET /api/hosts/{id}` at the tier
//! [`computer_read_tier`] grants the requester.
//!
//! The owner reads the full host wire with its live attachment name overlaid
//! (`view: "privileged"`); any other signed-in human reads [`HostPublicDto`]
//! (`view: "public"`). An absent row and someone else's unfinalized invite draft are
//! one `404 {"error":"host not found","error_code":"host_not_found"}`.
//!
//! The live attachment name is read only for the privileged tier, so a public reader
//! never learns the hostname a daemon announced.
//!
//! Unlike the bulk list, the owner still gets an unfinalized user-invite draft here,
//! so the add-computer modal that minted it can watch pending → attached → active.
//!
//! The human principal's absence is a composition fault: fail-closed `500`.

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use silverhand_backend_core::{
    foundation::identity::{HostId, UserId},
    host_runtime::read::{ComputerReadModel, computer_read_tier},
    identity_access::capability::read_tier::ReadTier,
};

use crate::{
    application_orchestration::daemon_delivery::DaemonDeliveryController,
    foundation::daemon_transport::DaemonConnectionAuthority, foundation::error,
};

use super::{
    host_not_found,
    host_wire::{HostDto, HostPublicDto, host_wire},
    internal_error,
};

/// The product route template, in axum's `{id}` capture syntax.
pub const HOST_DETAIL_PATH: &str = "/api/hosts/{id}";

/// The live attachment name for a computer, as the websocket controller sees it.
///
/// Diagnostic-only availability metadata — never identity, never a dedupe or
/// authorization input — so it is read only after the requester is admitted, and its
/// absence is an ordinary `null`, never a refusal. Synchronous because it reflects an
/// in-memory live view, not persistence.
pub trait ConnectedHostName: Clone + Send + Sync + 'static {
    fn connected_host_name(&self, computer: &HostId) -> Option<String>;
}

#[derive(Clone)]
pub struct ConnectedHostNameView {
    authority: DaemonConnectionAuthority,
}

impl ConnectedHostNameView {
    pub(crate) fn new(authority: DaemonConnectionAuthority) -> Self {
        Self { authority }
    }
}

impl ConnectedHostName for ConnectedHostNameView {
    fn connected_host_name(&self, computer: &HostId) -> Option<String> {
        self.authority.connected_host_name(computer)
    }
}

impl DaemonDeliveryController {
    pub fn connected_host_names(&self) -> ConnectedHostNameView {
        ConnectedHostNameView::new(self.connection_authority())
    }
}

#[derive(Clone)]
struct DetailState<R, N> {
    read_model: R,
    connected: N,
}

pub fn router<R, N>(read_model: R, connected: N) -> Router
where
    R: ComputerReadModel,
    N: ConnectedHostName,
{
    Router::new()
        .route(HOST_DETAIL_PATH, get(host_detail::<R, N>))
        .with_state(DetailState {
            read_model,
            connected,
        })
}

async fn host_detail<R, N>(
    Path(host_id): Path<String>,
    requester: Option<Extension<UserId>>,
    State(state): State<DetailState<R, N>>,
) -> Response
where
    R: ComputerReadModel,
    N: ConnectedHostName,
{
    let Some(Extension(requester)) = requester else {
        tracing::error!("host-detail route reached without its human principal fact");
        return internal_error();
    };

    // A malformed id names no computer — the same 404 an absent row gets, so the
    // surface never distinguishes "unparseable" from "not there".
    let Ok(computer) = HostId::parse(host_id) else {
        return host_not_found();
    };

    let record = match state.read_model.read_computer(computer).await {
        Ok(record) => record,
        Err(cause) => {
            tracing::error!(
                error_chain =
                    crate::foundation::safe_log_field::safe_display(&(error::describe(&cause))),
                "host-detail read failed"
            );
            return internal_error();
        }
    };

    let Some((tier, record)) = record
        .and_then(|record| computer_read_tier(&requester, &record).map(|tier| (tier, record)))
    else {
        return host_not_found();
    };

    let mut host = host_wire(&record);
    match tier {
        ReadTier::Privileged => {
            host.connected_host_name = state.connected.connected_host_name(&record.id);
            Json(HostDetailResponse {
                view: tier.as_str(),
                host,
            })
            .into_response()
        }
        ReadTier::Public => Json(HostPublicDetailResponse {
            view: tier.as_str(),
            host: host.into(),
        })
        .into_response(),
    }
}

/// Success body for the owner: `{ "view": "privileged", "host": HostDto }`.
#[derive(Serialize)]
struct HostDetailResponse {
    view: &'static str,
    host: HostDto,
}

/// Success body for any other signed-in human: `{ "view": "public", "host" }`.
#[derive(Serialize)]
struct HostPublicDetailResponse {
    view: &'static str,
    host: HostPublicDto,
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
        foundation::persistence::PersistenceFailure,
        host_runtime::computer::{ComputerRecord, HostKind, ProvisioningSource},
    };
    use tower::ServiceExt;

    use super::*;

    const REQUESTER: &str = "u-requester";

    fn user(id: &str) -> UserId {
        UserId::parse(id).expect("user id")
    }

    fn host(id: &str) -> HostId {
        HostId::parse(id).expect("host id")
    }

    /// A fully-active user-invite computer owned by `owner`.
    fn record(id: &str, owner: &str) -> ComputerRecord {
        ComputerRecord {
            id: host(id),
            name: format!("{id}-name"),
            kind: HostKind::Remote,
            created_at: 1,
            owner: Some(user(owner)),
            installation_id: None,
            last_seen_at: None,
            activated_at: Some(11),
            finalized_at: Some(12),
            revoked_at: None,
            provisioning_source: ProvisioningSource::UserInvite,
            display_label: None,
        }
    }

    #[derive(Clone)]
    enum ReadOutcome {
        Present(ComputerRecord),
        Absent,
        Fails,
    }

    #[derive(Clone)]
    struct FakeComputers {
        outcome: ReadOutcome,
        reads: Arc<AtomicUsize>,
    }

    impl ComputerReadModel for FakeComputers {
        async fn list_for_owner(
            &self,
            _owner: UserId,
        ) -> Result<Vec<ComputerRecord>, PersistenceFailure> {
            unreachable!("the by-id read never lists")
        }

        async fn read_computer(
            &self,
            computer: HostId,
        ) -> Result<Option<ComputerRecord>, PersistenceFailure> {
            assert_eq!(computer.as_str(), "h1");
            self.reads.fetch_add(1, SeqCst);
            match &self.outcome {
                ReadOutcome::Present(record) => Ok(Some(record.clone())),
                ReadOutcome::Absent => Ok(None),
                ReadOutcome::Fails => Err(PersistenceFailure::from_source(io::Error::other(
                    "host gone",
                ))),
            }
        }
    }

    /// A live-attachment seam that answers a fixed name for any computer, and counts
    /// how many times it is asked — so a test can prove it is read ONLY after the
    /// requester is admitted.
    #[derive(Clone)]
    struct FakeAttachment {
        name: Option<String>,
        lookups: Arc<AtomicUsize>,
    }

    impl FakeAttachment {
        fn announcing(name: Option<&str>) -> Self {
            Self {
                name: name.map(str::to_owned),
                lookups: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl ConnectedHostName for FakeAttachment {
        fn connected_host_name(&self, computer: &HostId) -> Option<String> {
            assert_eq!(computer.as_str(), "h1");
            self.lookups.fetch_add(1, SeqCst);
            self.name.clone()
        }
    }

    struct Reply {
        status: StatusCode,
        body: String,
        reads: usize,
        lookups: usize,
    }

    async fn get_detail(
        outcome: ReadOutcome,
        attachment: FakeAttachment,
        path_id: &str,
        with_principal: bool,
    ) -> Reply {
        let reads = Arc::new(AtomicUsize::new(0));
        let lookups = attachment.lookups.clone();
        let read_model = FakeComputers {
            outcome,
            reads: reads.clone(),
        };
        let mut request = Request::get(format!("/api/hosts/{path_id}"));
        if with_principal {
            request = request.extension(user(REQUESTER));
        }
        let response = router(read_model, attachment)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Reply {
            status,
            body: String::from_utf8(body.to_vec()).unwrap(),
            reads: reads.load(SeqCst),
            lookups: lookups.load(SeqCst),
        }
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str::<Value>(body).unwrap()
    }

    /// The owner's computer, projected once, with the live attachment name overlaid
    /// onto the wire's base `null`.
    #[tokio::test]
    async fn the_owner_gets_the_computer_with_its_live_attachment_name() {
        let reply = get_detail(
            ReadOutcome::Present(record("h1", REQUESTER)),
            FakeAttachment::announcing(Some("live-box")),
            "h1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "view": "privileged",
                "host": {
                    "id": "h1",
                    "name": "h1-name",
                    "kind": "remote",
                    "created_at": 1,
                    "revoked_at": null,
                    "last_seen_at": null,
                    "activated_at": 11,
                    "finalized_at": 12,
                    "connected_host_name": "live-box",
                    "provisioning_source": "user_invite",
                    "status": "active",
                    "placeable": true,
                    "creator_user_id": "u-requester",
                    "display_label": null
                }
            })
        );
        assert_eq!(reply.reads, 1);
        assert_eq!(reply.lookups, 1);
    }

    /// Nothing attached is an ordinary `null`, not a missing field or a refusal.
    #[tokio::test]
    async fn a_computer_with_no_live_attachment_reports_null() {
        let reply = get_detail(
            ReadOutcome::Present(record("h1", REQUESTER)),
            FakeAttachment::announcing(None),
            "h1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body)["host"].get("connected_host_name"),
            Some(&Value::Null)
        );
        assert_eq!(reply.lookups, 1);
    }

    /// A foreign computer is the public tier, and never reaches the live-attachment
    /// seam: a public reader does not learn the hostname a daemon announced.
    #[tokio::test]
    async fn a_foreign_computer_is_exactly_the_public_tier_and_never_looks_up() {
        let reply = get_detail(
            ReadOutcome::Present(record("h1", "u-someone-else")),
            FakeAttachment::announcing(Some("live-box")),
            "h1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            parsed(&reply.body),
            json!({
                "view": "public",
                "host": {
                    "id": "h1",
                    "name": "h1-name",
                    "display_label": null,
                    "kind": "remote",
                    "status": "active",
                    "creator_user_id": "u-someone-else",
                    "created_at": 1,
                    "last_seen_at": null
                }
            })
        );
        assert_eq!(reply.lookups, 0);
    }

    /// An absent row and someone else's unfinalized draft are one envelope, and
    /// neither reaches the live-attachment seam.
    #[tokio::test]
    async fn a_missing_computer_or_foreign_draft_is_the_same_404() {
        let refused = json!({ "error": "host not found", "error_code": "host_not_found" });
        let mut draft = record("h1", "u-someone-else");
        draft.finalized_at = None;
        for outcome in [ReadOutcome::Absent, ReadOutcome::Present(draft)] {
            let reply = get_detail(
                outcome,
                FakeAttachment::announcing(Some("live-box")),
                "h1",
                true,
            )
            .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND);
            assert_eq!(parsed(&reply.body), refused);
            assert_eq!(reply.reads, 1);
            assert_eq!(reply.lookups, 0);
        }
    }

    /// A trailing empty segment is not this route at all: `/api/hosts/` fails to
    /// match `{id}`, so the framework answers its own `404` before the handler — the
    /// principal, read and ownership gates are never even reached, let alone bypassed.
    /// (The in-handler empty-id guard mirrors the TypeScript one and the by-id-with-
    /// suffix siblings, where an empty middle segment DOES route; it is faithful
    /// defense, not reachable here.)
    #[tokio::test]
    async fn a_trailing_empty_segment_is_not_this_route() {
        let reply = get_detail(
            ReadOutcome::Present(record("h1", REQUESTER)),
            FakeAttachment::announcing(Some("live-box")),
            "",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.body, "");
        assert_eq!(reply.reads, 0);
        assert_eq!(reply.lookups, 0);
    }

    #[tokio::test]
    async fn a_failed_read_is_the_frameworks_plain_500() {
        let reply = get_detail(
            ReadOutcome::Fails,
            FakeAttachment::announcing(Some("live-box")),
            "h1",
            true,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.lookups, 0);
    }

    /// The principal is the perimeter's contribution; without it the route reads
    /// NOTHING and fails closed, rather than guessing a requester.
    #[tokio::test]
    async fn a_missing_principal_reads_nothing_and_fails_closed() {
        let reply = get_detail(
            ReadOutcome::Present(record("h1", REQUESTER)),
            FakeAttachment::announcing(Some("live-box")),
            "h1",
            false,
        )
        .await;
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.body, "Internal Server Error");
        assert_eq!(reply.reads, 0);
        assert_eq!(reply.lookups, 0);
    }
}
