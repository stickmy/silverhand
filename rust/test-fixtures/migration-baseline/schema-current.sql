CREATE TABLE hosts (
        id          TEXT PRIMARY KEY,
        name        TEXT NOT NULL UNIQUE,
        kind        TEXT NOT NULL,
        created_at  INTEGER NOT NULL
      , user_id TEXT REFERENCES users(id) ON DELETE CASCADE, installation_id TEXT, last_seen_at INTEGER, revoked_at INTEGER, display_label TEXT, activated_at INTEGER, provisioning_source TEXT NOT NULL DEFAULT 'system'
        CHECK (provisioning_source IN ('app_install', 'user_invite', 'system')), finalized_at INTEGER);

CREATE TABLE app_settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );

CREATE TABLE workspace_members (
        workspace_id  TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        agent_id      TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        joined_at     INTEGER NOT NULL,
        PRIMARY KEY (workspace_id, agent_id)
      );

CREATE INDEX idx_workspace_members_agent ON workspace_members(agent_id);

CREATE TABLE conversation_inboxes (
        id               INTEGER PRIMARY KEY,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        agent_id         TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        message_id       TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        delivered_at     INTEGER,
        created_at       INTEGER NOT NULL
      , in_flight_owner TEXT, in_flight_at INTEGER, failed_at INTEGER, failed_reason TEXT, delivery_kind TEXT);

CREATE INDEX idx_conversation_inbox_pending
        ON conversation_inboxes(conversation_id, agent_id, delivered_at);

CREATE TABLE attachments (
        id                     TEXT PRIMARY KEY,
        channel_id             TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        source_conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
        filename               TEXT NOT NULL,
        size                   INTEGER NOT NULL,
        content_type           TEXT NOT NULL,
        storage                TEXT NOT NULL,
        storage_key            TEXT NOT NULL,
        created_by_kind        TEXT NOT NULL,
        created_by_id          TEXT NOT NULL,
        created_at             INTEGER NOT NULL
      , content_revision INTEGER NOT NULL DEFAULT 1);

CREATE INDEX idx_attachments_channel
        ON attachments(channel_id, created_at DESC);

CREATE INDEX idx_attachments_source_conversation
        ON attachments(source_conversation_id, created_at DESC);

CREATE TABLE schedules (
        id              TEXT NOT NULL PRIMARY KEY,
        owner_agent_id  TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
        message         TEXT NOT NULL,
        spec_json       TEXT NOT NULL,
        next_fire_at    INTEGER NOT NULL,
        status          TEXT NOT NULL
                        CHECK (status IN ('pending','firing','done','cancelled','failed')),
        fired_count     INTEGER NOT NULL DEFAULT 0,
        last_fired_at   INTEGER,
        max_occurrences INTEGER,
        until_at        INTEGER,
        failure_reason  TEXT,
        cancelled_at    INTEGER,
        created_at      INTEGER NOT NULL,
        updated_at      INTEGER NOT NULL
      , title TEXT NOT NULL DEFAULT '', target_conversation_id TEXT NOT NULL DEFAULT '', last_failure_reason TEXT, last_failure_at INTEGER, consecutive_failure_count INTEGER NOT NULL DEFAULT 0);

CREATE INDEX idx_schedules_due
        ON schedules(status, next_fire_at);

CREATE INDEX idx_schedules_owner
        ON schedules(owner_agent_id, status);

CREATE UNIQUE INDEX uniq_conversation_inbox_delivery
        ON conversation_inboxes(conversation_id, agent_id, message_id);

CREATE TABLE users (
        id                  TEXT PRIMARY KEY,
        name                TEXT NOT NULL UNIQUE,
        display_name        TEXT NOT NULL,
        avatar_storage_key  TEXT,
        avatar_content_type TEXT,
        created_at          INTEGER NOT NULL,
        updated_at          INTEGER NOT NULL, email TEXT, last_seen_at INTEGER, avatar_design_id TEXT REFERENCES avatar_designs(id)
        CHECK (avatar_design_id IS NULL OR avatar_storage_key IS NOT NULL),
        CHECK (
          (avatar_storage_key IS NULL AND avatar_content_type IS NULL)
          OR
          (avatar_storage_key IS NOT NULL AND avatar_content_type IS NOT NULL)
        )
      );

CREATE TABLE messages (
        id                      TEXT PRIMARY KEY,
        conversation_id         TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        from_kind               TEXT NOT NULL,
        from_agent_id           TEXT REFERENCES agents(id) ON DELETE SET NULL,
        from_user_id            TEXT REFERENCES users(id) ON DELETE SET NULL,
        content                 TEXT NOT NULL,
        metadata                TEXT,
        seq                     INTEGER NOT NULL,
        created_at              INTEGER NOT NULL,
        thread_root_id TEXT REFERENCES messages(id) ON DELETE CASCADE, author_id           TEXT, author_name         TEXT, author_display_name TEXT, cc_source_message_id TEXT, client_message_id TEXT, client_request_json TEXT);

CREATE INDEX idx_messages_conversation
        ON messages(conversation_id, seq);

CREATE INDEX idx_messages_conversation_from_kind
        ON messages(conversation_id, from_kind, seq);

CREATE TABLE notifications (
        id                      TEXT PRIMARY KEY,
        kind                    TEXT NOT NULL,
        user_id                 TEXT REFERENCES users(id) ON DELETE SET NULL,
        agent_id                TEXT REFERENCES agents(id) ON DELETE SET NULL,
        workspace_id            TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
        conversation_id         TEXT REFERENCES conversations(id) ON DELETE SET NULL,
        message_id              TEXT REFERENCES messages(id) ON DELETE SET NULL,
        source_workspace_id     TEXT REFERENCES workspaces(id) ON DELETE SET NULL,
        source_conversation_id  TEXT REFERENCES conversations(id) ON DELETE SET NULL,
        source_message_id       TEXT REFERENCES messages(id) ON DELETE SET NULL,
        payload                 TEXT,
        read_at                 INTEGER,
        created_at              INTEGER NOT NULL
      );

CREATE INDEX idx_notifications_unread
        ON notifications(user_id, read_at, created_at DESC);

CREATE INDEX idx_notifications_conversation_unread
        ON notifications(conversation_id, read_at, created_at DESC);

CREATE INDEX idx_notifications_source_conversation
        ON notifications(source_conversation_id, created_at DESC);

CREATE TABLE workspace_users (
        workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        joined_at    INTEGER NOT NULL, role TEXT NOT NULL DEFAULT 'member'
        CHECK (role IN ('owner', 'admin', 'member')),
        PRIMARY KEY (workspace_id, user_id)
      );

CREATE INDEX idx_workspace_users_user
        ON workspace_users(user_id, joined_at);

CREATE TABLE conversation_users (
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        joined_at       INTEGER NOT NULL,
        PRIMARY KEY (conversation_id, user_id)
      );

CREATE INDEX idx_conversation_users_user
        ON conversation_users(user_id, joined_at);

