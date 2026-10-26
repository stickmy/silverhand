//! The send-ingress contract, consumed on the Rust side.
//!
//! ONE ARTIFACT, TWO IMPLEMENTATIONS. The cases, their bodies and their expectations are
//! checked in beside the reference implementation and asserted there against the real
//! production schemas; this file reads the SAME file and answers the same cases through the
//! real product router. Neither side's output is the other's truth, and neither carries its own
//! copy of the expectations.
//!
//! WHAT ORDER MEANS HERE: passing this test says nothing about whether the reference oracle
//! ran. The artifact names a checked-in command that runs the oracle first and this consumer
//! only on success; running `cargo test` alone proves one side.
//!
//! A DIVERGENCE IS EXPLICIT OR IT IS A BUG. A case may carry `rust_expected` only with a
//! declared, body-only deviation; this file therefore answers `rust_expected` when present and
//! the shared `reference` otherwise, and the artifact's own meta-gates (asserted by the oracle)
//! are what stop a Rust-only snapshot from hiding in that field.

use crate::no_aftermath;

use no_aftermath::NoAftermath;

use std::{
    convert::Infallible,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{Router, body::Body, http::Request};
use serde_json::{Value, json};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, AddressedPrincipals, CapabilityDenied, ChannelAccess, ChannelAccessRefusal,
        ConversationRefusal, DirectLineAuthority, DirectPairAllocation, ResolvedStream,
        SendCapability, SendDestination, SendMessage, SendMessageError, SendMessageOutcome,
        SendMessageUnitOfWork, TargetAddress,
    },
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId, UserId},
    host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration, RuntimePrincipal},
};
use silverhand_backend_server::conversation_messaging::messaging::router::{
    SendState, send_router,
};
use tower::ServiceExt;

const CONVERSATION: &str = "conv-1";
const USER: &str = "user-1";
const AGENT: &str = "agent-1";

fn contract() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../rust/test-fixtures/send-ingress/contract.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the checked-in contract is readable at {path:?}: {error}"));
    serde_json::from_str(&text).expect("the contract is JSON")
}

/// What the write unit was handed, in the terms the artifact's admitted cases describe.
#[derive(Clone, Debug, Default)]
struct Recorded {
    content: String,
    attachments: Vec<Value>,
    destination: Option<SendDestination>,
    freshness_bypass: bool,
}

#[derive(Clone, Default)]
struct RecordingUnit(Arc<Mutex<Option<Recorded>>>);

impl RecordingUnit {
    fn recorded(&self) -> Option<Recorded> {
        self.0.lock().unwrap().clone()
    }
}

impl SendMessageUnitOfWork for RecordingUnit {
    async fn send_message(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        *self.0.lock().unwrap() = Some(Recorded {
            content: command.content().as_str().to_owned(),
            attachments: command
                .attachment_refs()
                .iter()
                .map(|reference| match &reference.label {
                    Some(label) => {
                        json!({ "id": reference.id.as_str(), "label": label.to_string() })
                    }
                    None => json!({ "id": reference.id.as_str() }),
                })
                .collect(),
            destination: Some(command.destination().clone()),
            freshness_bypass: command.freshness_bypass(),
        });
        // A fact needs a committed transaction, which this has not got. An admitted case is
        // asserted by WHAT REACHED HERE, so the answer past this point is not the subject.
        Err(ConversationRefusal::NotFound.into())
    }
}

struct OpenChannel;

impl ChannelAccess for OpenChannel {
    type Error = Infallible;

    async fn may_post(
        &self,
        _channel: &ConversationId,
        _author: &UserId,
    ) -> Result<Result<(), ChannelAccessRefusal>, Self::Error> {
        Ok(Ok(()))
    }
}

/// Records the raw address the door handed it, which is the admitted `target` the artifact
/// names — the command itself carries the RESOLVED coordinate, which is a different fact.
#[derive(Clone, Default)]
struct RecordingAddress(Arc<Mutex<Option<String>>>);

impl TargetAddress for RecordingAddress {
    type Error = Infallible;

    async fn resolve(
        &self,
        _agent: &AgentId,
        target: &str,
        _allocation: DirectPairAllocation,
        _intent: silverhand_backend_core::conversation_messaging::messaging::DeliveryIntent,
    ) -> Result<Result<ResolvedStream, AddressRefusal>, Self::Error> {
        *self.0.lock().unwrap() = Some(target.to_owned());
        Ok(Ok(ResolvedStream {
            conversation: ConversationId::parse(CONVERSATION).unwrap(),
            thread_root: None,
        }))
    }
}

struct GrantsSend;

impl SendCapability for GrantsSend {
    type Error = Infallible;

    async fn may_send_messages(
        &self,
        _agent: &AgentId,
        _conversation: &ConversationId,
    ) -> Result<Result<(), CapabilityDenied>, Self::Error> {
        Ok(Ok(()))
    }
}

struct GrantsNothing;

