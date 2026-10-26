//! The two message-send routes as an operator and a caller see them, end to end.
//!
//! Everything here goes through the real [`send_router`]. An earlier version of this
//! suite mounted the stages under a hand-built router with a `200 reached` stub for its
//! terminal answer, which proved the fence worked under a router production does not
//! have — and could not have caught a handler that took its destination from the wrong
//! coordinate, because the stub had no destination at all.
//!
//! Three contracts live here that no unit test can hold:
//!
//! - the ingress ORDER over real HTTP: the fence before anything, then the path
//!   coordinate, then the shape, then the content cap;
//! - how MANY times a failure is reported — a future edit that logged the cause in the
//!   middleware and again in a renderer would leave every unit test green while doubling
//!   the noise an operator has to read;
//! - that each door's gate order survives being mounted. The order is decided in the
//!   adapters, but a router that reached for the body before dispatching would break it
//!   without touching them.

// The shared log-capture harness, included rather than copied: this suite and the product
// send suite both read what an operator saw, and one writer is enough for both.
use crate::log_capture;
use crate::no_aftermath;

use no_aftermath::NoAftermath;

use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode, header::CONTENT_LENGTH},
};
use futures_util::stream;
use serde_json::Value;
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::{
    conversation_messaging::messaging::{
        AddressRefusal, AddressedPrincipals, CapabilityDenied, ChannelAccess, ChannelAccessRefusal,
        ConversationRefusal, DirectLineAuthority, DirectPairAllocation, PrincipalKind,
        ResolvedStream, SendCapability, SendMessage, SendMessageError, SendMessageOutcome,
        SendMessageUnitOfWork, TargetAddress,
    },
    foundation::identity::{AgentId, ConversationId, DaemonInstanceId, UserId},
    host_runtime::runtime_credential::{RuntimeCustody, RuntimeGeneration, RuntimePrincipal},
};
use silverhand_backend_server::conversation_messaging::messaging::{
    MESSAGE_SEND_MAX_BODY_BYTES,
    router::{SendState, send_router},
};
use tower::ServiceExt;

use log_capture::LogBuffer;

const FRAME_CAUSE: &str = "upstream frame source vanished";

const HUMAN_PATH: &str = "/api/channels/conv-1/messages";
const AGENT_PATH: &str = "/internal/agents/agent-1/send-message";
const HUMAN_PATH_WITHOUT_ID: &str = "/api/channels//messages";
const AGENT_PATH_WITHOUT_ID: &str = "/internal/agents//send-message";

#[derive(Debug, thiserror::Error)]
#[error("upstream frame source vanished")]
struct FrameSourceGone;

/// A body that delivers real bytes and then fails, with no declared length. The prefix
/// matters: part of the read had already succeeded, so the failure cannot be mistaken
/// for an empty body.
fn breaks_after_one_frame() -> Body {
    Body::from_stream(stream::iter(vec![
        Ok::<Bytes, FrameSourceGone>(Bytes::from_static(b"{\"content\":\"hi\"}")),
        Err(FrameSourceGone),
    ]))
}

/// The human gate: a predicate, which is all the port can express.
struct Channel(Result<(), ChannelAccessRefusal>);

impl ChannelAccess for Channel {
    type Error = Infallible;

    async fn may_post(
        &self,
        _channel: &ConversationId,
        _author: &UserId,
    ) -> Result<Result<(), ChannelAccessRefusal>, Self::Error> {
        tokio::task::yield_now().await;
        Ok(self.0)
    }
}

/// How many times a gate was ASKED. The refusal a request gets says which gate answered; only a
/// counter says whether the later ones ran at all, and a capability check is a real read against
/// a roster — asking it early is not a harmless detail.
#[derive(Clone, Default)]
struct Calls(Arc<AtomicUsize>);

