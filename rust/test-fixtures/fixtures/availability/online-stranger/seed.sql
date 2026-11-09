INSERT INTO users (id, name, display_name, created_at, updated_at)
VALUES ('u-creator', 'creator', 'Creator', 1000, 1000);
-- A registered human in NO visibility arm of the agent: availability is public.
INSERT INTO users (id, name, display_name, created_at, updated_at)
VALUES ('u-stranger', 'stranger', 'Stranger', 1000, 1000);

INSERT INTO hosts (id, name, kind, created_at)
VALUES ('h1', 'host-one', 'desktop', 1000);

INSERT INTO agents (id, name, runtime, host_id, created_at, updated_at, created_by_user_id)
VALUES ('a1', 'agent-one', 'qoder', 'h1', 1000, 1000, 'u-creator');