CREATE TABLE agents (
        id                  TEXT PRIMARY KEY,
        name                TEXT NOT NULL UNIQUE,
        runtime             TEXT NOT NULL,
        model               TEXT,
        host_id             TEXT NOT NULL REFERENCES hosts(id) ON DELETE RESTRICT,
        description         TEXT,
        created_at          INTEGER NOT NULL,
        updated_at          INTEGER NOT NULL,
        display_name        TEXT NOT NULL DEFAULT '',
        reasoning_effort    TEXT,
        fast_mode           INTEGER,
        created_by_user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
        avatar_storage_key  TEXT,
        avatar_content_type TEXT, materialization_status TEXT NOT NULL
        DEFAULT 'active'
        CHECK (materialization_status IN (
          'pending_materialization', 'active', 'failed'
        )), materialization_failed_code TEXT, materialization_failed_at INTEGER, idempotency_key TEXT, runtime_transition_started_at INTEGER, deletion_started_at INTEGER, session_scope TEXT NOT NULL
        DEFAULT 'conversation'
        CHECK (session_scope IN ('conversation', 'agent')), last_activity_at INTEGER, runtime_transition_target TEXT, avatar_design_id TEXT REFERENCES avatar_designs(id)
        CHECK (avatar_design_id IS NULL OR avatar_storage_key IS NOT NULL),
        CHECK (
          (avatar_storage_key IS NULL AND avatar_content_type IS NULL)
          OR
          (avatar_storage_key IS NOT NULL AND avatar_content_type IS NOT NULL)
        )
      );

CREATE TRIGGER attachments_created_by_validate_insert
      BEFORE INSERT ON attachments
      BEGIN
        SELECT CASE
          WHEN NEW.created_by_kind = 'user'
               AND NOT EXISTS (SELECT 1 FROM users WHERE id = NEW.created_by_id)
          THEN RAISE(ABORT, 'attachments.created_by_id (kind=user) must reference users(id)')
          WHEN NEW.created_by_kind = 'agent'
               AND NOT EXISTS (SELECT 1 FROM agents WHERE id = NEW.created_by_id)
          THEN RAISE(ABORT, 'attachments.created_by_id (kind=agent) must reference agents(id)')
        END;
      END;

CREATE TRIGGER attachments_created_by_validate_update
      BEFORE UPDATE OF created_by_kind, created_by_id ON attachments
      BEGIN
        SELECT CASE
          WHEN NEW.created_by_kind = 'user'
               AND NOT EXISTS (SELECT 1 FROM users WHERE id = NEW.created_by_id)
          THEN RAISE(ABORT, 'attachments.created_by_id (kind=user) must reference users(id)')
          WHEN NEW.created_by_kind = 'agent'
               AND NOT EXISTS (SELECT 1 FROM agents WHERE id = NEW.created_by_id)
          THEN RAISE(ABORT, 'attachments.created_by_id (kind=agent) must reference agents(id)')
        END;
      END;

CREATE TRIGGER users_name_unique_against_agents_insert
      BEFORE INSERT ON users
      WHEN EXISTS (SELECT 1 FROM agents WHERE name = NEW.name)
      BEGIN
        SELECT RAISE(ABORT, 'users.name collides with an existing agent name');
      END;

CREATE TRIGGER users_name_unique_against_agents_update
      BEFORE UPDATE OF name ON users
      WHEN NEW.name != OLD.name
        AND EXISTS (SELECT 1 FROM agents WHERE name = NEW.name)
      BEGIN
        SELECT RAISE(ABORT, 'users.name collides with an existing agent name');
      END;

CREATE TRIGGER agents_name_unique_against_users_insert
      BEFORE INSERT ON agents
      WHEN EXISTS (SELECT 1 FROM users WHERE name = NEW.name)
      BEGIN
        SELECT RAISE(ABORT, 'agents.name collides with an existing user name');
      END;

CREATE TRIGGER agents_name_unique_against_users_update
      BEFORE UPDATE OF name ON agents
      WHEN NEW.name != OLD.name
        AND EXISTS (SELECT 1 FROM users WHERE name = NEW.name)
      BEGIN
        SELECT RAISE(ABORT, 'agents.name collides with an existing user name');
      END;

CREATE INDEX idx_schedules_target
        ON schedules(target_conversation_id);

CREATE UNIQUE INDEX uniq_users_email_when_set
        ON users(email COLLATE NOCASE)
        WHERE email IS NOT NULL;

CREATE TABLE user_password_credentials (
        user_id               TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
        password_hash         TEXT NOT NULL,
        password_hash_params  TEXT NOT NULL CHECK (json_valid(password_hash_params)),
        password_updated_at   INTEGER NOT NULL
      );

CREATE TABLE auth_sessions (
        id             TEXT PRIMARY KEY,
        user_id        TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        token_hash     TEXT NOT NULL UNIQUE,
        created_at     INTEGER NOT NULL,
        last_used_at   INTEGER NOT NULL,
        revoked_at     INTEGER,
        user_agent     TEXT,
        ip_first_seen  TEXT
      );

CREATE INDEX idx_auth_sessions_user
        ON auth_sessions(user_id, revoked_at, last_used_at DESC);

CREATE INDEX idx_hosts_user
        ON hosts(user_id, revoked_at, created_at);

CREATE TABLE host_credentials (
        id           TEXT PRIMARY KEY,
        host_id      TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
        token_hash   TEXT NOT NULL UNIQUE,
        kind         TEXT NOT NULL CHECK (kind IN ('long_lived', 'bootstrap')),
        expires_at   INTEGER,
        consumed_at  INTEGER,
        revoked_at   INTEGER
      );

CREATE INDEX idx_host_credentials_host
        ON host_credentials(host_id, kind, revoked_at, expires_at);

CREATE UNIQUE INDEX uniq_hosts_user_installation_when_active
        ON hosts(user_id, installation_id)
        WHERE user_id IS NOT NULL
          AND installation_id IS NOT NULL
          AND revoked_at IS NULL;

CREATE INDEX idx_conversation_inbox_replay_pending
        ON conversation_inboxes(conversation_id, agent_id, delivered_at, failed_at, in_flight_owner, id);

CREATE INDEX idx_conversation_inbox_in_flight_owner
        ON conversation_inboxes(in_flight_owner, delivered_at, failed_at);

CREATE UNIQUE INDEX idx_agents_idempotency_key_per_user
        ON agents (created_by_user_id, idempotency_key)
        WHERE idempotency_key IS NOT NULL;

CREATE TABLE workspace_agent_capabilities (
        workspace_id TEXT NOT NULL,
        agent_id     TEXT NOT NULL,
        capability   TEXT NOT NULL CHECK (capability IN (
          'read_messages',
          'send_messages',
          'read_tasks',
          'create_tasks',
          'write_tasks',
          'read_attachments',
          'upload_attachments',
          'create_threads',
          'create_channels',
          'manage_conversation_members',
          'manage_schedules'
        )),
        PRIMARY KEY (workspace_id, agent_id, capability),
        FOREIGN KEY (workspace_id, agent_id)
          REFERENCES workspace_members (workspace_id, agent_id) ON DELETE CASCADE
      );

CREATE UNIQUE INDEX uniq_messages_conversation_seq
        ON messages(conversation_id, seq);

CREATE INDEX idx_messages_thread_root
        ON messages(conversation_id, thread_root_id, seq);

CREATE TRIGGER messages_thread_root_insert
      BEFORE INSERT ON messages
      WHEN NEW.thread_root_id IS NOT NULL
      BEGIN
        SELECT CASE
          WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
          THEN RAISE(ABORT, 'thread_root_id must reference an existing message')
        END;
        SELECT CASE
          WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
          THEN RAISE(ABORT, 'thread root must be in the same conversation')
        END;
        SELECT CASE
          WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
          THEN RAISE(ABORT, 'thread root must not itself be a reply (no nested threads)')
        END;
      END;

