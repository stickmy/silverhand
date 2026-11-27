//! Identity/access transport adapters and agent-owned profile capabilities.
//!
//! Access governance, identity/session authentication, profile and avatar custody, shared wire
//! projection, and workspace membership/metadata remain independent leaves; complete production
//! colocation here does not merge their contracts or responsibilities.
//! The runtime self/peer profile HTTP faces share one private profile-wire projector; multipart
//! write admission remains a separate private wire leaf beside the self-profile write handler.

pub mod access;
pub mod account_deletion;
pub mod agent_audit;
pub mod agent_avatar;
pub mod agent_avatar_write;
pub mod agent_detail;
pub mod agent_env;
pub mod agent_list;
pub mod agent_list_workspaces;
pub mod agent_name_availability;
pub mod agent_patch;
pub mod agent_profile_mcp_data_plane;
mod agent_profile_wire;
pub(crate) mod agent_wire;
pub mod agent_workspace_index_probe;
pub mod auth;
pub mod avatar_blob;
pub mod avatar_design;
pub(crate) mod daemon_profile_mcp_codec;
pub mod embedded_profile;
pub mod human_auth;
pub mod password_hasher;
pub mod peer_profile;
pub mod public_profile_mcp;
pub mod related_users;
pub mod scope_event_publishers;
pub mod self_profile;
pub mod self_profile_mcp;
pub mod self_profile_write;
mod self_profile_write_admission;
pub mod users;
pub mod workspace_creation;
pub mod workspace_deletion;
pub mod workspace_detail;
pub mod workspace_metadata_write;
pub mod workspace_name_availability;
pub mod workspaces;

pub mod profile_message_counts;

pub mod password_change;
pub mod password_recovery;
