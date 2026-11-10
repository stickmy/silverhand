use super::*;
use silverhand_backend_core::conversation_messaging::messaging::collaboration::attention_projection::{
    AttentionCount, WorkspaceAttentionCount,
};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;

#[derive(Clone, Default)]
struct Ports {
    calls: Arc<AtomicUsize>,
}

impl AttentionProjectionPort for Ports {
    type Error = Infallible;

    async fn badges(&self, _: &UserId) -> Result<AttentionBadgeRollup, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AttentionBadgeRollup::projected(
            vec![WorkspaceAttentionCount::projected(
                WorkspaceId::parse("w").unwrap(),
                AttentionCount::new(2).unwrap(),
            )],
            AttentionCount::new(3).unwrap(),
            vec![],
        )
        .unwrap())
    }

    async fn modes(
        &self,
        _: &UserId,
        _: &WorkspaceId,
    ) -> Result<Result<WorkspaceAttentionModes, AttentionModesRefusal>, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Ok(WorkspaceAttentionModes::projected(false, vec![])))
    }
}

async fn call(port: Ports, path: &str, auth: bool) -> (u16, Value) {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    if auth {
        request
            .extensions_mut()
            .insert(UserId::parse("local").unwrap());
    }
    let response = router(port).oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn authentication_refusal_reaches_no_projection() {
    let port = Ports::default();
    let calls = port.calls.clone();
    let (status, body) = call(port, BADGES_PATH, false).await;
    assert_eq!(status, 500);
    assert_eq!(body, json!({"error":"internal error"}));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn badge_wire_carries_direct_pairs_inside_total() {
    let (status, body) = call(Ports::default(), BADGES_PATH, true).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"by_workspace":{"w":2},"direct_pairs":3,"total":5,"muted_workspace_ids":[]})
    );
}

#[tokio::test]
async fn modes_wire_uses_the_admitted_request_coordinate_not_a_port_return_channel() {
    let (status, body) = call(
        Ports::default(),
        "/api/attention/modes?workspace_id=requested-b",
        true,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["workspace_id"], "requested-b");
}