CREATE TRIGGER messages_thread_root_update
      BEFORE UPDATE OF thread_root_id ON messages
      WHEN NEW.thread_root_id IS NOT NULL
      BEGIN
        SELECT CASE
          WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
          THEN RAISE(ABORT, 'thread_root_id must reference an existing message')
        END;
        SELECT CASE
          WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
          THEN RAISE(ABORT, 'thread root must be in the same conversation')
        END;
        SELECT CASE
          WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
          THEN RAISE(ABORT, 'thread root must not itself be a reply (no nested threads)')
        END;
        -- Turning a message that already HAS replies into a reply would
        -- orphan its children into a nested thread. The app never updates
        -- thread_root_id, but the schema must still forbid the transition.
        SELECT CASE
          WHEN EXISTS (SELECT 1 FROM messages WHERE thread_root_id = OLD.id)
          THEN RAISE(ABORT, 'cannot turn a thread root with existing replies into a reply')
        END;
      END;

CREATE TABLE thread_followers (
        thread_root_id   TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        principal_kind   TEXT NOT NULL CHECK (principal_kind IN ('human', 'agent')),
        principal_id     TEXT NOT NULL,
        followed_at      INTEGER NOT NULL,
        source           TEXT NOT NULL CHECK (source IN ('reply', 'mention', 'manual', 'target_author')),
        PRIMARY KEY (thread_root_id, principal_kind, principal_id)
      );

CREATE INDEX idx_thread_followers_principal
        ON thread_followers(principal_kind, principal_id);

CREATE TRIGGER thread_followers_root_insert
      BEFORE INSERT ON thread_followers
      BEGIN
        SELECT CASE
          WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
          THEN RAISE(ABORT, 'thread_followers.thread_root_id must reference an existing message')
        END;
        SELECT CASE
          WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
          THEN RAISE(ABORT, 'thread follower root must be in the same conversation')
        END;
        SELECT CASE
          WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
          THEN RAISE(ABORT, 'thread follower root must not itself be a reply')
        END;
      END;

CREATE TRIGGER thread_followers_root_update
      BEFORE UPDATE OF thread_root_id, conversation_id ON thread_followers
      BEGIN
        SELECT CASE
          WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
          THEN RAISE(ABORT, 'thread_followers.thread_root_id must reference an existing message')
        END;
        SELECT CASE
          WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
          THEN RAISE(ABORT, 'thread follower root must be in the same conversation')
        END;
        SELECT CASE
          WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
          THEN RAISE(ABORT, 'thread follower root must not itself be a reply')
        END;
      END;

CREATE TABLE conversation_stream_heads (
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        thread_root_id   TEXT REFERENCES messages(id) ON DELETE CASCADE,
        latest_seq       INTEGER NOT NULL
      );

CREATE UNIQUE INDEX uniq_stream_head_main
        ON conversation_stream_heads(conversation_id) WHERE thread_root_id IS NULL;

CREATE UNIQUE INDEX uniq_stream_head_thread
        ON conversation_stream_heads(conversation_id, thread_root_id) WHERE thread_root_id IS NOT NULL;

CREATE TRIGGER conversation_stream_heads_root_insert
      BEFORE INSERT ON conversation_stream_heads
      WHEN NEW.thread_root_id IS NOT NULL
      BEGIN
        SELECT CASE
          WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
          THEN RAISE(ABORT, 'conversation_stream_heads.thread_root_id must reference an existing message')
        END;
        SELECT CASE
          WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
          THEN RAISE(ABORT, 'stream head root must be in the same conversation')
        END;
        SELECT CASE
          WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
          THEN RAISE(ABORT, 'stream head root must not itself be a reply')
        END;
      END;

CREATE TRIGGER conversation_stream_heads_root_update
      BEFORE UPDATE OF thread_root_id, conversation_id ON conversation_stream_heads
      WHEN NEW.thread_root_id IS NOT NULL
      BEGIN
        SELECT CASE
          WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
          THEN RAISE(ABORT, 'conversation_stream_heads.thread_root_id must reference an existing message')
        END;
        SELECT CASE
          WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
          THEN RAISE(ABORT, 'stream head root must be in the same conversation')
        END;
        SELECT CASE
          WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
          THEN RAISE(ABORT, 'stream head root must not itself be a reply')
        END;
      END;

CREATE TABLE message_reactions (
        id            TEXT PRIMARY KEY,
        message_id    TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        reactor_kind  TEXT NOT NULL CHECK (reactor_kind IN ('user', 'agent')),
        reactor_id    TEXT NOT NULL,
        emoji         TEXT NOT NULL,
        created_at    INTEGER NOT NULL,
        UNIQUE (message_id, reactor_kind, reactor_id, emoji)
      );

CREATE INDEX idx_message_reactions_message
        ON message_reactions(message_id);

CREATE TABLE user_sort_entry (
        user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        list_key         TEXT NOT NULL,
        entity_id        TEXT NOT NULL,
        rank             INTEGER,
        last_accessed_at INTEGER, starred_at INTEGER,
        PRIMARY KEY (user_id, list_key, entity_id)
      );

CREATE INDEX idx_user_sort_entry_lookup
        ON user_sort_entry(user_id, list_key);

CREATE TABLE user_list_sort_mode (
        user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        list_key   TEXT NOT NULL,
        scope      TEXT NOT NULL DEFAULT '',
        mode       TEXT NOT NULL CHECK (mode IN ('recent', 'manual')),
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (user_id, list_key, scope)
      );

CREATE TABLE "conversation_reads" (
        user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        thread_root_id   TEXT REFERENCES messages(id) ON DELETE CASCADE,
        last_read_seq    INTEGER NOT NULL DEFAULT 0,
        read_at          INTEGER NOT NULL
      );

CREATE UNIQUE INDEX uniq_conversation_reads_main
        ON conversation_reads(user_id, conversation_id) WHERE thread_root_id IS NULL;

CREATE UNIQUE INDEX uniq_conversation_reads_thread
        ON conversation_reads(user_id, conversation_id, thread_root_id) WHERE thread_root_id IS NOT NULL;

CREATE INDEX idx_conversation_reads_conversation
        ON conversation_reads(conversation_id);

CREATE TRIGGER conversation_reads_root_insert
        BEFORE INSERT ON conversation_reads
        WHEN NEW.thread_root_id IS NOT NULL
        BEGIN
          SELECT CASE
            WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
            THEN RAISE(ABORT, 'conversation_reads.thread_root_id must reference an existing message')
          END;
          SELECT CASE
            WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
            THEN RAISE(ABORT, 'conversation_reads root must be in the same conversation')
          END;
          SELECT CASE
            WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
            THEN RAISE(ABORT, 'conversation_reads root must not itself be a reply')
          END;
        END;

CREATE TRIGGER conversation_reads_root_update
        BEFORE UPDATE OF thread_root_id, conversation_id ON conversation_reads
        WHEN NEW.thread_root_id IS NOT NULL
        BEGIN
          SELECT CASE
            WHEN NOT EXISTS (SELECT 1 FROM messages WHERE id = NEW.thread_root_id)
            THEN RAISE(ABORT, 'conversation_reads.thread_root_id must reference an existing message')
          END;
          SELECT CASE
            WHEN COALESCE((SELECT conversation_id FROM messages WHERE id = NEW.thread_root_id), '') != NEW.conversation_id
            THEN RAISE(ABORT, 'conversation_reads root must be in the same conversation')
          END;
          SELECT CASE
            WHEN (SELECT thread_root_id FROM messages WHERE id = NEW.thread_root_id) IS NOT NULL
            THEN RAISE(ABORT, 'conversation_reads root must not itself be a reply')
          END;
        END;

