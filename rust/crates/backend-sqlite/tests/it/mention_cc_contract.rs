//! The Rust half of the mention CC's durable-text contract.
//!
//! The artifact under `rust/test-fixtures/mention-cc/` is the contract both
//! implementations answer to: inputs and the expectation, checked in, with the
//! other side asserting the same rows against its production renderer. Nothing
//! here is a snapshot of this side's own output.
//!
//! It lives in this crate's test tree because a JSON reader does: the projection it
//! exercises is pure and belongs to `backend-core`.

use silverhand_backend_core::conversation_messaging::messaging::{
    ParticipantHandle,
    mention_cc::{MentionCopySource, mention_excerpt, render_copy_body},
};

const CONTRACT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../rust/test-fixtures/mention-cc/contract.json"
));

fn contract() -> serde_json::Value {
    serde_json::from_str(CONTRACT).expect("the shared contract must parse")
}

fn text<'a>(case: &'a serde_json::Value, field: &str) -> &'a str {
    case[field]
        .as_str()
        .unwrap_or_else(|| panic!("contract case is missing {field}: {case}"))
}

#[test]
fn the_excerpt_matches_the_shared_contract() {
    let contract = contract();
    let cases = contract["excerpt"].as_array().expect("excerpt cases");
    assert!(!cases.is_empty());

    for case in cases {
        assert_eq!(
            mention_excerpt(text(case, "input")),
            text(case, "expected"),
            "excerpt case {} disagrees with the shared contract",
            text(case, "name")
        );
    }
}

#[test]
fn the_copy_body_matches_the_shared_contract() {
    let contract = contract();
    let cases = contract["body"].as_array().expect("body cases");
    assert!(!cases.is_empty());

    for case in cases {
        let recipient = ParticipantHandle::parse(text(case, "agentHandle"))
            .expect("a contract recipient is a name");
        let rendered = render_copy_body(&MentionCopySource {
            recipient_handle: &recipient,
            source_workspace_name: text(case, "sourceWorkspaceName"),
            source_conversation_title: text(case, "sourceConversationTitle"),
            source_content: text(case, "sourceContent"),
        });

        assert_eq!(
            rendered,
            text(case, "expectedBody"),
            "body case {} disagrees with the shared contract",
            text(case, "name")
        );
    }
}
