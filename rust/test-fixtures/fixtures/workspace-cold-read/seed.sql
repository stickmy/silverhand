-- Shared cross-language fixture for the GET /api/workspaces cold-read envelope
-- (#task:2190). Executed VERBATIM against a fresh current-schema database by both
-- the TypeScript parity test (tests/workspace-cold-read-parity.test.ts) and the
-- Rust parity test (backend-server/tests/workspace_cold_read_parity.rs), which then
-- assert their real projector's output equals fixtures/.../expected-envelope.json.
--
-- The seed is raw SQL on purpose: it is ONE contract both languages replay, and it
-- constructs states no repository writes (a workspace the viewer is NOT a member of;
-- sort-entry orphans for archived / foreign workspaces). Timestamps are mutually
-- unequal so the published two-key order is unambiguous without an id tie-break.
-- foreign_keys are ON, so principals precede the rows that reference them.

-- Principals.
INSERT INTO users (id, name, display_name, created_at, updated_at)
  VALUES ('user-1', 'viewer', 'Viewer', 1, 1);
INSERT INTO users (id, name, display_name, created_at, updated_at)
  VALUES ('user-2', 'owner', 'Owner', 2, 2);
INSERT INTO hosts (id, name, kind, created_at)
  VALUES ('host-1', 'Host', 'local', 1);
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at)
  VALUES ('agent-1', 'alice', 'Alice', 'claude', 'host-1', 'user-1', 1, 1);

-- Regular workspaces (owner_agent_id NULL => name_key NOT NULL) and one agent-home
-- workspace (owner_agent_id NOT NULL => name_key NULL). updated_at is 400/300/200.
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at)
  VALUES ('ws-archived', 'Archived', 'archived', 'gone', NULL, 'user-1', 999, 1, 400);
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at)
  VALUES ('ws-regular', 'Regular', 'regular', NULL, NULL, 'user-1', NULL, 1, 300);
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at)
  VALUES ('ws-agent', 'Agent Home', NULL, NULL, 'agent-1', NULL, NULL, 1, 200);
-- A workspace the viewer is not a member of must never leak onto the face.
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at)
  VALUES ('ws-foreign', 'Foreign', 'foreign', NULL, NULL, 'user-2', NULL, 1, 500);

-- Membership: everything but ws-foreign.
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
  VALUES ('ws-archived', 'user-1', 1, 'owner');
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
  VALUES ('ws-regular', 'user-1', 1, 'owner');
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
  VALUES ('ws-agent', 'user-1', 1, 'owner');

-- Fixed-roster DM channels for the agent-home workspace, priority Onboarding then dm-<name>.
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, created_at, updated_at)
  VALUES ('conv-dm', 'ws-agent', 'workspace', 'fixed', 'dm-alice', 'dm-alice', 20, 20);
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, created_at, updated_at)
  VALUES ('conv-onboarding', 'ws-agent', 'workspace', 'fixed', 'Onboarding', 'onboarding', 10, 10);
-- An open-roster channel is not a DM channel and must be excluded.
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, created_at, updated_at)
  VALUES ('conv-open', 'ws-agent', 'workspace', 'open', 'general', 'general', 5, 5);

-- Viewer sort preferences for the workspace rail (both hard-group scopes present).
INSERT INTO user_list_sort_mode (user_id, list_key, scope, mode, updated_at)
  VALUES ('user-1', 'workspaces', 'regular', 'recent', 1);
INSERT INTO user_list_sort_mode (user_id, list_key, scope, mode, updated_at)
  VALUES ('user-1', 'workspaces', 'agent_space', 'manual', 1);

-- Sort entries: two visible-and-live (kept), one archived and one foreign (both
-- filtered off the wire). Insertion order is the wire order both readers preserve.
INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at, starred_at)
  VALUES ('user-1', 'workspaces', 'ws-regular', 1, 50, NULL);
INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at, starred_at)
  VALUES ('user-1', 'workspaces', 'ws-agent', 2, NULL, 70);
INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at, starred_at)
  VALUES ('user-1', 'workspaces', 'ws-archived', 0, NULL, NULL);
INSERT INTO user_sort_entry (user_id, list_key, entity_id, rank, last_accessed_at, starred_at)
  VALUES ('user-1', 'workspaces', 'ws-foreign', 9, NULL, NULL);