CREATE INDEX idx_messages_thread_root_id
        ON messages(thread_root_id);

CREATE INDEX idx_conversation_inboxes_message
        ON conversation_inboxes(message_id);

CREATE INDEX idx_notifications_workspace
        ON notifications(workspace_id);

CREATE INDEX idx_notifications_source_workspace
        ON notifications(source_workspace_id);

CREATE INDEX idx_notifications_message
        ON notifications(message_id);

CREATE INDEX idx_notifications_source_message
        ON notifications(source_message_id);

CREATE INDEX idx_conversation_reads_thread_root
        ON conversation_reads(thread_root_id);

CREATE INDEX idx_conversation_stream_heads_conversation
        ON conversation_stream_heads(conversation_id);

CREATE INDEX idx_conversation_stream_heads_thread_root
        ON conversation_stream_heads(thread_root_id);

CREATE INDEX idx_thread_followers_conversation
        ON thread_followers(conversation_id);

CREATE TABLE "channel_attention_preferences" (
        user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        mode             TEXT NOT NULL CHECK (mode IN ('all', 'mentions', 'mute')),
        updated_at       INTEGER NOT NULL,
        PRIMARY KEY (user_id, conversation_id)
      );

CREATE INDEX idx_channel_attention_prefs_conversation
        ON channel_attention_preferences(conversation_id);

CREATE TABLE workspace_attention_preferences (
        user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        workspace_id  TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        muted         INTEGER NOT NULL CHECK (muted IN (0, 1)),
        updated_at    INTEGER NOT NULL,
        PRIMARY KEY (user_id, workspace_id)
      );

CREATE INDEX idx_workspace_attention_prefs_workspace
        ON workspace_attention_preferences(workspace_id);

CREATE INDEX idx_conversation_reads_user_conversation_stream
        ON conversation_reads(user_id, conversation_id, thread_root_id);

CREATE TRIGGER agents_session_scope_immutable
        BEFORE UPDATE OF session_scope ON agents
        WHEN NEW.session_scope != OLD.session_scope
        BEGIN
          SELECT RAISE(ABORT, 'agents.session_scope is immutable');
        END;

CREATE TABLE "conversation_agents" (
        conversation_id TEXT NOT NULL
          REFERENCES conversations(id) ON DELETE CASCADE,
        agent_id        TEXT NOT NULL
          REFERENCES agents(id) ON DELETE CASCADE,
        joined_at       INTEGER NOT NULL,
        PRIMARY KEY (conversation_id, agent_id)
      );

CREATE INDEX idx_conversation_agents_agent
        ON conversation_agents(agent_id);

CREATE TABLE agent_runtime_events (
        id                            INTEGER PRIMARY KEY AUTOINCREMENT,
        -- Historical coordinate, deliberately no FK: deleting an Agent/slot
        -- must not mutate the append-only runtime history or be blocked by its
        -- anti-DELETE trigger.
        slot_id                       INTEGER NOT NULL,
        agent_id                      TEXT NOT NULL,
        conversation_id               TEXT,
        generation                    INTEGER NOT NULL
          CHECK (generation >= 0),
        event_kind                    TEXT NOT NULL CHECK (event_kind IN (
          'slot_migrated',
          'generation_claimed',
          'projection_updated',
          'generation_finalized'
        )),
        daemon_instance_id            TEXT,
        observed_status               TEXT NOT NULL
          CHECK (observed_status IN ('idle', 'starting', 'active', 'crashed')),
        observed_pid                  INTEGER,
        cli_session_id                TEXT,
        occurred_at                   INTEGER NOT NULL
      );

CREATE INDEX idx_agent_runtime_events_slot_generation
        ON agent_runtime_events(slot_id, generation, id);

CREATE INDEX idx_agent_runtime_events_scope_generation
        ON agent_runtime_events(agent_id, conversation_id, generation, id);

CREATE TRIGGER agent_runtime_events_append_only_update
        BEFORE UPDATE ON agent_runtime_events
        BEGIN
          SELECT RAISE(ABORT, 'agent_runtime_events is append-only');
        END;

CREATE TRIGGER agent_runtime_events_append_only_delete
        BEFORE DELETE ON agent_runtime_events
        BEGIN
          SELECT RAISE(ABORT, 'agent_runtime_events is append-only');
        END;

CREATE UNIQUE INDEX idx_messages_cc_dedupe
        ON messages(cc_source_message_id, conversation_id)
        WHERE cc_source_message_id IS NOT NULL;

CREATE TRIGGER trg_agents_stamp_endpoint_deleted
      BEFORE DELETE ON agents
      BEGIN
        UPDATE conversations
           SET endpoint_a_deleted_at = CAST(unixepoch('subsec') * 1000 AS INTEGER)
         WHERE placement_kind = 'direct_pair'
           AND endpoint_a_kind = 'agent'
           AND endpoint_a_id = OLD.id
           AND endpoint_a_deleted_at IS NULL;
        UPDATE conversations
           SET endpoint_b_deleted_at = CAST(unixepoch('subsec') * 1000 AS INTEGER)
         WHERE placement_kind = 'direct_pair'
           AND endpoint_b_kind = 'agent'
           AND endpoint_b_id = OLD.id
           AND endpoint_b_deleted_at IS NULL;
      END;

CREATE TRIGGER trg_users_stamp_endpoint_deleted
      BEFORE DELETE ON users
      BEGIN
        UPDATE conversations
           SET endpoint_a_deleted_at = CAST(unixepoch('subsec') * 1000 AS INTEGER)
         WHERE placement_kind = 'direct_pair'
           AND endpoint_a_kind = 'human'
           AND endpoint_a_id = OLD.id
           AND endpoint_a_deleted_at IS NULL;
        UPDATE conversations
           SET endpoint_b_deleted_at = CAST(unixepoch('subsec') * 1000 AS INTEGER)
         WHERE placement_kind = 'direct_pair'
           AND endpoint_b_kind = 'human'
           AND endpoint_b_id = OLD.id
           AND endpoint_b_deleted_at IS NULL;
      END;

CREATE TABLE "agent_runtime_slots" (
        id                            INTEGER PRIMARY KEY AUTOINCREMENT,
        agent_id                      TEXT NOT NULL
          REFERENCES agents(id) ON DELETE CASCADE,
        conversation_id               TEXT
          REFERENCES conversations(id) ON DELETE CASCADE,
        generation                    INTEGER NOT NULL DEFAULT 0
          CHECK (generation >= 0),
        claimed_by_daemon_instance_id TEXT,
        observed_status               TEXT NOT NULL DEFAULT 'idle'
          CHECK (observed_status IN ('idle', 'starting', 'active', 'crashed')),
        observed_pid                  INTEGER,
        cli_session_id                TEXT
      );

CREATE UNIQUE INDEX uniq_agent_runtime_slots_agent_scope
        ON agent_runtime_slots(agent_id)
        WHERE conversation_id IS NULL;

CREATE UNIQUE INDEX uniq_agent_runtime_slots_conversation_scope
        ON agent_runtime_slots(agent_id, conversation_id)
        WHERE conversation_id IS NOT NULL;

CREATE INDEX idx_agent_runtime_slots_conversation
        ON agent_runtime_slots(conversation_id, agent_id);