impl Calls {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn record(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Resolves every address to one fixed main-line stream, or refuses. The grammar that reads an
/// address is not restated here; what a test needs is where one landed.
struct Address {
    outcome: Result<(), AddressRefusal>,
    calls: Calls,
}

impl TargetAddress for Address {
    type Error = Infallible;

    async fn resolve(
        &self,
        _agent: &AgentId,
        _target: &str,
        _allocation: DirectPairAllocation,
        _intent: silverhand_backend_core::conversation_messaging::messaging::DeliveryIntent,
    ) -> Result<Result<ResolvedStream, AddressRefusal>, Self::Error> {
        tokio::task::yield_now().await;
        self.calls.record();
        Ok(self.outcome.map(|()| ResolvedStream {
            conversation: resolved_stream(),
            thread_root: None,
        }))
    }
}

/// The stream every address in this file resolves to. Deliberately NOT the conversation an
/// anchored credential is anchored to, so a reach mistake is visible rather than accidental.
fn resolved_stream() -> ConversationId {
    ConversationId::parse("conv-2").unwrap()
}

/// The grant, and only the grant.
struct Grant {
    outcome: Result<(), CapabilityDenied>,
    calls: Calls,
}

impl SendCapability for Grant {
    type Error = Infallible;

    async fn may_send_messages(
        &self,
        _agent: &AgentId,
        _conversation: &ConversationId,
    ) -> Result<Result<(), CapabilityDenied>, Self::Error> {
        tokio::task::yield_now().await;
        self.calls.record();
        Ok(self.outcome)
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

/// Which side of the write unit's failure this fake plays: a refusal the caller can act on,
/// or a failure of ours. The two are answered AND reported differently, and that difference
/// is only observable at the boundary.
#[derive(Clone, Copy, Default)]
enum Fails {
    #[default]
    AsRefusal,
    /// Our own store, broken, with a named cause underneath.
    AsOurFailure,
}

const UNDERLYING_CAUSE: &str = "the write-ahead log is unreadable";

#[derive(Debug, thiserror::Error)]
#[error("the write-ahead log is unreadable")]
struct StoreBroken;

/// Counts what it was asked to commit. "Reached the write unit" is the only honest way
/// to say a request got all the way through, now that there is no stub answer standing
/// in for one.
#[derive(Clone, Default)]
struct CountingUnitOfWork(Arc<AtomicUsize>, Fails, Arc<Mutex<Vec<String>>>);

impl CountingUnitOfWork {
    fn failing(how: Fails) -> Self {
        Self(Arc::default(), how, Arc::default())
    }

    fn calls(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    /// Who the commands it was handed addressed, as the send itself derived it.
    fn addressed(&self) -> Vec<String> {
        self.2.lock().unwrap().clone()
    }
}

impl SendMessageUnitOfWork for CountingUnitOfWork {
    async fn send_message(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.2
            .lock()
            .unwrap()
            .extend(command.addressed().iter().map(|reference| {
                let kind = match reference.kind() {
                    PrincipalKind::Agent => "agent",
                    PrincipalKind::Human => "human",
                };
                format!("{kind}/{}", reference.handle().as_str())
            }));
        // A fact needs a committed transaction, which a fake has not got. Recording the
        // attempt, and which KIND of failure follows it, is the whole purpose.
        Err(match self.1 {
            Fails::AsRefusal => ConversationRefusal::NotFound.into(),
            Fails::AsOurFailure => PersistenceFailure::from_source(StoreBroken).into(),
        })
    }
}

fn mounted(
    channel: Result<(), ChannelAccessRefusal>,
    address: Result<(), AddressRefusal>,
    grant: Result<(), CapabilityDenied>,
    unit: CountingUnitOfWork,
) -> Router {
    watched(channel, address, grant, unit).0
}

/// The same router, plus the two counters, for the cells that are about WHEN a gate is asked.
fn watched(
    channel: Result<(), ChannelAccessRefusal>,
    address: Result<(), AddressRefusal>,
    grant: Result<(), CapabilityDenied>,
    unit: CountingUnitOfWork,
) -> (Router, Calls, Calls) {
    let asked_address = Calls::default();
    let asked_capability = Calls::default();
    let router = send_router(SendState {
        channel_access: Channel(channel),
        target_address: Address {
            outcome: address,
            calls: asked_address.clone(),
        },
        capability: Grant {
            outcome: grant,
            calls: asked_capability.clone(),
        },
        authority: GrantsNothing,
        unit_of_work: unit,
        aftermath: NoAftermath,
    });
    (router, asked_address, asked_capability)
}

/// Both gates open, so whatever answers is the request's own doing.
fn open(unit: CountingUnitOfWork) -> Router {
    mounted(Ok(()), Ok(()), Ok(()), unit)
}

fn human_request(path: &str, body: Body) -> Request<Body> {
    let mut request = Request::post(path).body(body).unwrap();
    request
        .extensions_mut()
        .insert(UserId::parse("user-1").unwrap());
    request
}

fn agent_request(path: &str, body: Body) -> Request<Body> {
    let mut request = Request::post(path).body(body).unwrap();
    // Unanchored by default: reach is then the resolver's answer alone, so a cell about
    // something else is not silently also a reach cell.
    request
        .extensions_mut()
        .insert(RuntimePrincipal::AgentSlot {
            agent: AgentId::parse("agent-1").unwrap(),
            custody: RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        });
    request
}

/// One request, and everything an operator and a caller each saw. Both doors go through this
/// helper on purpose: a future adapter that added a report of its own would have to defeat the
/// same assertion twice.
async fn observed(router: Router, request: Request<Body>) -> (StatusCode, String, Vec<Value>) {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let (status, body) = answered(router, request).await;
    drop(guard);
    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect();
    (status, body, errors)
}

/// A request from a credential ANCHORED to one conversation.
fn anchored_request(source: ConversationId) -> Request<Body> {
    let mut request = Request::post(AGENT_PATH)
        .body(Body::from(agent_send_body(16)))
        .unwrap();
    request
        .extensions_mut()
        .insert(RuntimePrincipal::ConversationSlot {
            agent: AgentId::parse("agent-1").unwrap(),
            source,
            custody: RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        });
    request
}

async fn answered(router: Router, request: Request<Body>) -> (StatusCode, String) {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let body = String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    (status, body)
}

/// A legal human body whose content is exactly `content_bytes` long.
fn human_send_body(content_bytes: usize) -> String {
    format!(r#"{{"content":"{}"}}"#, "a".repeat(content_bytes))
}

/// The agent equivalent. It carries an address because that door requires one, which is
/// also why the two envelopes are not the same size. The address itself is arbitrary:
/// its grammar belongs to the resolver, and the fake above ignores it.
fn agent_send_body(content_bytes: usize) -> String {
    format!(
        r#"{{"content":"{}","target":"conv-2"}}"#,
        "a".repeat(content_bytes)
    )
}

/// A body over the bound, DECLARED, on a request that also names no identifier AND
/// carries no principal.
///
/// The cross is the point: either of the other two faults would be answered by the
/// handler, so a 413 here can only mean the fence ran before the handler existed as far
/// as this request is concerned. It pins "envelope first" rather than "envelope before
/// parsing".
#[tokio::test]
async fn a_declared_oversize_body_is_refused_before_any_other_fault() {
    for path in [HUMAN_PATH_WITHOUT_ID, AGENT_PATH_WITHOUT_ID] {
        let unit = CountingUnitOfWork::default();
        let oversized = "a".repeat(MESSAGE_SEND_MAX_BODY_BYTES + 1);
        let request = Request::post(path)
            .header(CONTENT_LENGTH, oversized.len())
            .body(Body::from(oversized))
            .unwrap();

        let (status, body) = answered(open(unit.clone()), request).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        assert!(
            body.contains("request body exceeds message-send bound"),
            "{path}"
        );
        assert!(body.contains(r#""code":"content_too_large""#), "{path}");
        assert!(body.contains(r#""max_body_bytes":524288"#), "{path}");
        assert_eq!(unit.calls(), 0, "{path}");
    }
}

/// The same cross with the size UNDECLARED, which is the cell that kills a
/// limited-extractor design: nothing about this request would ever have reached for the
/// body, so a limit that only applies to extraction never fires and the answer degrades
/// to whichever fault the handler found first.
#[tokio::test]
async fn a_chunked_oversize_body_is_refused_before_any_other_fault() {
    for path in [HUMAN_PATH_WITHOUT_ID, AGENT_PATH_WITHOUT_ID] {
        let request = Request::post(path)
            .body(Body::from("a".repeat(MESSAGE_SEND_MAX_BODY_BYTES + 1)))
            .unwrap();

        let (status, body) = answered(open(CountingUnitOfWork::default()), request).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        assert!(
            body.contains("request body exceeds message-send bound"),
            "{path}"
        );
    }
}

/// The bound is exact, and it bounds the ENVELOPE rather than the content: a body of
/// precisely the bound gets past the fence and is answered by a LATER gate, and one byte
/// more does not get that far.
#[tokio::test]
async fn the_envelope_bound_admits_its_own_size_and_nothing_over() {
    let envelope = agent_send_body(0).len();
    let at_bound = agent_send_body(MESSAGE_SEND_MAX_BODY_BYTES - envelope);
    assert_eq!(at_bound.len(), MESSAGE_SEND_MAX_BODY_BYTES);

    // At the bound the fence passes it on, and the CONTENT cap is what answers — a
    // different gate with a different body, so the fence is not the thing refusing it.
    let (status, body) = answered(
        open(CountingUnitOfWork::default()),
        agent_request(AGENT_PATH, Body::from(at_bound)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body.contains("message content exceeds"), "{body}");
    assert!(!body.contains("max_body_bytes"), "{body}");

    let over = agent_send_body(MESSAGE_SEND_MAX_BODY_BYTES - envelope + 1);
    assert_eq!(over.len(), MESSAGE_SEND_MAX_BODY_BYTES + 1);
    let (status, body) = answered(
        open(CountingUnitOfWork::default()),
        agent_request(AGENT_PATH, Body::from(over)),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        body.contains("request body exceeds message-send bound"),
        "{body}"
    );
}

/// Once the envelope is admitted, the human door parses its conversation coordinate
/// before reading the body. The agent coordinate belongs to runtime authentication and
/// is exercised with that middleware in the served-app composition.
#[tokio::test]
async fn a_missing_human_identifier_answers_before_the_body_is_parsed() {
    let unit = CountingUnitOfWork::default();
    // The body is malformed too. If parsing came first this would be `invalid input`;
    // the identifier wins.
    let unparseable = || Body::from("not json at all");

    let (human_status, human_body) = answered(
        open(unit.clone()),
        human_request(HUMAN_PATH_WITHOUT_ID, unparseable()),
    )
    .await;
    assert_eq!(human_status, StatusCode::BAD_REQUEST);
    assert!(human_body.contains("channel id required"), "{human_body}");

    assert_eq!(unit.calls(), 0);
}

/// Shape before size: a body that is both the wrong shape AND oversized is answered as
/// the wrong shape — the shared minimal `422 {"error":"invalid input"}`, never the content
/// cap and never an echo of what was sent. The status alone carries "shape, not size": a
/// 413 would mean the cap won, and the door publishes no enumerated reason to leak.
#[tokio::test]
async fn an_invalid_shape_answers_before_the_content_cap() {
    let oversized_but_not_a_string = format!(r#"{{"content":{}}}"#, "1".repeat(70_000));

    let (status, body) = answered(
        open(CountingUnitOfWork::default()),
        agent_request(AGENT_PATH, Body::from(oversized_but_not_a_string)),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        serde_json::json!({ "error": "invalid input" }),
        "{body}"
    );
    assert!(
        !body.contains("111"),
        "the refusal echoed the value: {body}"
    );
}

/// The two doors word the same bound differently, and the difference is the contract: a
/// runtime is told what to do instead, a person is not.
#[tokio::test]
async fn each_door_words_the_content_cap_for_its_own_caller() {
    let unit = CountingUnitOfWork::default();

    let (agent_status, agent_body) = answered(
        open(unit.clone()),
        agent_request(AGENT_PATH, Body::from(agent_send_body(65_537))),
    )
    .await;
    let (human_status, human_body) = answered(
        open(unit.clone()),
        human_request(HUMAN_PATH, Body::from(human_send_body(65_537))),
    )
    .await;

    assert_eq!(agent_status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(human_status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        agent_body
            .contains("message content exceeds 64KiB limit; send it as an attachment instead"),
        "{agent_body}"
    );
    assert!(
        human_body.contains(r#""error":"message content exceeds 64KiB limit""#),
        "{human_body}"
    );
    // The human door must not acquire the agent's instruction.
    assert!(!human_body.contains("attachment"), "{human_body}");
    // Both report the same two bounds.
    for body in [&agent_body, &human_body] {
        assert!(body.contains(r#""max_bytes":65536"#), "{body}");
        assert!(body.contains(r#""actual_bytes":65537"#), "{body}");
    }
    assert_eq!(unit.calls(), 0);
}

/// A LEGAL JSON BODY THAT IS NOT AN OBJECT is refused at the root — the shared minimal
/// `422 {"error":"invalid input"}`, the same answer a missing or ill-typed field gets.
///
/// The door does not go looking for fields a non-object cannot have: the agent door in
/// particular does not invent its required `target`. The status is the assertion, and the
/// minimal body carries no enumerated reason to leak.
#[tokio::test]
async fn a_non_object_body_is_refused_at_the_root_on_both_doors() {
    let unit = CountingUnitOfWork::default();

    for root in ["7", "null", "[]", "\"x\""] {
        let (human_status, human_body) = answered(
            open(unit.clone()),
            human_request(HUMAN_PATH, Body::from(root)),
        )
        .await;
        let (agent_status, agent_body) = answered(
            open(unit.clone()),
            agent_request(AGENT_PATH, Body::from(root)),
        )
        .await;

        for (status, body) in [(human_status, &human_body), (agent_status, &agent_body)] {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{root}");
            assert_eq!(
                serde_json::from_str::<Value>(body).unwrap(),
                serde_json::json!({ "error": "invalid input" }),
                "{root}"
            );
        }
        // Neither door invented a missing field — the agent door in particular does not
        // report its required `target` against a body that has no fields.
        assert!(!agent_body.contains("target"), "{root}: {agent_body}");
    }

    assert_eq!(unit.calls(), 0);
}

/// THE SAME REQUEST AT BOTH DOORS, mounted. One body, malformed, aimed at a gate that
/// also refuses: the human door answers its gate and the agent door answers the body.
///
/// This is the ordering contract surviving the router. A handler that read and judged the
/// body before dispatching — the obvious way to share work between two routes — would
/// answer both of these the same way, and a person who may not post to a channel would
/// learn how their body was judged there.
#[tokio::test]
async fn one_faulty_request_is_answered_by_whichever_gate_its_own_door_asks_first() {
    let unit = CountingUnitOfWork::default();
    let closed = || {
        mounted(
            Err(ChannelAccessRefusal::Forbidden),
            Err(AddressRefusal::Unresolved),
            Ok(()),
            unit.clone(),
        )
    };
    let malformed = || Body::from(r#"{"content":7}"#);

    let (human_status, human_body) =
        answered(closed(), human_request(HUMAN_PATH, malformed())).await;
    assert_eq!(human_status, StatusCode::FORBIDDEN);
    // The gate's own body, which carries no code: the reference's does not either.
    assert_eq!(
        serde_json::from_str::<Value>(&human_body).unwrap(),
        serde_json::json!({"error": "forbidden"})
    );

    let (agent_status, agent_body) =
        answered(closed(), agent_request(AGENT_PATH, malformed())).await;
    assert_eq!(agent_status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(agent_body.contains("invalid input"), "{agent_body}");
    assert!(!agent_body.contains("forbidden"), "{agent_body}");

    // Neither reached the write unit, which is what makes both answers refusals rather
    // than reports of something that already happened.
    assert_eq!(unit.calls(), 0);
}

/// A body within every bound reaches the WRITE UNIT — through the real router, for both
/// principals — and the refusal it meets there comes back as the refusal it IS.
///
/// The fake write unit refuses with "conversation not found", so this is the whole path in
/// one cell: door, gates, write unit, projection, wire. Without it the refusals above could
/// all be an accident of something else failing, and the projection could be correct in
/// isolation while reaching neither door.
#[tokio::test]
async fn a_well_shaped_body_reaches_the_write_unit_and_its_refusal_reaches_the_caller() {
    let unit = CountingUnitOfWork::default();
    let refused = serde_json::json!({
        "error": "conversation not found",
        "code": "conversation_not_found"
    });

    for (label, request) in both_doors() {
        let (status, body, errors) = observed(open(unit.clone()), request).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{label}");
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            refused,
            "{label}"
        );
        // A REFUSAL IS NOT AN INCIDENT. One line per refused send tells an operator nothing
        // and buries the failures that are ours — and this boundary is where a door that
        // logged its own copy would show up.
        assert!(errors.is_empty(), "{label}: {errors:?}");
    }
    assert_eq!(unit.calls(), 2);
}

/// OUR OWN FAILURE, through both real doors: answered safely, and reported EXACTLY ONCE.
///
/// The unit tests beside the projection prove the split; what they cannot see is a second
/// report added by a route, a middleware or a future adapter. This lane has already made that
/// mistake once, at the ingress fence, so it is asserted at the boundary rather than trusted.
#[tokio::test(flavor = "current_thread")]
async fn our_own_failure_is_answered_safely_and_reported_once_at_either_door() {
    for (label, request) in both_doors() {
        let unit = CountingUnitOfWork::failing(Fails::AsOurFailure);
        let (status, body, errors) = observed(open(unit.clone()), request).await;

        assert_eq!(unit.calls(), 1, "{label}");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{label}");
        assert_eq!(body, "Internal Server Error", "{label}");

        // ONE report, with the whole chain. Not "at least one": a second would leave an
        // operator unable to tell one broken request from two.
        assert_eq!(errors.len(), 1, "{label}: {errors:?}");
        let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
        assert!(
            chain.contains("persistence operation failed"),
            "{label}: {chain}"
        );
        assert!(chain.contains(UNDERLYING_CAUSE), "{label}: {chain}");
    }
}

/// Four principals, one per position, with NO source indentation able to reach the body.
///
/// Written as `concat!` fragments that each begin at column 0. A line-continuation escape
/// looked equivalent and was not: `cargo fmt` reflowed the literal onto one line and kept the
/// alignment spaces it had introduced, so every line after the first arrived with twenty
/// leading spaces — which makes all three of them INDENTED CODE BLOCKS. The cell still passed,
/// because one cause was quietly explaining three exclusions.
const FOUR_POSITIONS: &str = concat!(
    "hi <silverhand://agent/Alice>\n",
    "\n",
    "> quoting <silverhand://human/shiki>\n",
    "\n",
    "`<silverhand://agent/Bob>`\n",
    "\n",
    "[Dana](silverhand://agent/Dana)",
);

/// The same four in PROSE, which is the control: it proves each of them CAN address, so an
/// empty answer below means the position refused it — not that the handle was unparseable, and
/// not that the line had quietly become something else.
const FOUR_IN_PROSE: &str = concat!(
    "hi <silverhand://agent/Alice>\n",
    "\n",
    "quoting <silverhand://human/shiki>\n",
    "\n",
    "<silverhand://agent/Bob>\n",
    "\n",
    "<silverhand://agent/Dana>",
);

fn sent(content: &str) -> Body {
    Body::from(serde_json::json!({ "content": content }).to_string())
}

/// Who a body addresses once it has gone all the way through the real route.
async fn addressed_through_the_route(content: &str) -> Vec<String> {
    let unit = CountingUnitOfWork::default();
    let (status, _) = answered(open(unit.clone()), human_request(HUMAN_PATH, sent(content))).await;

    // It reached the write unit, so what follows is a real derivation and not the silence of a
    // request that never got there.
    assert_eq!(status, StatusCode::NOT_FOUND, "{content:?}");
    assert_eq!(unit.calls(), 1, "{content:?}");
    unit.addressed()
}

/// THE POSITION RULE IS THE PRODUCT'S, not the composition's.
///
/// Asserted on the command the REAL router handed the write unit, so it is the send's own
/// answer rather than the rule's in isolation. Both ways of getting the wiring wrong are
/// caught: a route with a no-op rule addresses nobody, and a permissive one that took every
/// bracketed run would add the quoted and quoted-syntax names. Neither is expressible any more
/// — the route has no position parameter — and this is what would go red if one came back.
#[tokio::test]
async fn only_prose_addresses_and_the_route_decides_that_not_its_composition() {
    // The BYTES, not the source layout. This is the check the earlier version of this cell
    // needed and did not have.
    for line in FOUR_POSITIONS.lines().chain(FOUR_IN_PROSE.lines()) {
        assert!(
            !line.starts_with(' '),
            "source indentation reached the body: {line:?}"
        );
    }

    // THE CONTROL. All four in prose, so all four address, in first-occurrence order.
    assert_eq!(
        addressed_through_the_route(FOUR_IN_PROSE).await,
        ["agent/Alice", "human/shiki", "agent/Bob", "agent/Dana"]
    );

    // THE SUBJECT. The same four, three of them moved into a blockquote, a code span and a
    // label-carrying link — and only the one still in prose addresses.
    assert_eq!(
        addressed_through_the_route(FOUR_POSITIONS).await,
        ["agent/Alice"]
    );

    // And each excluded position on its own, so no single cause can stand in for three.
    for (position, content) in [
        ("blockquote", "> quoting <silverhand://human/shiki>"),
        ("code span", "`<silverhand://agent/Bob>`"),
        ("label link", "[Dana](silverhand://agent/Dana)"),
    ] {
        assert!(
            addressed_through_the_route(content).await.is_empty(),
            "{position} addressed someone"
        );
    }
}

/// EVERY ADDRESS REFUSAL, exactly as an agent receives it.
///
/// One table, because the interesting part is that the statuses DISAGREE and only disagree
/// where the fact does. A token judgement is a 400 and says nothing about what exists; the two
/// existence answers share ONE 404, which is what stops this door from separating unknown from
/// unauthorized by status.
#[tokio::test]
async fn each_address_refusal_reaches_the_agent_in_its_own_words() {
    let cases: Vec<(AddressRefusal, StatusCode, serde_json::Value)> = vec![
        (
            AddressRefusal::Malformed,
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "target is not an address",
                "code": "target_malformed"
            }),
        ),
        (
            AddressRefusal::SelfPair,
            StatusCode::BAD_REQUEST,
            serde_json::json!({
                "error": "a direct message needs another participant",
                "code": "self_pair_not_addressable"
            }),
        ),
        (
            AddressRefusal::Unresolved,
            StatusCode::NOT_FOUND,
            serde_json::json!({
                "error": "target names no stream you can use",
                "code": "target_unresolved"
            }),
        ),
        (
            AddressRefusal::ThreadNotInTarget,
            StatusCode::NOT_FOUND,
            serde_json::json!({
                "error": "target names no reply cluster of that conversation",
                "code": "thread_not_in_target"
            }),
        ),
    ];

    for (refusal, status, body) in cases {
        let unit = CountingUnitOfWork::default();
        let (answered_status, answered_body) = answered(
            mounted(Ok(()), Err(refusal), Ok(()), unit.clone()),
            agent_request(AGENT_PATH, Body::from(agent_send_body(16))),
        )
        .await;

        assert_eq!(answered_status, status, "{refusal:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&answered_body).unwrap(),
            body,
            "{refusal:?}"
        );
        assert_eq!(unit.calls(), 0, "{refusal:?}");
    }
}

/// THE TWO ANSWERS ABOUT THE CREDENTIAL, which are the only 403s this door gives.
///
/// `slot_out_of_scope` says the address was right and the session is anchored elsewhere;
/// `capability_denied` says a human has to grant something. Neither is fixable by trying
/// another address, which is why they are a third status family rather than more 404s.
///
/// The capability body is its OWN shape: no `error` member, and the missing bucket named twice.
/// A codeless 403 degrades to "forbidden" in the CLI and loses the one fact that makes the
/// refusal actionable.
#[tokio::test]
async fn the_credentials_own_refusals_are_the_only_forbidden_answers() {
    // An anchored credential aimed at a stream that is not its anchor.
    let unit = CountingUnitOfWork::default();
    let (status, body) = answered(
        open(unit.clone()),
        anchored_request(ConversationId::parse("conv-9").unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        serde_json::json!({
            "error": "this operation acts only in the conversation your session is anchored to",
            "code": "slot_out_of_scope"
        })
    );
    assert_eq!(unit.calls(), 0);

    let unit = CountingUnitOfWork::default();
    let (status, body) = answered(
        mounted(Ok(()), Ok(()), Err(CapabilityDenied), unit.clone()),
        agent_request(AGENT_PATH, Body::from(agent_send_body(16))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        serde_json::json!({
            "code": "capability_denied",
            "capability": "send_messages",
            "message": "missing 'send_messages' capability in this workspace",
            "details": { "capability": "send_messages" }
        })
    );
    assert_eq!(unit.calls(), 0);
}

/// REACH DEPENDS ON WHICH CREDENTIAL IT IS, and the two variants answer differently for the
/// same address.
///
/// An anchored session may act only where it is anchored; an unanchored one may act wherever
/// the resolver says the agent participates. A single principal shape could not express both —
/// it would have to force an anchor on the unanchored one and then either refuse every send it
/// makes or let every anchored session post anywhere.
#[tokio::test]
async fn an_anchored_credential_reaches_only_its_own_stream() {
    // Anchored TO the stream the address resolves to: through.
    let unit = CountingUnitOfWork::default();
    let (status, _) = answered(open(unit.clone()), anchored_request(resolved_stream())).await;
    assert_eq!(unit.calls(), 1);
    assert_eq!(status, StatusCode::NOT_FOUND, "it reached the write unit");

    // Anchored elsewhere: refused, and the write unit never sees it.
    let unit = CountingUnitOfWork::default();
    let (status, body) = answered(
        open(unit.clone()),
        anchored_request(ConversationId::parse("conv-9").unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("slot_out_of_scope"), "{body}");
    assert_eq!(unit.calls(), 0);

    // Unanchored, same address: through.
    let unit = CountingUnitOfWork::default();
    let _ = answered(
        open(unit.clone()),
        agent_request(AGENT_PATH, Body::from(agent_send_body(16))),
    )
    .await;
    assert_eq!(unit.calls(), 1);
}

/// THE ORDER OF THE FOUR AUTHORITIES, held by crosses rather than by types.
///
/// Nothing in the code makes `validate → resolve → reach → capability` fail to compile in
/// another order: the failure types differ, and a swapped pair still type-checks.
///
/// And a response alone is not enough to hold it. An implementation that asked the capability
/// FIRST and still returned the address or reach refusal would answer every cell below
/// identically — it would have turned an evaluation order into an answer priority, while a real
/// capability read hit a roster for requests that were already doomed. So each case pins HOW
/// MANY TIMES each gate was asked, and the counts are what go red.
#[tokio::test]
async fn a_request_that_fails_two_gates_is_answered_by_the_earlier_one() {
    /// What one request cost: the answer, and how far down the chain it got.
    struct Asked {
        status: StatusCode,
        body: String,
        address: usize,
        capability: usize,
        committed: usize,
    }

    async fn ask(
        request: Request<Body>,
        address: Result<(), AddressRefusal>,
        grant: Result<(), CapabilityDenied>,
    ) -> Asked {
        let unit = CountingUnitOfWork::default();
        let (router, asked_address, asked_capability) =
            watched(Ok(()), address, grant, unit.clone());
        let (status, body) = answered(router, request).await;
        Asked {
            status,
            body,
            address: asked_address.count(),
            capability: asked_capability.count(),
            committed: unit.calls(),
        }
    }

    let elsewhere = || ConversationId::parse("conv-9").unwrap();

    // A malformed BODY, with both later gates also refusing: admission answers, and NEITHER
    // gate is asked. This door validates before it looks at where the request aimed.
    let payload = ask(
        agent_request(AGENT_PATH, Body::from(r#"{"content":7,"target":"conv-2"}"#)),
        Err(AddressRefusal::Unresolved),
        Err(CapabilityDenied),
    )
    .await;
    assert_eq!(payload.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(payload.body.contains("invalid input"), "{}", payload.body);
    assert_eq!(
        (payload.address, payload.capability, payload.committed),
        (0, 0, 0)
    );

    // The ADDRESS refuses and the grant would too: the address answers, and the grant is never
    // asked about a stream that does not resolve.
    let address = ask(
        agent_request(AGENT_PATH, Body::from(agent_send_body(16))),
        Err(AddressRefusal::Unresolved),
        Err(CapabilityDenied),
    )
    .await;
    assert_eq!(address.status, StatusCode::NOT_FOUND);
    assert!(
        address.body.contains("target_unresolved"),
        "{}",
        address.body
    );
    assert!(!address.body.contains("capability"), "{}", address.body);
    assert_eq!(
        (address.address, address.capability, address.committed),
        (1, 0, 0)
    );

    // REACH refuses and the grant would too: reach answers, and the grant is never asked about
    // a stream this session may not act on.
    let reach = ask(anchored_request(elsewhere()), Ok(()), Err(CapabilityDenied)).await;
    assert_eq!(reach.status, StatusCode::FORBIDDEN);
    assert!(reach.body.contains("slot_out_of_scope"), "{}", reach.body);
    assert!(!reach.body.contains("capability"), "{}", reach.body);
    assert_eq!(
        (reach.address, reach.capability, reach.committed),
        (1, 0, 0)
    );

    // Only the GRANT refuses: it is asked, exactly once, and nothing is committed.
    let grant = ask(
        agent_request(AGENT_PATH, Body::from(agent_send_body(16))),
        Ok(()),
        Err(CapabilityDenied),
    )
    .await;
    assert_eq!(grant.status, StatusCode::FORBIDDEN);
    assert!(grant.body.contains("capability_denied"), "{}", grant.body);
    assert_eq!(
        (grant.address, grant.capability, grant.committed),
        (1, 1, 0)
    );

    // Everything open: each gate asked once, and the send reaches the write unit.
    let through = ask(
        agent_request(AGENT_PATH, Body::from(agent_send_body(16))),
        Ok(()),
        Ok(()),
    )
    .await;
    assert_eq!(
        (through.address, through.capability, through.committed),
        (1, 1, 1)
    );
}

/// The two doors keep their own vocabularies. The human door's gate answers carry NO code,
/// because the reference draws no distinction there — and acquiring the agent door's codes
/// would publish a contract its clients never agreed to.
#[tokio::test]
async fn the_human_gate_does_not_borrow_the_agent_doors_codes() {
    for (refusal, status, error) in [
        (
            ChannelAccessRefusal::NotFound,
            StatusCode::NOT_FOUND,
            "channel not found",
        ),
        (
            ChannelAccessRefusal::Forbidden,
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
    ] {
        let unit = CountingUnitOfWork::default();
        let (answered_status, body) = answered(
            mounted(Err(refusal), Ok(()), Ok(()), unit.clone()),
            human_request(HUMAN_PATH, Body::from(human_send_body(16))),
        )
        .await;

        assert_eq!(answered_status, status, "{refusal:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            serde_json::json!({ "error": error }),
            "{refusal:?}"
        );
        assert_eq!(unit.calls(), 0, "{refusal:?}");
    }
}

/// One legal request per door, so a cell that is about the OUTCOME does not restate how a
/// request is shaped.
fn both_doors() -> Vec<(&'static str, Request<Body>)> {
    vec![
        (
            "human",
            human_request(HUMAN_PATH, Body::from(human_send_body(16))),
        ),
        (
            "agent",
            agent_request(AGENT_PATH, Body::from(agent_send_body(16))),
        ),
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn an_unreadable_body_is_reported_exactly_once_and_never_to_the_caller() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let unit = CountingUnitOfWork::default();
    let (status, body) = answered(
        open(unit.clone()),
        agent_request(AGENT_PATH, breaks_after_one_frame()),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_ne!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_ne!(status, StatusCode::BAD_REQUEST);
    assert_eq!(unit.calls(), 0);

    let events = logs.events();
    let errors = events
        .iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    // ONE report. Not "at least one": a second would mean an operator reads the same
    // failure twice and cannot tell one broken request from two.
    assert_eq!(errors.len(), 1, "{events:?}");
    assert_eq!(
        errors[0]["fields"]["message"],
        "message-send request body could not be read"
    );
    assert!(
        errors[0]["fields"]["error_chain"]
            .as_str()
            .is_some_and(|chain| chain.contains(FRAME_CAUSE)),
        "{:?}",
        errors[0]["fields"]["error_chain"]
    );

    // The operator has the cause; the caller has none of it.
    assert!(!body.contains(FRAME_CAUSE), "{body}");
    assert!(!body.contains("content"), "{body}");
    assert!(!body.contains("invalid input"), "{body}");
    assert!(!body.contains("message-send bound"), "{body}");
}

/// A route reached without the principal its composition owes it refuses, and tells the
/// caller nothing about why. The alternative — an anonymous or default identity — would
/// attribute a send to someone who did not write it.
///
/// This is also what a router mounted without its authentication middleware looks like,
/// which is the state this slice is deliberately in.
#[tokio::test(flavor = "current_thread")]
async fn a_missing_principal_refuses_rather_than_inventing_one() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let unit = CountingUnitOfWork::default();
    for path in [HUMAN_PATH, AGENT_PATH] {
        let (status, body) = answered(
            open(unit.clone()),
            Request::post(path)
                .body(Body::from(r#"{"content":"hi","target":"conv-2"}"#))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        assert!(!body.contains("principal"), "{path}: {body}");
    }

    assert_eq!(unit.calls(), 0);
    assert_eq!(
        logs.events()
            .iter()
            .filter(|event| event["level"] == "ERROR")
            .count(),
        2
    );
}
