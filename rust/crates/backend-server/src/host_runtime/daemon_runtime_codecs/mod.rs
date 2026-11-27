//! Runtime lifecycle command codecs owned by the host-runtime context.

use serde_json::{Value, json};
use silverhand_backend_core::{
    conversation_messaging::runtime_delivery::{
        RuntimeMember, RuntimePlacement, RuntimeRoster, RuntimeSpawnPlan,
    },
    foundation::identity::AgentId,
    host_runtime::runtime_credential::RuntimeScope,
};

use crate::foundation::daemon_transport::DaemonFeatures;

pub(crate) mod runtime_clear_session;
pub(crate) mod runtime_start;
pub(crate) mod runtime_stop;

pub(crate) fn spawn_options(
    plan: &RuntimeSpawnPlan,
    token: &str,
    features: DaemonFeatures,
) -> Value {
    let mut driver = json!({
        "kind": plan.agent.runtime.as_str(),
        "cli_session_id": plan.agent.cli_session_id,
        "model": plan.agent.model,
        "reasoning_effort": plan.agent.reasoning_effort,
    });
    // An older daemon strict-decodes the driver options and requires `fast_mode`; its
    // flag never selected a tier, so it is sent as null.
    if features.service_tier {
        driver["service_tier"] = json!(plan.agent.service_tier);
    } else {
        driver["fast_mode"] = Value::Null;
    }
    let mut env_policy = json!({ "silverhand": { "auth_token": token } });
    if !plan.agent.env_vars.is_empty() {
        env_policy["user_env"] = Value::Array(
            plan.agent
                .env_vars
                .iter()
                .map(|entry| json!({ "name": entry.name, "value": entry.value }))
                .collect(),
        );
    }
    let common = json!({
        "agent": { "id": plan.agent.id.as_str(), "name": plan.agent.name },
        "driver": driver,
        "env_policy": env_policy,
    });
    let mut object = common.as_object().cloned().expect("object literal");
    object.insert("runtime_scope".to_owned(), scope_wire(&plan.scope));
    if let Some(session) = &plan.session {
        object.insert(
            "workspace".to_owned(),
            plan.workspace.as_ref().map_or(Value::Null, workspace_wire),
        );
        object.insert(
            "workspace_memory_index".to_owned(),
            Value::Array(
                plan.workspace_memory_index
                    .iter()
                    .map(workspace_wire)
                    .collect(),
            ),
        );
        object.insert("session".to_owned(), session_wire(session));
    }
    Value::Object(object)
}

pub(crate) fn scope_wire(scope: &RuntimeScope) -> Value {
    match scope {
        RuntimeScope::ConversationSlot { agent, source } => json!({
            "kind": "conversation", "conversation_id": source.as_str(), "agent_id": agent.as_str()
        }),
        RuntimeScope::AgentSlot { agent } => json!({ "kind": "agent", "agent_id": agent.as_str() }),
    }
}

pub(crate) fn workspace_wire(
    workspace: &silverhand_backend_core::conversation_messaging::runtime_delivery::RuntimeWorkspaceRef,
) -> Value {
    json!({ "id": workspace.id().as_str(), "name": workspace.name() })
}

fn session_wire(
    session: &silverhand_backend_core::conversation_messaging::runtime_delivery::RuntimeSessionSpec,
) -> Value {
    let members = session
        .members
        .iter()
        .map(|member| match member {
            RuntimeMember::Agent {
                id,
                name,
                display_name,
                runtime,
            } => json!({
                "kind": "agent", "agent_id": id.as_str(), "name": name,
                "display_name": display_name, "runtime": runtime.as_str(),
            }),
            RuntimeMember::Human {
                id,
                name,
                display_name,
            } => json!({
                "kind": "human", "user_id": id.as_str(), "name": name,
                "display_name": display_name, "avatar_url": Value::Null,
            }),
        })
        .collect::<Vec<_>>();
    json!({
        "conversation_id": session.conversation.as_str(),
        "placement_kind": match session.placement { RuntimePlacement::Workspace => "workspace", RuntimePlacement::DirectPair => "direct_pair" },
        "roster_policy": match session.roster { RuntimeRoster::Open => "open", RuntimeRoster::Fixed => "fixed" },
        "title": session.title,
        "spawned_by_agent_id": session.spawned_by_agent.as_ref().map(AgentId::as_str),
        "members": members,
    })
}

#[cfg(test)]
mod tests {
    use silverhand_backend_core::{
        conversation_messaging::runtime_delivery::RuntimeAgentSpec,
        identity_access::agents::runtime::RuntimeKind,
    };

    use super::*;

    #[test]
    fn the_service_tier_key_follows_the_daemon_capability() {
        let agent = AgentId::parse("agent-a").unwrap();
        let plan = RuntimeSpawnPlan {
            scope: RuntimeScope::AgentSlot {
                agent: agent.clone(),
            },
            agent: RuntimeAgentSpec {
                id: agent,
                name: "agent-a".to_owned(),
                runtime: RuntimeKind::Codex,
                model: None,
                reasoning_effort: None,
                service_tier: Some("ultrafast".to_owned()),
                cli_session_id: None,
                env_vars: Vec::new(),
            },
            workspace: None,
            workspace_memory_index: Vec::new(),
            session: None,
        };
        let current = DaemonFeatures {
            service_tier: true,
            ..DaemonFeatures::default()
        };
        let driver = &spawn_options(&plan, "t", current)["driver"];
        assert_eq!(driver["service_tier"], "ultrafast");
        assert!(driver.get("fast_mode").is_none());

        let driver = &spawn_options(&plan, "t", DaemonFeatures::default())["driver"];
        assert_eq!(driver["fast_mode"], Value::Null);
        assert!(driver.get("service_tier").is_none());
    }

    #[test]
    fn user_env_is_emitted_under_env_policy_only_when_non_empty() {
        let agent = AgentId::parse("agent-a").unwrap();
        let mut plan = RuntimeSpawnPlan {
            scope: RuntimeScope::AgentSlot {
                agent: agent.clone(),
            },
            agent: RuntimeAgentSpec {
                id: agent,
                name: "agent-a".to_owned(),
                runtime: RuntimeKind::Antigravity,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                cli_session_id: None,
                env_vars: Vec::new(),
            },
            workspace: None,
            workspace_memory_index: Vec::new(),
            session: None,
        };
        let empty = spawn_options(&plan, "t", DaemonFeatures::default());
        assert!(empty["env_policy"].get("user_env").is_none());

        plan.agent.env_vars = vec![
            silverhand_backend_core::identity_access::agent_env::AgentEnvVar {
                name: "SILVERHAND_ACP_AUTO_COMPACT".to_owned(),
                value: "200000".to_owned(),
            },
        ];
        let populated = spawn_options(&plan, "t", DaemonFeatures::default());
        assert_eq!(
            populated["env_policy"]["user_env"],
            json!([{ "name": "SILVERHAND_ACP_AUTO_COMPACT", "value": "200000" }])
        );
    }
}