CREATE TRIGGER agent_runtime_slots_scope_consistency_insert
        BEFORE INSERT ON agent_runtime_slots
        WHEN COALESCE(
          (SELECT session_scope FROM agents WHERE id = NEW.agent_id), ''
        ) != CASE
          WHEN NEW.conversation_id IS NULL THEN 'agent'
          ELSE 'conversation'
        END
        BEGIN
          SELECT RAISE(ABORT, 'runtime slot scope must match agents.session_scope');
        END;

CREATE TRIGGER agent_runtime_slots_scope_consistency_update
        BEFORE UPDATE OF agent_id, conversation_id ON agent_runtime_slots
        WHEN COALESCE(
          (SELECT session_scope FROM agents WHERE id = NEW.agent_id), ''
        ) != CASE
          WHEN NEW.conversation_id IS NULL THEN 'agent'
          ELSE 'conversation'
        END
        BEGIN
          SELECT RAISE(ABORT, 'runtime slot scope must match agents.session_scope');
        END;

CREATE TRIGGER agent_runtime_slots_identity_immutable
        BEFORE UPDATE OF agent_id, conversation_id ON agent_runtime_slots
        WHEN NEW.agent_id != OLD.agent_id
          OR NEW.conversation_id IS NOT OLD.conversation_id
        BEGIN
          SELECT RAISE(ABORT, 'runtime slot identity is immutable');
        END;

CREATE TRIGGER agent_runtime_slots_generation_monotonic
        BEFORE UPDATE OF generation ON agent_runtime_slots
        WHEN NEW.generation != OLD.generation + 1
        BEGIN
          SELECT RAISE(ABORT, 'runtime slot generation must advance by one');
        END;

CREATE TRIGGER conversation_agents_edge_immutable
        BEFORE UPDATE OF conversation_id, agent_id ON conversation_agents
        WHEN NEW.conversation_id != OLD.conversation_id
          OR NEW.agent_id != OLD.agent_id
        BEGIN
          SELECT RAISE(
            ABORT,
            'a roster edge is immutable; leave and join instead of moving it'
          );
        END;

CREATE TRIGGER conversation_agents_create_runtime_slot
        AFTER INSERT ON conversation_agents
        WHEN (SELECT session_scope FROM agents WHERE id = NEW.agent_id)
             = 'conversation'
         AND (SELECT placement_kind FROM conversations WHERE id = NEW.conversation_id)
             = 'workspace'
        BEGIN
          INSERT INTO agent_runtime_slots (
            agent_id, conversation_id, generation,
            observed_status, observed_pid, cli_session_id
          ) VALUES (
            NEW.agent_id, NEW.conversation_id, 0,
            'idle', NULL, NULL
          );
        END;

CREATE TRIGGER agents_create_agent_runtime_slot
        AFTER INSERT ON agents
        WHEN NEW.session_scope = 'agent'
        BEGIN
          INSERT INTO agent_runtime_slots (
            agent_id, conversation_id, generation,
            observed_status, observed_pid, cli_session_id
          ) VALUES (
            NEW.id, NULL, 0,
            'idle', NULL, NULL
          );
        END;

CREATE TRIGGER conversation_agents_delete_runtime_slot
        AFTER DELETE ON conversation_agents
        WHEN (SELECT placement_kind FROM conversations WHERE id = OLD.conversation_id)
             = 'workspace'
        BEGIN
          DELETE FROM agent_runtime_slots
            WHERE conversation_id = OLD.conversation_id
              AND agent_id = OLD.agent_id;
        END;

CREATE TABLE "tasks" (
        id                       TEXT PRIMARY KEY,
        workspace_id             TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
        task_number              INTEGER NOT NULL,
        conversation_id          TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        thread_root_id           TEXT,
        title                    TEXT NOT NULL,
        content                  TEXT NOT NULL DEFAULT '',
        status                   TEXT NOT NULL,
        assignee_agent_id        TEXT REFERENCES agents(id) ON DELETE SET NULL,
        assignee_user_id         TEXT REFERENCES users(id) ON DELETE SET NULL,
        creator_kind             TEXT NOT NULL,
        creator_agent_id         TEXT,
        creator_user_id          TEXT,
        source_message_id        TEXT,
        created_event_message_id TEXT,
        last_editor_kind         TEXT,
        last_editor_id           TEXT,
        last_edited_at           INTEGER,
        created_at               INTEGER NOT NULL,
        closed_at                INTEGER,
        board_key                TEXT NOT NULL,
        CHECK (status IN ('todo', 'in_progress', 'in_review', 'done', 'closed')),
        CHECK (task_number > 0),
        -- At most one assignee (agent XOR user, or neither = unassigned).
        CHECK (assignee_agent_id IS NULL OR assignee_user_id IS NULL),
        -- Creator kind pairs EXACTLY with its id column (immutable provenance).
        -- 'system' = both ids null: a task whose creating agent/user was since
        -- deleted (from_agent_id/from_user_id are ON DELETE SET NULL — a
        -- supported product path), degraded exactly as the old store did.
        CHECK (
          (creator_kind = 'agent' AND creator_agent_id IS NOT NULL AND creator_user_id IS NULL)
          OR (creator_kind = 'user' AND creator_user_id IS NOT NULL AND creator_agent_id IS NULL)
          OR (creator_kind = 'system' AND creator_agent_id IS NULL AND creator_user_id IS NULL)
        ),
        -- Last-editor is all-null (never edited) or a COMPLETE triple, so the
        -- "edited by X · <time>" byline can always be fully rendered.
        CHECK (
          (last_editor_kind IS NULL AND last_editor_id IS NULL AND last_edited_at IS NULL)
          OR (last_editor_kind IN ('agent', 'user') AND last_editor_id IS NOT NULL AND last_edited_at IS NOT NULL)
        ),
        -- closed_at is set IFF the task is in the terminal 'closed' status
        -- (mirrors the service: 'done' does NOT set closed_at).
        CHECK ((status = 'closed') = (closed_at IS NOT NULL)),
        -- Board arc (P0-f): the key's namespace prefix and the workspace
        -- column's null-ness imply each other in BOTH directions. Any prefix
        -- outside {ws:, pair:} leaves the two biconditionals contradictory,
        -- so an unknown board family is unsatisfiable by construction.
        CHECK ((board_key LIKE 'ws:%') = (workspace_id IS NOT NULL)),
        CHECK ((board_key LIKE 'pair:%') = (workspace_id IS NULL))
      );

CREATE INDEX idx_tasks_conversation ON tasks(conversation_id);

CREATE INDEX idx_tasks_status_number
        ON tasks(status, task_number DESC, id DESC);

CREATE INDEX idx_tasks_workspace_status_number
        ON tasks(workspace_id, status, task_number DESC, id DESC);

CREATE UNIQUE INDEX uniq_tasks_source_message
        ON tasks(source_message_id) WHERE source_message_id IS NOT NULL;

CREATE UNIQUE INDEX uniq_tasks_board_number
        ON tasks(board_key, task_number);

CREATE TRIGGER trg_tasks_board_key_derivation_insert
        BEFORE INSERT ON tasks
        WHEN NOT EXISTS (
          SELECT 1 FROM conversations c
           WHERE c.id = NEW.conversation_id
             AND (
               (c.placement_kind = 'workspace'
                 AND NEW.board_key = 'ws:' || c.workspace_id
                 AND NEW.workspace_id IS c.workspace_id)
               OR
               (c.placement_kind = 'direct_pair'
                 AND NEW.board_key = 'pair:' || c.pair_key
                 AND NEW.workspace_id IS NULL)
             )
        )
        BEGIN
          SELECT RAISE(
            ABORT,
            'tasks.board_key must equal the board derived from the conversation placement'
          );
        END;

