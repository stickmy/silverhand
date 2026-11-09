-- Legacy contract fixture for GET /api/workspaces/:id (#task:2193).
-- Executed against a fresh current-schema database by the Rust HTTP/SQLite test
-- (backend-server/tests/workspace_detail_parity.rs), which replays cases.json:
-- given viewer user-1, each id must yield the same HTTP status and body.
-- foreign_keys are ON, so principals precede the rows that reference them.

INSERT INTO users (id, name, display_name, created_at, updated_at)
  VALUES ('user-1', 'viewer', 'Viewer', 1, 1);
INSERT INTO users (id, name, display_name, created_at, updated_at)
  VALUES ('user-2', 'other', 'Other', 2, 2);
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1);
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at)
  VALUES ('agent-1', 'alice', 'Alice', 'claude', 'host-1', 'user-1', 1, 1);

-- A regular workspace the viewer owns and belongs to: visible face at 200.
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at)
  VALUES ('ws-regular', 'Regular', 'regular', 'desc', NULL, 'user-1', NULL, 1, 300);
-- A regular workspace only user-2 belongs to: present, so the viewer gets 403
-- (existence resolved before visibility), observably distinct from the 404 absence.
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at)
  VALUES ('ws-foreign', 'Foreign', 'foreign', NULL, NULL, 'user-2', NULL, 1, 500);
-- An agent-home workspace the viewer belongs to: GET :id does not filter it, so a
-- member sees the same DM derivation the list read publishes.
INSERT INTO workspaces (id, name, name_key, description, owner_agent_id, owner_user_id, archived_at, created_at, updated_at)
  VALUES ('ws-home', 'Agent Home', NULL, NULL, 'agent-1', NULL, NULL, 1, 200);

INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
  VALUES ('ws-regular', 'user-1', 1, 'owner');
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
  VALUES ('ws-foreign', 'user-2', 1, 'owner');
INSERT INTO workspace_users (workspace_id, user_id, joined_at, role)
  VALUES ('ws-home', 'user-1', 1, 'owner');

-- Two fixed-roster channels in the agent-home workspace drive the DM derivation:
-- Onboarding sorts first, then dm-<owner name>.
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, created_at, updated_at)
  VALUES ('conv-onboarding', 'ws-home', 'workspace', 'fixed', 'Onboarding', 'onboarding', 10, 10);
INSERT INTO conversations (id, workspace_id, placement_kind, roster_policy, title, title_key, created_at, updated_at)
  VALUES ('conv-dm', 'ws-home', 'workspace', 'fixed', 'dm-alice', 'dm-alice', 20, 20);