impl DirectLineAuthority for GrantsNothing {
    async fn direct_lines(
        &self,
        _author: &UserId,
        _addressed: &AddressedPrincipals,
    ) -> Result<Vec<AgentId>, PersistenceFailure> {
        Ok(Vec::new())
    }
}

/// The real product routes over permissive gates: what a case exercises is the ingress, so
/// every gate that is not the subject answers yes.
fn mounted(unit: RecordingUnit, address: RecordingAddress) -> Router {
    send_router(SendState {
        channel_access: OpenChannel,
        target_address: address,
        capability: GrantsSend,
        authority: GrantsNothing,
        unit_of_work: unit,
        aftermath: NoAftermath,
    })
}

/// The body a case sends: its own JSON, its own unreadable text, or its JSON with a generated
/// `content` — the one generator rule the artifact declares.
fn body_of(case: &Value) -> Body {
    if let Some(raw) = case.get("raw_body").and_then(Value::as_str) {
        return Body::from(raw.to_owned());
    }
    let generated = case.get("generated_content").map(|spec| {
        let character = spec["repeat"].as_str().expect("a character to repeat");
        let times = usize::try_from(spec["times"].as_u64().expect("a repeat count")).unwrap();
        character.repeat(times)
    });
    match generated {
        None => Body::from(case["body"].to_string()),
        Some(content) => {
            let mut object = case["body"].as_object().cloned().unwrap_or_default();
            object.insert("content".to_owned(), json!(content));
            Body::from(Value::Object(object).to_string())
        }
    }
}

fn request(case: &Value) -> Request<Body> {
    let door = case["door"].as_str().expect("a door");
    let path = match door {
        "human" => format!("/api/channels/{CONVERSATION}/messages"),
        "agent" => format!("/internal/agents/{AGENT}/send-message"),
        other => panic!("unknown door {other}"),
    };
    let mut request = Request::post(path).body(body_of(case)).unwrap();
    match door {
        "human" => {
            request
                .extensions_mut()
                .insert(UserId::parse(USER).unwrap());
        }
        _ => {
            request
                .extensions_mut()
                .insert(RuntimePrincipal::AgentSlot {
                    agent: AgentId::parse(AGENT).unwrap(),
                    custody: RuntimeCustody::new(
                        RuntimeGeneration::new(1).unwrap(),
                        DaemonInstanceId::parse("daemon-1").unwrap(),
                    ),
                });
        }
    }
    request
}

/// The expectation this side answers: the declared deviation when the case has one, the shared
/// reference otherwise.
fn expected(case: &Value) -> &Value {
    case.get("rust_expected").unwrap_or(&case["reference"])
}

#[tokio::test]
async fn the_ingress_answers_every_case_the_contract_names() {
    let contract = contract();
    let cases = contract["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "the contract has cases");
    // The artifact must point at this file, so a rename cannot leave it pointing at nothing.
    assert!(
        contract["consumer"]
            .as_str()
            .is_some_and(|path| path.ends_with("tests/it/send_ingress_contract.rs")),
        "{}",
        contract["consumer"]
    );

    for case in cases {
        let name = case["name"].as_str().expect("a name");
        let unit = RecordingUnit::default();
        let address = RecordingAddress::default();
        let router = mounted(unit.clone(), address.clone());

        let response = router.oneshot(request(case)).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);

        let expectation = expected(case);
        if let Some(refused) = expectation.get("refused") {
            assert_eq!(
                status.as_u16(),
                u16::try_from(refused["status"].as_u64().unwrap()).unwrap(),
                "{name}: status, body was {body}"
            );
            assert_eq!(body, refused["body"], "{name}");
            assert!(
                unit.recorded().is_none(),
                "{name}: a refused request reached the write unit"
            );
            continue;
        }

        let admitted = &expectation["admitted"];
        // ADMITTED means it got past the ingress, which is what reaching the write unit is.
        let recorded = unit
            .recorded()
            .unwrap_or_else(|| panic!("{name}: never reached the write unit ({status}: {body})"));
        assert_eq!(recorded.content, admitted["content"], "{name}: content");
        assert_eq!(
            Value::Array(recorded.attachments.clone()),
            admitted["attachments"],
            "{name}: attachments"
        );
        match case["door"].as_str().unwrap() {
            "human" => {
                let expected_reply = admitted["reply_to_message_id"].as_str();
                let actual = match recorded.destination.as_ref().expect("a destination") {
                    SendDestination::MainLine => None,
                    SendDestination::Reply(message) => Some(message.as_str().to_owned()),
                    other => panic!("{name}: the human door produced {other:?}"),
                };
                assert_eq!(
                    actual.as_deref(),
                    expected_reply,
                    "{name}: reply_to_message_id"
                );
            }
            _ => {
                assert_eq!(
                    address.0.lock().unwrap().as_deref(),
                    admitted["target"].as_str(),
                    "{name}: target"
                );
                assert_eq!(
                    recorded.freshness_bypass,
                    admitted["freshness_bypass"].as_bool().unwrap(),
                    "{name}: freshness_bypass"
                );
            }
        }
    }
}