CREATE TRIGGER trg_tasks_board_coordinates_immutable
        BEFORE UPDATE OF conversation_id, workspace_id, board_key ON tasks
        WHEN NOT (
          NEW.conversation_id IS OLD.conversation_id
          AND NEW.workspace_id IS OLD.workspace_id
          AND NEW.board_key IS OLD.board_key
        )
        BEGIN
          SELECT RAISE(ABORT, 'tasks: board coordinates are immutable from birth');
        END;

CREATE TABLE agent_runtime_control_events (
        id                             INTEGER PRIMARY KEY AUTOINCREMENT,
        agent_id                       TEXT NOT NULL,
        generation                     INTEGER NOT NULL CHECK (generation >= 0),
        actor_user_id                  TEXT NOT NULL,
        reason                         TEXT NOT NULL
          CHECK (reason IN ('session_cleared')),
        was_running                    INTEGER NOT NULL
          CHECK (was_running IN (0, 1)),
        previous_cli_session_id_prefix TEXT,
        occurred_at                    INTEGER NOT NULL
      );

CREATE INDEX idx_agent_runtime_control_events_agent
        ON agent_runtime_control_events(agent_id, id);

CREATE TRIGGER agent_runtime_slots_daemon_claim_generation_bound
        BEFORE UPDATE OF claimed_by_daemon_instance_id ON agent_runtime_slots
        WHEN NOT (
          NEW.claimed_by_daemon_instance_id IS OLD.claimed_by_daemon_instance_id
          OR (
            NEW.generation = OLD.generation + 1
            AND NEW.claimed_by_daemon_instance_id IS NOT NULL
          )
          OR (
            NEW.generation = OLD.generation
            AND OLD.claimed_by_daemon_instance_id IS NOT NULL
            AND NEW.claimed_by_daemon_instance_id IS NULL
          )
        )
        BEGIN
          SELECT RAISE(
            ABORT,
            'runtime slot daemon claim cannot rebind within a generation'
          );
        END;

CREATE TRIGGER agent_runtime_control_events_append_only_update
        BEFORE UPDATE ON agent_runtime_control_events
        BEGIN
          SELECT RAISE(ABORT, 'agent_runtime_control_events is append-only');
        END;

CREATE TRIGGER agent_runtime_control_events_append_only_delete
        BEFORE DELETE ON agent_runtime_control_events
        BEGIN
          SELECT RAISE(ABORT, 'agent_runtime_control_events is append-only');
        END;

CREATE TRIGGER channel_attention_prefs_channel_only_insert
        BEFORE INSERT ON channel_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT roster_policy FROM conversations WHERE id = NEW.conversation_id), ''
            ) != 'open'
            THEN RAISE(ABORT, 'channel_attention_preferences.conversation_id must reference a regular channel')
          END;
        END;

CREATE TRIGGER channel_attention_prefs_channel_only_update
        BEFORE UPDATE OF conversation_id ON channel_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT roster_policy FROM conversations WHERE id = NEW.conversation_id), ''
            ) != 'open'
            THEN RAISE(ABORT, 'channel_attention_preferences.conversation_id must reference a regular channel')
          END;
        END;

CREATE TABLE "saved_messages" (
        id               TEXT PRIMARY KEY,
        message_id       TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        saved_at         INTEGER NOT NULL,
        user_id          TEXT REFERENCES users(id) ON DELETE CASCADE
      );

CREATE UNIQUE INDEX uniq_saved_messages_user_message
        ON saved_messages(user_id, message_id)
        WHERE user_id IS NOT NULL;

CREATE INDEX idx_saved_messages_user_saved_at
        ON saved_messages(user_id, saved_at DESC)
        WHERE user_id IS NOT NULL;

CREATE INDEX idx_saved_messages_conversation
        ON saved_messages(conversation_id);

CREATE INDEX idx_saved_messages_message
        ON saved_messages(message_id);

CREATE TABLE "workspaces" (
        id                 TEXT PRIMARY KEY,
        name               TEXT NOT NULL,
        name_key           TEXT
          CHECK (
            name_key IS NULL
            OR (
              name_key <> ''
              AND NOT name_key GLOB '*[A-Z]*'
              AND NOT name_key GLOB ' *'
              AND NOT name_key GLOB '* '
            )
          ),
        description        TEXT,
        owner_agent_id     TEXT UNIQUE REFERENCES agents(id) ON DELETE CASCADE,
        archived_at        INTEGER,
        created_at         INTEGER NOT NULL,
        updated_at         INTEGER NOT NULL,
        owner_user_id      TEXT REFERENCES users(id),
        CHECK ( (owner_agent_id IS NULL) = (name_key IS NOT NULL) )
      );

CREATE UNIQUE INDEX idx_workspaces_name_key
        ON workspaces(name_key) WHERE owner_agent_id IS NULL;

CREATE TRIGGER workspace_members_no_agent_home_insert
        BEFORE INSERT ON workspace_members
        WHEN (SELECT owner_agent_id FROM workspaces WHERE id = NEW.workspace_id) IS NOT NULL
        BEGIN
          SELECT RAISE(ABORT, 'workspace_members rows must reference regular workspaces only');
        END;

CREATE TRIGGER workspace_members_no_agent_home_update
        BEFORE UPDATE OF workspace_id ON workspace_members
        WHEN (SELECT owner_agent_id FROM workspaces WHERE id = NEW.workspace_id) IS NOT NULL
        BEGIN
          SELECT RAISE(ABORT, 'workspace_members rows must reference regular workspaces only');
        END;

CREATE TRIGGER workspace_attention_prefs_regular_only_insert
        BEFORE INSERT ON workspace_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT owner_agent_id IS NULL FROM workspaces WHERE id = NEW.workspace_id), 0
            ) = 0
            THEN RAISE(ABORT, 'workspace_attention_preferences.workspace_id must reference a regular workspace')
          END;
        END;

CREATE TRIGGER workspace_attention_prefs_regular_only_update
        BEFORE UPDATE OF workspace_id ON workspace_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT owner_agent_id IS NULL FROM workspaces WHERE id = NEW.workspace_id), 0
            ) = 0
            THEN RAISE(ABORT, 'workspace_attention_preferences.workspace_id must reference a regular workspace')
          END;
        END;

