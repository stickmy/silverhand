//! Runtime activity projection onto the scope-event wire, keyed by runtime scope.

use serde_json::{Value, json};
use silverhand_backend_core::host_runtime::runtime_event::RuntimeActivityPayload;

use crate::{
    foundation::{scope_event_transport::ScopeEventHub, scope_event_wire::ProductEvent},
    host_runtime::runtime_event_settlement::{
        ConversationRuntimeProjectionFact, ConversationRuntimeProjectionPort, LiveProjectionFailure,
    },
};
use silverhand_backend_core::foundation::scope_event::EventScope;

#[derive(Clone)]
pub struct HubRuntimeActivityPublisher(ScopeEventHub);

impl ScopeEventHub {
    pub fn runtime_activity_publisher(&self) -> HubRuntimeActivityPublisher {
        HubRuntimeActivityPublisher(self.clone())
    }
}

impl ConversationRuntimeProjectionPort for HubRuntimeActivityPublisher {
    fn publish(
        &self,
        fact: ConversationRuntimeProjectionFact,
    ) -> Result<(), LiveProjectionFailure> {
        let agent_scope = match &fact {
            ConversationRuntimeProjectionFact::AgentEvent { scope, .. }
            | ConversationRuntimeProjectionFact::Activity { scope, .. }
            | ConversationRuntimeProjectionFact::RefinedToolCall { scope, .. } => match scope {
                silverhand_backend_core::host_runtime::runtime_credential::RuntimeScope::AgentSlot { agent } => Some(EventScope::Agent(agent.clone())),
                _ => None,
            },
            _ => None,
        };
        let (conversation, event) = match fact {
            ConversationRuntimeProjectionFact::AgentActivity {
                conversation,
                state,
            } => {
                let payload =
                    serde_json::to_value(state).map_err(LiveProjectionFailure::from_source)?;
                (conversation, ProductEvent::AgentActivity(payload))
            }
            ConversationRuntimeProjectionFact::AgentInitTimeout {
                conversation,
                agent,
                reason,
                elapsed_ms,
                timeout_ms,
                retrying,
                retry_attempt,
                max_retries,
                cooldown_until,
            } => (
                conversation,
                ProductEvent::AgentInitTimeout(json!({
                    "agent_id": agent.as_str(),
                    "reason": reason.as_str(),
                    "elapsed_ms": elapsed_ms,
                    "timeout_ms": timeout_ms,
                    "retrying": retrying,
                    "retry_attempt": retry_attempt,
                    "max_retries": max_retries,
                    "cooldown_until": cooldown_until,
                })),
            ),
            ConversationRuntimeProjectionFact::CompactionStale {
                conversation,
                agent,
                elapsed_ms,
            } => (
                conversation,
                ProductEvent::CompactionStale(json!({
                    "agent_id": agent.as_str(),
                    "elapsed_ms": elapsed_ms,
                })),
            ),
            ConversationRuntimeProjectionFact::AgentEvent {
                scope: _,
                conversation,
                agent,
                event,
            } => (
                conversation,
                ProductEvent::AgentEvent(json!({
                    "agent_id": agent.as_str(),
                    "event": event,
                })),
            ),
            ConversationRuntimeProjectionFact::Activity { id, fact, .. } => {
                let conversation = fact.conversation().clone();
                let payload = activity_payload_value(fact.payload())?;
                (
                    conversation,
                    ProductEvent::ConversationActivity(json!({
                        "id": id,
                        "conversation_id": fact.conversation().as_str(),
                        "agent_id": fact.agent().as_str(),
                        "kind": fact.kind().as_str(),
                        "payload": payload,
                        "created_at": fact.created_at().as_unix_millis(),
                    })),
                )
            }
            ConversationRuntimeProjectionFact::RefinedToolCall {
                scope: _,
                conversation,
                agent,
                activity,
            } => {
                let payload = serde_json::from_str::<Value>(activity.payload_json())
                    .map_err(LiveProjectionFailure::from_source)?;
                (
                    conversation.clone(),
                    ProductEvent::ConversationActivity(json!({
                        "id": activity.id(),
                        "conversation_id": conversation.as_str(),
                        "agent_id": agent.as_str(),
                        "kind": "tool_call",
                        "payload": payload,
                        "created_at": activity.created_at().as_unix_millis(),
                    })),
                )
            }
        };
        let event = match (agent_scope.is_some(), event) {
            (true, ProductEvent::ConversationActivity(mut payload)) => {
                if let Some(object) = payload.as_object_mut() {
                    object.remove("conversation_id");
                }
                ProductEvent::AgentStreamActivity(payload)
            }
            (_, event) => event,
        };
        self.0
            .publish(
                agent_scope.unwrap_or(EventScope::ConversationSide(conversation)),
                event,
            )
            .map_err(LiveProjectionFailure::from_source)
    }
}

fn activity_payload_value(
    payload: &RuntimeActivityPayload,
) -> Result<Value, LiveProjectionFailure> {
    Ok(match payload {
        RuntimeActivityPayload::None => Value::Null,
        RuntimeActivityPayload::Text { text } => json!({ "text": text }),
        RuntimeActivityPayload::ToolCall {
            name,
            input_json,
            tool_json,
            tool_call_id,
        } => {
            let mut value = json!({
                "name": name,
                "input": serde_json::from_str::<Value>(input_json)
                    .map_err(LiveProjectionFailure::from_source)?,
                "tool_call_id": tool_call_id,
            });
            if let Some(tool_json) = tool_json {
                value["tool"] = serde_json::from_str::<Value>(tool_json)
                    .map_err(LiveProjectionFailure::from_source)?;
            }
            value
        }
        RuntimeActivityPayload::ToolOutput {
            name,
            tool_call_id,
            outcome,
        } => json!({
            "name": name, "tool_call_id": tool_call_id, "outcome": outcome,
        }),
        RuntimeActivityPayload::ApiResponse { status, message } => {
            json!({ "status": status, "message": message })
        }
        RuntimeActivityPayload::Error {
            message,
            code,
            fatal,
        } => json!({ "message": message, "code": code, "fatal": fatal }),
        RuntimeActivityPayload::ParsedEventDropped {
            parsed_kind,
            encoded_bytes,
            reason,
        } => json!({
            "parsed_kind": parsed_kind, "encoded_bytes": encoded_bytes, "reason": reason,
        }),
        RuntimeActivityPayload::MessageReceived { count } => json!({ "count": count }),
    })
}
