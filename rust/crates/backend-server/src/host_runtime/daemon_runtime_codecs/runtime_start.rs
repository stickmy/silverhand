//! Transport-owned codec for `runtime_start`.

use std::{error::Error, fmt};

use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::runtime_delivery::RuntimeDispatchTarget,
    foundation::wire_number::JsSafeInteger,
};

use crate::foundation::daemon_transport::DaemonFeatures;
use crate::host_runtime::runtime_credential::RuntimeStartHandoff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeStartCodecError;

impl fmt::Display for RuntimeStartCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime_start target is outside the command schema")
    }
}

impl Error for RuntimeStartCodecError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeStartResult {
    Started,
    InitCooldown,
}

pub(crate) fn encode(
    target: &RuntimeDispatchTarget,
    handoff: &RuntimeStartHandoff,
    features: DaemonFeatures,
) -> Result<Value, RuntimeStartCodecError> {
    let principal = handoff.principal();
    let generation = JsSafeInteger::new(principal.custody().generation().as_i64())
        .map_err(|_| RuntimeStartCodecError)?;
    if generation.get() <= 0
        || principal.agent() != target.plan.scope.agent()
        || principal.scope() != target.plan.scope
        || target
            .plan
            .scope
            .source()
            .is_some_and(|source| source != &target.conversation)
    {
        return Err(RuntimeStartCodecError);
    }
    Ok(json!({
        "conversation_id": target.conversation.as_str(),
        "agent_id": principal.agent().as_str(),
        "runtime_generation": generation.get(),
        "options": super::spawn_options(&target.plan, handoff.daemon_auth_token(), features),
    }))
}

pub(crate) fn decode_result(value: &Value) -> Option<RuntimeStartResult> {
    let object = value.as_object()?;
    match object.get("started").and_then(Value::as_bool)? {
        true if object.len() == 3 => {
            JsSafeInteger::new(object.get("pid")?.as_i64()?).ok()?;
            JsSafeInteger::new(object.get("started_at")?.as_i64()?).ok()?;
            Some(RuntimeStartResult::Started)
        }
        false
            if object.len() == 3
                && object.get("reason").and_then(Value::as_str) == Some("init_cooldown") =>
        {
            let cooldown = JsSafeInteger::new(object.get("cooldown_until")?.as_i64()?).ok()?;
            (cooldown.get() > 0).then_some(RuntimeStartResult::InitCooldown)
        }
        _ => None,
    }
}