CREATE TABLE "conversations" (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
        endpoint_a_kind         TEXT,
        endpoint_a_id           TEXT,
        endpoint_b_kind         TEXT,
        endpoint_b_id           TEXT,
        endpoint_a_deleted_at   INTEGER,
        endpoint_b_deleted_at   INTEGER,
        placement_kind          TEXT NOT NULL
          CHECK (placement_kind IN ('workspace', 'direct_pair')),
        roster_policy           TEXT NOT NULL
          CHECK (roster_policy IN ('open', 'fixed')),
        title                   TEXT NOT NULL,
        title_key               TEXT
          CHECK (
            title_key IS NULL
            OR (
              title_key <> ''
              AND NOT title_key GLOB '*[A-Z]*'
              AND NOT title_key GLOB ' *'
              AND NOT title_key GLOB '* '
            )
          ),
        status                  TEXT NOT NULL,
        spawned_by_agent_id     TEXT REFERENCES agents(id) ON DELETE SET NULL,
        closed_at               INTEGER,
        close_summary           TEXT,
        pinned_at               INTEGER,
        created_at              INTEGER NOT NULL,
        updated_at              INTEGER NOT NULL,
        archived_at             INTEGER,
        latest_message_seq      INTEGER NOT NULL DEFAULT 0,
        visibility              TEXT NOT NULL DEFAULT 'private'
          CHECK (
            visibility IN ('private', 'public')
            AND (visibility = 'private' OR roster_policy = 'open')
          ),
        pair_key                TEXT
        GENERATED ALWAYS AS (
          CASE
            WHEN placement_kind <> 'direct_pair' THEN NULL
            ELSE substr(endpoint_a_kind, 1, 1)
              || length(endpoint_a_id) || ':' || endpoint_a_id
              || substr(endpoint_b_kind, 1, 1)
              || length(endpoint_b_id) || ':' || endpoint_b_id
          END
        ) VIRTUAL,
        CHECK (
          (endpoint_a_kind IS NULL OR endpoint_a_kind IN ('human', 'agent'))
          AND (endpoint_b_kind IS NULL OR endpoint_b_kind IN ('human', 'agent'))
        ),
        CHECK (
          placement_kind <> 'direct_pair'
          OR (endpoint_a_kind, endpoint_a_id) < (endpoint_b_kind, endpoint_b_id)
        ),
        CHECK (
          CASE placement_kind
            WHEN 'workspace'
              THEN workspace_id IS NOT NULL
                   AND title_key IS NOT NULL
                   AND endpoint_a_kind IS NULL AND endpoint_a_id IS NULL
                   AND endpoint_b_kind IS NULL AND endpoint_b_id IS NULL
                   AND endpoint_a_deleted_at IS NULL
                   AND endpoint_b_deleted_at IS NULL
            WHEN 'direct_pair'
              THEN workspace_id IS NULL
                   AND roster_policy = 'fixed'
                   AND title_key IS NULL
                   AND endpoint_a_kind IS NOT NULL AND endpoint_a_id IS NOT NULL
                   AND endpoint_b_kind IS NOT NULL AND endpoint_b_id IS NOT NULL
            ELSE 0
          END
        )
      );

CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;

CREATE INDEX idx_conversations_direct_pair_endpoint_b
        ON conversations(endpoint_b_kind, endpoint_b_id)
        WHERE placement_kind = 'direct_pair';

CREATE UNIQUE INDEX idx_conversations_direct_pair_endpoints
        ON conversations(
          endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id
        )
        WHERE placement_kind = 'direct_pair';

CREATE UNIQUE INDEX idx_conversations_pair_key
        ON conversations(pair_key)
        WHERE placement_kind = 'direct_pair';

CREATE INDEX idx_conversations_status
        ON conversations(status);

CREATE UNIQUE INDEX idx_conversations_title_key
        ON conversations(workspace_id, title_key)
        WHERE placement_kind = 'workspace';

CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, updated_at DESC);

CREATE TRIGGER conversations_create_direct_pair_runtime_slots
        AFTER INSERT ON conversations
        WHEN NEW.placement_kind = 'direct_pair'
        BEGIN
          INSERT INTO agent_runtime_slots (
            agent_id, conversation_id, generation,
            observed_status, observed_pid, cli_session_id
          )
            SELECT a.id, NEW.id, 0, 'idle', NULL, NULL
              FROM agents a
             WHERE a.session_scope = 'conversation'
               AND ((NEW.endpoint_a_kind = 'agent' AND a.id = NEW.endpoint_a_id)
                 OR (NEW.endpoint_b_kind = 'agent' AND a.id = NEW.endpoint_b_id));
        END;

CREATE TRIGGER conversations_placement_immutable
        BEFORE UPDATE OF
          placement_kind, workspace_id, roster_policy
        ON conversations
        WHEN NOT (
          NEW.placement_kind IS OLD.placement_kind
          AND NEW.workspace_id IS OLD.workspace_id
          AND NEW.roster_policy IS OLD.roster_policy
        )
        BEGIN
          SELECT RAISE(
            ABORT,
            'conversations: placement is immutable from birth'
          );
        END;

CREATE TRIGGER trg_conversations_endpoint_tombstone_monotonic
        BEFORE UPDATE OF endpoint_a_deleted_at, endpoint_b_deleted_at
        ON conversations
        WHEN NOT (
          (NEW.endpoint_a_deleted_at IS OLD.endpoint_a_deleted_at
           OR (OLD.endpoint_a_deleted_at IS NULL
               AND NEW.endpoint_a_deleted_at IS NOT NULL))
          AND
          (NEW.endpoint_b_deleted_at IS OLD.endpoint_b_deleted_at
           OR (OLD.endpoint_b_deleted_at IS NULL
               AND NEW.endpoint_b_deleted_at IS NOT NULL))
        )
        BEGIN
          SELECT RAISE(
            ABORT,
            'conversations: an endpoint deletion is stamped once and never unstamped'
          );
        END;

CREATE TRIGGER trg_conversations_endpoints_birth
        BEFORE INSERT ON conversations
        WHEN NEW.placement_kind = 'direct_pair'
         AND NOT (
           (CASE NEW.endpoint_a_kind
              WHEN 'human' THEN EXISTS (SELECT 1 FROM users u WHERE u.id = NEW.endpoint_a_id)
              WHEN 'agent' THEN EXISTS (SELECT 1 FROM agents g WHERE g.id = NEW.endpoint_a_id)
              ELSE 0
            END)
           AND
           (CASE NEW.endpoint_b_kind
              WHEN 'human' THEN EXISTS (SELECT 1 FROM users u WHERE u.id = NEW.endpoint_b_id)
              WHEN 'agent' THEN EXISTS (SELECT 1 FROM agents g WHERE g.id = NEW.endpoint_b_id)
              ELSE 0
            END)
         )
        BEGIN
          SELECT RAISE(
            ABORT,
            'conversations: each direct_pair endpoint must name an existing principal of its own kind'
          );
        END;

CREATE TRIGGER trg_conversations_endpoints_born_alive
        BEFORE INSERT ON conversations
        WHEN NEW.endpoint_a_deleted_at IS NOT NULL
          OR NEW.endpoint_b_deleted_at IS NOT NULL
        BEGIN
          SELECT RAISE(
            ABORT,
            'conversations: an endpoint is born alive; its deletion is stamped by a later write'
          );
        END;

CREATE TRIGGER trg_conversations_endpoints_immutable
        BEFORE UPDATE OF
          endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id
        ON conversations
        WHEN NOT (
          NEW.endpoint_a_kind IS OLD.endpoint_a_kind
          AND NEW.endpoint_a_id IS OLD.endpoint_a_id
          AND NEW.endpoint_b_kind IS OLD.endpoint_b_kind
          AND NEW.endpoint_b_id IS OLD.endpoint_b_id
        )
        BEGIN
          SELECT RAISE(
            ABORT,
            'conversations: endpoint coordinates are immutable'
          );
        END;

