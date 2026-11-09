INSERT INTO users (id, name, display_name, created_at, updated_at)
VALUES ('u-creator', 'creator', 'Creator', 1000, 1000);
INSERT INTO users (id, name, display_name, created_at, updated_at)
VALUES ('u-pair', 'pair', 'Pair Human', 1000, 1000);

INSERT INTO hosts (id, name, kind, created_at)
VALUES ('h1', 'host-one', 'desktop', 1000);

INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id)
VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator');

-- A human<->agent direct pair whose human endpoint is the non-creator viewer.
-- The arc-ordering CHECK forces the agent into slot A ('agent' < 'human'), so the
-- human endpoint is slot B, which the adapter's first UNION arm reads.
INSERT INTO conversations
  (id, workspace_id, endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id,
   placement_kind, roster_policy, title, created_at, updated_at)
VALUES
  ('c-pair', NULL, 'agent', 'a1', 'human', 'u-pair',
   'direct_pair', 'fixed', 'dm', 1000, 1000);
