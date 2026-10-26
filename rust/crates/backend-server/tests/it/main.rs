//! The crate's integration tests, linked as one binary. Suites that install a global
//! tracing subscriber or depend on span callsite interest stay as their own binaries.

#[path = "../../../backend-sqlite/tests/support/current_schema_snapshot.rs"]
mod current_schema;
#[path = "../support/log_capture.rs"]
mod log_capture;
#[path = "../support/no_aftermath.rs"]
mod no_aftermath;

mod access_http_contract;
mod agent_detail_contract;
mod agent_list_contract;
mod agent_list_mount;
mod agent_runtime_wire_vocabulary;
mod architecture_ownership;
mod attention_preference_e2e;
mod attention_projection_e2e;
mod availability_parity;
mod conversation_message_window_router;
mod conversation_threads_router;
mod daemon_delivery_e2e;
mod daemon_delivery_typed_plane;
mod direct_pair_read_e2e;
mod embedded_profile_setup;
mod host_agent_cold_read_e2e;
mod host_agent_scaffold;
mod host_resources_contract;
mod host_runtime_e2e;
mod human_auth_e2e;
mod human_search_router;
mod human_session_e2e;
mod message_unread;
mod messaging_send_door;
mod password_recovery_e2e;
mod presence_feed_clock;
mod presence_parity;
mod profile_message_counts;
mod runtime_credential_e2e;
mod scope_event_e2e;
mod send_ingress_contract;
mod stale_message_resolution_mount;
mod task_list_e2e;
mod task_list_mount;
mod users_avatar_e2e;
mod vocabulary_ownership;
mod workspace_cold_read_e2e;
mod workspace_detail_parity;
mod workspace_metadata_write_e2e;
mod workspace_name_availability_parity;