CREATE TRIGGER agent_runtime_slots_participation_insert
        BEFORE INSERT ON agent_runtime_slots
        WHEN NEW.conversation_id IS NOT NULL
         AND NOT EXISTS (
           SELECT 1
             FROM conversations c
            WHERE c.id = NEW.conversation_id
              AND ((c.placement_kind = 'workspace'
                    AND EXISTS (
                      SELECT 1 FROM conversation_agents ca
                       WHERE ca.conversation_id = c.id
                         AND ca.agent_id = NEW.agent_id))
                OR (c.placement_kind = 'direct_pair'
                    AND ((c.endpoint_a_kind = 'agent'
                          AND c.endpoint_a_id = NEW.agent_id)
                      OR (c.endpoint_b_kind = 'agent'
                          AND c.endpoint_b_id = NEW.agent_id))))
         )
        BEGIN
          SELECT RAISE(
            ABORT,
            'runtime slot requires participation in its conversation'
          );
        END;

CREATE TABLE "conversation_events" (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL,
        kind TEXT NOT NULL,
        payload TEXT,
        created_at INTEGER NOT NULL,
        runtime_generation INTEGER CHECK(runtime_generation IS NULL OR runtime_generation > 0)
      );

CREATE INDEX idx_conversation_events ON conversation_events(conversation_id, id);

CREATE INDEX idx_conversation_events_agent ON conversation_events(conversation_id, agent_id, id);

CREATE INDEX idx_conversation_events_stream ON conversation_events(agent_id, id);

CREATE INDEX idx_conversation_events_created_at ON conversation_events(created_at);

CREATE UNIQUE INDEX idx_messages_client_identity ON messages(from_user_id, client_message_id)
        WHERE client_message_id IS NOT NULL;

CREATE TRIGGER agents_runtime_transition_pair_insert BEFORE INSERT ON agents
      WHEN (NEW.runtime_transition_target IS NULL) != (NEW.runtime_transition_started_at IS NULL)
        OR (NEW.runtime_transition_target IS NOT NULL AND NEW.runtime_transition_target NOT IN
          ('claude','codex','cursor','gemini','qoder','opencode','pi'))
      BEGIN SELECT RAISE(ABORT, 'invalid agents runtime transition lock'); END;

CREATE TRIGGER agents_runtime_transition_pair_update
      BEFORE UPDATE OF runtime_transition_target, runtime_transition_started_at ON agents
      WHEN (NEW.runtime_transition_target IS NULL) != (NEW.runtime_transition_started_at IS NULL)
        OR (NEW.runtime_transition_target IS NOT NULL AND NEW.runtime_transition_target NOT IN
          ('claude','codex','cursor','gemini','qoder','opencode','pi'))
      BEGIN SELECT RAISE(ABORT, 'invalid agents runtime transition lock'); END;

CREATE INDEX idx_messages_profile_user ON messages(from_user_id, created_at)
        WHERE from_kind='user' AND cc_source_message_id IS NULL;

CREATE INDEX idx_messages_profile_agent ON messages(from_agent_id, created_at)
        WHERE from_kind='agent' AND cc_source_message_id IS NULL;

CREATE INDEX idx_auth_sessions_last_used_at
        ON auth_sessions(last_used_at, user_id);

CREATE INDEX idx_messages_product_activity_created_at
        ON messages(created_at)
        WHERE from_kind IN ('user','agent') AND cc_source_message_id IS NULL;

CREATE TABLE avatar_designs (
        id            TEXT PRIMARY KEY,
        parts_version INTEGER NOT NULL CHECK (parts_version > 0),
        config        TEXT NOT NULL CHECK (json_valid(config) AND json_type(config) = 'object'),
        created_at    INTEGER NOT NULL
      );

CREATE UNIQUE INDEX idx_users_avatar_design ON users(avatar_design_id)
        WHERE avatar_design_id IS NOT NULL;

CREATE UNIQUE INDEX idx_agents_avatar_design ON agents(avatar_design_id)
        WHERE avatar_design_id IS NOT NULL;

CREATE TRIGGER users_avatar_design_follows_image BEFORE UPDATE OF avatar_storage_key ON users
        WHEN OLD.avatar_design_id IS NOT NULL AND NEW.avatar_design_id IS OLD.avatar_design_id
          AND NEW.avatar_storage_key IS NOT OLD.avatar_storage_key
        BEGIN SELECT RAISE(ABORT, 'avatar design must change with its image'); END;

CREATE TRIGGER agents_avatar_design_follows_image BEFORE UPDATE OF avatar_storage_key ON agents
        WHEN OLD.avatar_design_id IS NOT NULL AND NEW.avatar_design_id IS OLD.avatar_design_id
          AND NEW.avatar_storage_key IS NOT OLD.avatar_storage_key
        BEGIN SELECT RAISE(ABORT, 'avatar design must change with its image'); END;

CREATE TRIGGER users_avatar_design_release AFTER UPDATE OF avatar_design_id ON users
        WHEN OLD.avatar_design_id IS NOT NULL AND NEW.avatar_design_id IS NOT OLD.avatar_design_id
        BEGIN DELETE FROM avatar_designs WHERE id = OLD.avatar_design_id; END;

CREATE TRIGGER agents_avatar_design_release AFTER UPDATE OF avatar_design_id ON agents
        WHEN OLD.avatar_design_id IS NOT NULL AND NEW.avatar_design_id IS NOT OLD.avatar_design_id
        BEGIN DELETE FROM avatar_designs WHERE id = OLD.avatar_design_id; END;

CREATE TRIGGER users_avatar_design_delete AFTER DELETE ON users
        WHEN OLD.avatar_design_id IS NOT NULL
        BEGIN DELETE FROM avatar_designs WHERE id = OLD.avatar_design_id; END;

CREATE TRIGGER agents_avatar_design_delete AFTER DELETE ON agents
        WHEN OLD.avatar_design_id IS NOT NULL
        BEGIN DELETE FROM avatar_designs WHERE id = OLD.avatar_design_id; END;

CREATE TABLE password_recovery_state (
        user_id           TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
        email             TEXT NOT NULL,
        code_id           TEXT,
        code_hash         TEXT,
        code_expires_at   INTEGER,
        code_attempts     INTEGER NOT NULL DEFAULT 0,
        window_started_at INTEGER NOT NULL,
        window_sends      INTEGER NOT NULL DEFAULT 0,
        window_attempts   INTEGER NOT NULL DEFAULT 0,
        last_sent_at      INTEGER,
        CHECK ((code_id IS NULL) = (code_hash IS NULL)),
        CHECK ((code_id IS NULL) = (code_expires_at IS NULL))
      );

CREATE TRIGGER conversation_inboxes_settlement_state_insert BEFORE INSERT ON conversation_inboxes
  WHEN (NEW.in_flight_owner IS NULL) != (NEW.in_flight_at IS NULL)
    OR (NEW.delivered_at IS NOT NULL AND NEW.failed_at IS NOT NULL)
    OR (NEW.in_flight_owner IS NOT NULL
        AND (NEW.delivered_at IS NOT NULL OR NEW.failed_at IS NOT NULL))
  BEGIN SELECT RAISE(ABORT, 'invalid conversation inbox settlement state'); END;

CREATE TRIGGER conversation_inboxes_settlement_state_update
  BEFORE UPDATE OF delivered_at, failed_at, in_flight_owner, in_flight_at ON conversation_inboxes
  WHEN (NEW.in_flight_owner IS NULL) != (NEW.in_flight_at IS NULL)
    OR (NEW.delivered_at IS NOT NULL AND NEW.failed_at IS NOT NULL)
    OR (NEW.in_flight_owner IS NOT NULL
        AND (NEW.delivered_at IS NOT NULL OR NEW.failed_at IS NOT NULL))
  BEGIN SELECT RAISE(ABORT, 'invalid conversation inbox settlement state'); END;
