import { canonicalizeTaskTitleV96 } from './migration-contracts/task-title-v96.js';
import { randomBytes } from 'node:crypto';
import {
  auditConversationTitleAddresses,
  describeConversationTitleAudit,
} from '../conversation-title-audit.js';
import { canonicalizeStoredConversationTitle } from '../conversation-title.js';
import {
  auditWorkspaceNameAddresses,
  describeWorkspaceNameAudit,
} from '../workspace-name-audit.js';
import { canonicalizeStoredWorkspaceName } from '../workspace-name.js';
import { BASELINE_MIGRATION_STEP, BASELINE_SCHEMA_SQL } from './baseline-schema.js';

/**
 * Backend-injected db handle type. We keep `precondition` typed as
 * `unknown` here in the shared package so this module stays free of a
 * runtime dependency on `better-sqlite3` (only the backend imports
 * that). The migration runner in `packages/backend/src/db.ts` casts
 * to its `Database` instance before invoking.
 */
export type MigrationDb = unknown;

export interface Migration {
  version: number;
  name: string;
  up: string;
  foreignKeyEscape?: true;
  /**
   * Optional code-level precondition. Runs INSIDE the migration's
   * transaction, BEFORE `up` is exec'd. Throwing aborts the
   * transaction — no rows are written to `_migrations`, so the
   * migration is retried on next boot.
   *
   * Use cases:
   *   - asserting a prior bridge has been consumed before a cleanup
   *     migration runs;
   *   - inspecting `PRAGMA foreign_key_check` results before applying
   *     destructive DDL;
   *   - any guard whose error message must be clearer than what a SQL
   *     CHECK / UNIQUE violation would produce.
   *
   * SQL-only `RAISE(ABORT, ...)` cannot be used in standalone
   * statements (SQLite restricts it to trigger programs), so this
   * hook is the canonical way to fail-fast a migration with a
   * developer-readable error.
   */
  precondition?: (db: MigrationDb) => void;
}

export const migrations: Migration[] = [
  {
    version: BASELINE_MIGRATION_STEP,
    name: 'baseline',
    // Compacted snapshot of historical migrations v1..v44 (#task:1213). The
    // executable DDL is the single authoritative source in baseline-schema.ts
    // and is pinned by the two-stage migration-baseline drift guard. Plain
    // CREATE (no IF NOT EXISTS): the floor guard
    // (`MIN_SUPPORTED_MIGRATION_STEP` in schema-epochs.ts, derived from the
    // support-policy epoch and never below `BASELINE_MIGRATION_STEP`, enforced by
    // the step guard in packages/backend/src/db.ts) guarantees this only ever runs
    // on an empty DB, so it must fail loud if that invariant is ever violated.
    up: BASELINE_SCHEMA_SQL,
  },
  {
    version: 45,
    name: 'users_last_seen_at',
    up: `
      ALTER TABLE users ADD COLUMN last_seen_at INTEGER;
    `,
  },
  {
    version: 46,
    name: 'message_reactions',
    up: `
      -- Message reactions (docs/reactions-design.md §3.1 / §4).
      -- Reactions are an out-of-band projection on a message, NOT messages
      -- themselves: a separate table so they never write a message row, never
      -- allocate a conversation seq, and never advance a stream head — the CAS
      -- exclusion invariant (§12.A). High-frequency, multi-actor, aggregated
      -- data; a metadata-JSON blob would have concurrent-write clobber + no
      -- per-reactor uniqueness, so it gets its own table + unique constraint.
      --
      -- reactor identity is isomorphic (reactor_kind + reactor_id), mirroring
      -- messages.from_* — never a user_id/agent_id column pair
      -- (architecture-principles.md §1 #5). No FK on reactor_id: it softly
      -- references either users(id) or agents(id) by kind.
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
    `,
  },
  {
    version: 47,
    name: 'user_list_sort',
    up: `
      -- Per-user (viewer-scoped) list sort state for the workspace rail and
      -- each workspace's channel sidebar (#task:1146 / #task:1147).
      --
      -- This is personal VIEW STATE, not an aggregate field: ordering is
      -- "how THIS viewer arranged the list", so it never lives on the
      -- workspace / conversation aggregate (which would pollute it with
      -- viewer-relative data). It is its own read model with its own tables.
      --
      -- list_key namespaces a list: 'workspaces' (the rail) | 'channels:<ws_id>'
      -- (one workspace's channel sidebar). mode is per-LIST (one [recent|manual]
      -- toggle per list); absence of a row = default 'recent'.
      CREATE TABLE user_list_sort_mode (
        user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        list_key   TEXT NOT NULL,
        mode       TEXT NOT NULL CHECK (mode IN ('recent', 'manual')),
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (user_id, list_key)
      );

      -- Per-entity viewer state within a list: manual rank + recent-opened
      -- timestamp. rank is a SINGLE value per (user, list, entity) — the hard
      -- groups (rail: dm / regular:pinned / regular:unpinned; channel:
      -- pinned / unpinned) are partitioned LIVE at sort time from the entity's
      -- own pin/kind state, NOT baked into a bucket key here. That keeps
      -- pin/unpin from having to migrate ranks across buckets.
      --   rank NULL             = not manually arranged (tails by created_at).
      --   last_accessed_at NULL = never opened by this viewer.
      -- entity_id softly references workspaces(id) OR conversations(id) by
      -- list_key (no FK — polymorphic, mirroring message_reactions.reactor_id);
      -- orphaned rows after a delete are harmless (filtered out when joined
      -- against the live list).
      CREATE TABLE user_sort_entry (
        user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        list_key         TEXT NOT NULL,
        entity_id        TEXT NOT NULL,
        rank             INTEGER,
        last_accessed_at INTEGER,
        PRIMARY KEY (user_id, list_key, entity_id)
      );

      CREATE INDEX idx_user_sort_entry_lookup
        ON user_sort_entry(user_id, list_key);
    `,
  },
  {
    version: 48,
    name: 'user_list_sort_mode_per_scope',
    up: `
      -- Per-SCOPE sort mode (#task:1146). The workspace rail's sort mode is now
      -- per hard-group (pinned / dm / regular) instead of one per list, so a
      -- "scope" column joins the key. The channel sidebar stays list-wide via
      -- the sentinel scope '' (one mode for the whole list, unchanged).
      --   scope ''                       = list-wide  (channels:<ws_id>)
      --   scope 'pinned'|'dm'|'regular'  = one rail section (workspaces)
      -- SQLite can't add a column to a PRIMARY KEY in place, so rebuild the
      -- table and fold every existing row into the list-wide scope ''. Existing
      -- channel rows are already list-wide (correct). Pre-existing 'workspaces'
      -- rows become a harmless orphan at scope '' — the rail now reads its modes
      -- at the group scopes, so they simply fall back to the default 'recent'
      -- (an accepted reset; the rail's group taxonomy is changing anyway).
      ALTER TABLE user_list_sort_mode RENAME TO user_list_sort_mode_old;
      CREATE TABLE user_list_sort_mode (
        user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        list_key   TEXT NOT NULL,
        scope      TEXT NOT NULL DEFAULT '',
        mode       TEXT NOT NULL CHECK (mode IN ('recent', 'manual')),
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (user_id, list_key, scope)
      );
      INSERT INTO user_list_sort_mode (user_id, list_key, scope, mode, updated_at)
        SELECT user_id, list_key, '', mode, updated_at FROM user_list_sort_mode_old;
      DROP TABLE user_list_sort_mode_old;
    `,
  },
  {
    version: 49,
    name: 'conversation_activity_agent_index',
    up: `
      CREATE INDEX IF NOT EXISTS idx_conversation_events_agent
        ON conversation_events(conversation_id, agent_id, id);
    `,
  },
  {
    version: 50,
    name: 'user_sort_entry_starred',
    up: `
      -- Personal ⭐ star (#task:1209). Pin/Star was previously a SHARED aggregate
      -- field on the workspace / conversation (admin-gated, one value for
      -- everyone). It becomes per-VIEWER personal view state: which entities THIS
      -- viewer has starred, living alongside their rank / recent-opened in the
      -- per-(user, list, entity) sort entry. Non-null = starred. It is a PRESENCE
      -- marker (+ timestamp for future/debug), NOT a sort key: within-group order
      -- stays governed by the group's mode (recent → last_accessed_at / manual →
      -- rank), so the STARRED section's sort menu is not silently overridden.
      --
      -- No seed: the old global workspaces.pinned_at / conversations.pinned_at
      -- columns are deliberately NOT migrated into per-user stars (decided by
      -- shiki — few users, a one-time re-star is cheaper than carrying migration
      -- dirty-code as debt). Those columns become vestigial (migrations are
      -- append-only, so they physically remain) and stop being read/written.
      ALTER TABLE user_sort_entry ADD COLUMN starred_at INTEGER;
    `,
  },
  {
    version: 51,
    name: 'conversation_reads_per_stream',
    // Per-stream read cursor (#task:1226). The read boundary was per-CONVERSATION
    // (one cursor for the mainline + every thread together), so thread-level
    // unread could not be expressed (ThreadSummary.unread_count stayed optional
    // and unfilled). This generalizes `conversation_reads` to per-STREAM, exactly
    // mirroring the per-stream `conversation_stream_heads` freshness model: one
    // concept (stream) at one grain on both the read and write sides.
    //
    // thread_root_id NULL = mainline; non-NULL = that reply-cluster. Uniqueness
    // uses the SAME two partial-unique indexes as conversation_stream_heads —
    // SQLite composite PK with a nullable column does not dedupe NULLs reliably,
    // so a PK is wrong here.
    //
    // Table rebuild (SQLite can't drop the old PRIMARY KEY) → foreignKeyEscape so
    // the runner toggles foreign_keys OFF + legacy_alter_table ON and runs
    // foreign_key_check afterward.
    foreignKeyEscape: true,
    up: `
      CREATE TABLE conversation_reads_new (
        user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        thread_root_id   TEXT REFERENCES messages(id) ON DELETE CASCADE,
        last_read_seq    INTEGER NOT NULL DEFAULT 0,
        read_at          INTEGER NOT NULL
      );

      -- Old per-conversation rows become the MAINLINE cursor (thread_root_id NULL),
      -- value carried over verbatim.
      INSERT INTO conversation_reads_new (user_id, conversation_id, thread_root_id, last_read_seq, read_at)
        SELECT user_id, conversation_id, NULL, last_read_seq, read_at FROM conversation_reads;

      DROP TABLE conversation_reads;
      ALTER TABLE conversation_reads_new RENAME TO conversation_reads;

      CREATE UNIQUE INDEX uniq_conversation_reads_main
        ON conversation_reads(user_id, conversation_id) WHERE thread_root_id IS NULL;
      CREATE UNIQUE INDEX uniq_conversation_reads_thread
        ON conversation_reads(user_id, conversation_id, thread_root_id) WHERE thread_root_id IS NOT NULL;
      CREATE INDEX idx_conversation_reads_conversation
        ON conversation_reads(conversation_id);

      -- Root-invariant guard (mirrors conversation_stream_heads / thread_followers):
      -- thread_root_id REFERENCES messages only proves the message exists, NOT that
      -- it belongs to this conversation or is itself a root (non-reply). Without this
      -- a bad client cursor would silently corrupt unread/first-unread. DB-level
      -- fail-loud backstop; the route layer also pre-validates with friendly 400s.
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

      -- Eager seed (#task:1226 key decision): every EXISTING thread gets its own
      -- cursor frozen at the user's old mainline value, so the upgrade does not
      -- flood users with historical thread unread, AND the steady-state read model
      -- has ZERO fallback branch (no vestigial baseline column, no dynamic COALESCE
      -- back to the advancing mainline cursor). Bounded one-time INSERT...SELECT.
      INSERT INTO conversation_reads (user_id, conversation_id, thread_root_id, last_read_seq, read_at)
        SELECT cr.user_id, cr.conversation_id, sh.thread_root_id, cr.last_read_seq, cr.read_at
          FROM conversation_reads cr
          JOIN conversation_stream_heads sh
            ON sh.conversation_id = cr.conversation_id
         WHERE cr.thread_root_id IS NULL
           AND sh.thread_root_id IS NOT NULL;
    `,
  },
  {
    version: 52,
    name: 'channel_attention_preferences',
    // Activity / Attention bounded context (#task:1227, docs/activity-lifecycle-v1.md
    // §9.1). The FIRST write-side aggregate Attention owns: a per-(viewer,
    // channel) ambient preference — the policy input to `enter()` (§6). It is a
    // user DECISION (the channel `所有消息 / 仅提及` control), not a derived fact,
    // so it legitimately gets its own table (建表准入测试, §4.2). Everything else
    // Attention surfaces is a projection (cursor read-model / notifications /
    // tasks), never stored. (v65 adds the SECOND and — today — last one, the
    // per-(viewer, workspace) mute suppressor, and a third `mode` value `'mute'`;
    // the shape below is v52's, kept verbatim as history.)
    //
    // `mode`:
    //   'all'      = ambient on  → channel mainline ambient unread enters Activity
    //   'mentions' = ambient off → ambient unread stays as in-place hint only;
    //                directed (@me / task / DM) still enters, unconditionally (§6).
    // Absence of a row = the default (`'all'`, ambient on — §6 channel
    // default). The table stores the viewer's explicit choice (which MAY equal
    // the default — we do not delete-on-default); reads resolve absence to the
    // default. Scope is REGULAR channel only: a DM's unread is directed-grade
    // (always enters, §5.1) and a thread's ambient is follow-driven delivery, so
    // neither carries this preference. The route gates with getChannel (kind =
    // 'channel'); the triggers below are the DB fail-loud backstop so no
    // repo/test can write a pref for a dm_channel or thread out from under the
    // route.
    //
    // delivery ⊥ attention (§5.2): this never touches thread_followers/wake.
    up: `
      CREATE TABLE channel_attention_preferences (
        user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        mode             TEXT NOT NULL CHECK (mode IN ('all', 'mentions')),
        updated_at       INTEGER NOT NULL,
        PRIMARY KEY (user_id, conversation_id)
      );

      CREATE INDEX idx_channel_attention_prefs_conversation
        ON channel_attention_preferences(conversation_id);

      -- Regular-channel-only invariant (DB backstop; route pre-validates with a
      -- friendly 404). A pref on a dm_channel/thread is meaningless dead state
      -- and would leave ②c's enter() ambiguous — fail loud instead.
      CREATE TRIGGER channel_attention_prefs_channel_only_insert
        BEFORE INSERT ON channel_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT kind FROM conversations WHERE id = NEW.conversation_id), ''
            ) != 'channel'
            THEN RAISE(ABORT, 'channel_attention_preferences.conversation_id must reference a regular channel')
          END;
        END;

      CREATE TRIGGER channel_attention_prefs_channel_only_update
        BEFORE UPDATE OF conversation_id ON channel_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT kind FROM conversations WHERE id = NEW.conversation_id), ''
            ) != 'channel'
            THEN RAISE(ABORT, 'channel_attention_preferences.conversation_id must reference a regular channel')
          END;
        END;
    `,
  },
  {
    version: 53,
    name: 'retire_thread_conversation_kind',
    // #task:1303 (2c) — retire the legacy `conversations.kind='thread'` fossil
    // (isolated thread conversations, "Sense1"). Its producer was removed
    // 2026-06-11; the reply-cluster model (messages.thread_root_id, "Sense2")
    // is now the sole meaning of "thread" in the domain. Phase-1 of #task:1303
    // hard-deleted every fossil row from the live DBs (the legacy-thread purge
    // device); this migration tightens the schema so no new fossil row can ever
    // be written, closing the fossil out of the ubiquitous language for good.
    //
    // SQLite cannot ALTER a CHECK constraint, so narrowing the column CHECK
    // `kind IN ('channel','dm_channel','thread')` → `('channel','dm_channel')`
    // and dropping the `thread` branch of the table-level parent CHECK requires
    // a full table rebuild. foreignKeyEscape: the runner toggles foreign_keys
    // OFF + legacy_alter_table ON around it (conversations has a self-FK via
    // parent_conversation_id and is referenced by nearly every table) and runs
    // foreign_key_check afterward.
    //
    // parent_conversation_id is KEPT (now provably always NULL under the tight
    // CHECK) along with idx_conversations_parent — dropping the column is out of
    // scope for this cut. DROP TABLE auto-drops the old
    // conversations_thread_parent_{insert,update} triggers; they are NOT
    // recreated (nothing left to guard once 'thread' cannot exist).
    foreignKeyEscape: true,
    // Fail-loud backstop: safe only on a DB whose fossil rows were already
    // purged (Phase-1). If any kind='thread' row — or any non-NULL
    // parent_conversation_id — survives, the INSERT...SELECT below would trip
    // the new CHECK with an opaque SQLite error. Abort first with a
    // developer-readable message; the throw rolls back the transaction so no
    // _migrations row is written and the upgrade is retried next boot (after
    // the operator runs the purge device).
    precondition: (db) => {
      const handle = db as {
        prepare: (sql: string) => { get: () => { n: number } };
      };
      const { n } = handle
        .prepare(
          `SELECT COUNT(*) AS n FROM conversations
             WHERE kind = 'thread' OR parent_conversation_id IS NOT NULL`,
        )
        .get();
      if (n > 0) {
        throw new Error(
          `migration v53: refusing to tighten the conversations CHECK — ${n} legacy thread fossil row(s) remain ` +
            `(kind='thread' or parent_conversation_id NOT NULL). Run the legacy-thread purge before upgrading.`,
        );
      }
    },
    up: `
      CREATE TABLE conversations_new (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        workspace_path          TEXT NOT NULL,
        kind                    TEXT NOT NULL CHECK (kind IN ('channel', 'dm_channel')),
        parent_conversation_id  TEXT REFERENCES conversations(id) ON DELETE CASCADE,
        title                   TEXT NOT NULL,
        status                  TEXT NOT NULL,
        spawned_by_agent_id     TEXT REFERENCES agents(id) ON DELETE SET NULL,
        closed_at               INTEGER,
        close_summary           TEXT,
        pinned_at               INTEGER,
        created_at              INTEGER NOT NULL,
        updated_at              INTEGER NOT NULL,
        archived_at             INTEGER,
        latest_message_seq      INTEGER NOT NULL DEFAULT 0,
        CHECK (
          (kind = 'channel' AND parent_conversation_id IS NULL)
          OR
          (kind = 'dm_channel' AND parent_conversation_id IS NULL)
        )
      );

      INSERT INTO conversations_new (
        id, workspace_id, workspace_path, kind, parent_conversation_id, title,
        status, spawned_by_agent_id, closed_at, close_summary, pinned_at,
        created_at, updated_at, archived_at, latest_message_seq
      )
        SELECT
          id, workspace_id, workspace_path, kind, parent_conversation_id, title,
          status, spawned_by_agent_id, closed_at, close_summary, pinned_at,
          created_at, updated_at, archived_at, latest_message_seq
        FROM conversations;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

      -- Rebuild every conversations index verbatim (DROP TABLE took them out).
      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, kind, updated_at DESC);
      CREATE INDEX idx_conversations_parent
        ON conversations(parent_conversation_id, created_at ASC);
      CREATE INDEX idx_conversations_status
        ON conversations(status);
      CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;
    `,
  },
  {
    version: 54,
    name: 'drop_conversation_parent_column',
    // #task:1305 — the legacy isolated-thread conversation model is gone from
    // data, schema, DTOs, and runtime code. v53 kept parent_conversation_id as
    // an always-NULL physical column so the kind discriminator cleanup could
    // land independently; this migration removes that last storage fossil.
    //
    // SQLite table rebuild is the least ambiguous shape here: it drops the
    // column, drops idx_conversations_parent, and preserves every surviving
    // conversations column/index explicitly.
    foreignKeyEscape: true,
    precondition: (db) => {
      const handle = db as {
        prepare: (sql: string) => { get: () => { n: number } };
      };
      const { n } = handle
        .prepare(
          `SELECT COUNT(*) AS n FROM conversations
             WHERE parent_conversation_id IS NOT NULL`,
        )
        .get();
      if (n > 0) {
        throw new Error(
          `migration v54: refusing to drop conversations.parent_conversation_id — ${n} row(s) still reference a parent conversation`,
        );
      }
    },
    up: `
      CREATE TABLE conversations_new (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        workspace_path          TEXT NOT NULL,
        kind                    TEXT NOT NULL CHECK (kind IN ('channel', 'dm_channel')),
        title                   TEXT NOT NULL,
        status                  TEXT NOT NULL,
        spawned_by_agent_id     TEXT REFERENCES agents(id) ON DELETE SET NULL,
        closed_at               INTEGER,
        close_summary           TEXT,
        pinned_at               INTEGER,
        created_at              INTEGER NOT NULL,
        updated_at              INTEGER NOT NULL,
        archived_at             INTEGER,
        latest_message_seq      INTEGER NOT NULL DEFAULT 0
      );

      INSERT INTO conversations_new (
        id, workspace_id, workspace_path, kind, title, status,
        spawned_by_agent_id, closed_at, close_summary, pinned_at,
        created_at, updated_at, archived_at, latest_message_seq
      )
        SELECT
          id, workspace_id, workspace_path, kind, title, status,
          spawned_by_agent_id, closed_at, close_summary, pinned_at,
          created_at, updated_at, archived_at, latest_message_seq
        FROM conversations;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, kind, updated_at DESC);
      CREATE INDEX idx_conversations_status
        ON conversations(status);
      CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;
    `,
  },
  {
    version: 55,
    name: 'host_activation_and_provisioning_source',
    up: `
      -- User-managed computers split host lifecycle from provisioning source:
      -- activated_at is the readiness marker (NULL = pending attach),
      -- while provisioning_source is lifecycle metadata only. Runtime
      -- placement/ACL must compose owner scope with activated/not-revoked and
      -- never branch on provisioning_source.
      ALTER TABLE hosts ADD COLUMN activated_at INTEGER;
      ALTER TABLE hosts ADD COLUMN provisioning_source TEXT NOT NULL DEFAULT 'system'
        CHECK (provisioning_source IN ('app_install', 'user_invite', 'system'));

      -- Existing desktop-claimed installs are app-managed. Unclaimed bootstrap
      -- placeholders and standalone/system rows intentionally remain system.
      UPDATE hosts
         SET provisioning_source = 'app_install'
       WHERE user_id IS NOT NULL
         AND installation_id IS NOT NULL;

      -- Existing active user-owned local hosts predate the pending lifecycle;
      -- treat them as activated so current desktop placement continues to work.
      -- Revoked rows and unowned/system placeholders stay NULL.
      UPDATE hosts
         SET activated_at = COALESCE(last_seen_at, created_at)
       WHERE user_id IS NOT NULL
         AND kind = 'local'
         AND revoked_at IS NULL;
    `,
  },
  {
    version: 56,
    name: 'host_finalization_lifecycle',
    up: `
      -- User-invited computers are setup drafts until the user confirms the
      -- Add computer modal. activated_at only proves daemon attachment;
      -- finalized_at is the boundary for host list / placement eligibility.
      ALTER TABLE hosts ADD COLUMN finalized_at INTEGER;

      -- Existing app-managed active hosts predate the finalize boundary and
      -- are already formal computers. User-invite rows are left NULL so any
      -- in-flight setup remains modal-scoped until explicit finalization.
      UPDATE hosts
         SET finalized_at = activated_at
       WHERE activated_at IS NOT NULL
         AND revoked_at IS NULL
         AND provisioning_source != 'user_invite';
    `,
  },
  {
    version: 57,
    name: 'agent_runtime_transition_lock',
    up: `
      -- Runtime switching is a multi-step transition: block new spawn/delivery,
      -- clear every old-runtime session, then atomically install the target
      -- runtime and tuning. The persisted lock survives a process crash; boot
      -- safely rolls it back because agents.runtime is unchanged until commit.
      ALTER TABLE agents ADD COLUMN runtime_transition_started_at INTEGER;
      ALTER TABLE agents ADD COLUMN runtime_transition_target TEXT
        CHECK (
          (runtime_transition_target IS NULL AND runtime_transition_started_at IS NULL)
          OR
          (runtime_transition_target IN ('claude','codex','cursor','gemini','qoder','opencode')
           AND runtime_transition_started_at IS NOT NULL)
        );

      -- SQLite may skip a column-level CHECK when an UPDATE touches only the
      -- sibling column. Pair triggers make the two-column invariant explicit
      -- for every insert and either-column update.
      CREATE TRIGGER agents_runtime_transition_pair_insert
        BEFORE INSERT ON agents
        WHEN (NEW.runtime_transition_target IS NULL) !=
             (NEW.runtime_transition_started_at IS NULL)
        BEGIN
          SELECT RAISE(ABORT, 'agents runtime transition lock must be a pair');
        END;

      CREATE TRIGGER agents_runtime_transition_pair_update
        BEFORE UPDATE OF runtime_transition_target, runtime_transition_started_at ON agents
        WHEN (NEW.runtime_transition_target IS NULL) !=
             (NEW.runtime_transition_started_at IS NULL)
        BEGIN
          SELECT RAISE(ABORT, 'agents runtime transition lock must be a pair');
        END;
    `,
  },
  {
    version: 58,
    name: 'conversation_visibility',
    up: `
      -- public/private channel visibility. ADDITIVE: every
      -- existing conversation is explicitly backfilled 'private' by the DEFAULT
      -- so there is NO "missing = private" long-term compat branch — the column
      -- is the single source of truth from day one. visibility is only
      -- meaningful for regular channels; a 'dm_channel' (and any future
      -- non-channel kind) is private by construction. public read is gated on
      -- kind='channel' in the permission layer, so this pairing is a
      -- security-relevant invariant, not cosmetic.
      --
      -- A single cross-column CHECK is the whole enforcement. SQLite re-validates
      -- every CHECK on the row for ANY INSERT or UPDATE, including an UPDATE that
      -- touches only the sibling 'kind' column — so demoting a public channel's
      -- kind out from under the pairing aborts on the CHECK alone. (Verified: a
      -- same-shape table with only this CHECK and no trigger rejects
      -- UPDATE...SET kind='dm_channel' on a (channel, public) row with a
      -- "CHECK constraint failed" error.) No BEFORE trigger is needed; adding one
      -- would be permanently redundant. Mirrors the v57 runtime-transition
      -- ADD COLUMN + cross-column CHECK pattern.
      ALTER TABLE conversations ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (
          visibility IN ('private', 'public')
          AND (visibility = 'private' OR kind = 'channel')
        );
    `,
  },
  {
    version: 59,
    name: 'task_edit_source_provenance',
    up: `
      -- Task block-1 redesign. A task keeps being a messages row with task_*
      -- columns; this migration makes its DEFINITION explicitly editable
      -- (title + content via \`task edit\`) with optimistic concurrency and
      -- last-editor attribution, and adds an immutable source-message
      -- provenance link that doubles as a creation mutex.

      -- Immutable provenance: the message that TRIGGERED this task. NULL for a
      -- self-authored task with no originating message. It is NOT a content
      -- source — title/content are the task's own definition; this is pure
      -- provenance + the lock key below.
      ALTER TABLE messages ADD COLUMN source_message_id TEXT;

      -- Optimistic-concurrency token for \`task edit\` (title/content). Bumped
      -- ONLY on an edit, so a concurrent status/assignee change never
      -- spuriously conflicts a title/content edit. NULL for non-task rows.
      ALTER TABLE messages ADD COLUMN task_revision INTEGER;

      -- Last-editor attribution for the "edited by X · <time>" byline. Set
      -- ONLY on \`task edit\` (never on status/assignee), so it names who last
      -- shaped the definition. NULL until the first edit.
      ALTER TABLE messages ADD COLUMN task_last_editor_kind TEXT
        CHECK (task_last_editor_kind IS NULL OR task_last_editor_kind IN ('agent', 'user'));
      ALTER TABLE messages ADD COLUMN task_last_editor_id TEXT;
      ALTER TABLE messages ADD COLUMN task_last_edited_at INTEGER;

      -- Every existing task starts at revision 0 (concrete, never NULL for a
      -- task row) so the edit path has no "NULL means 0" compat branch.
      UPDATE messages SET task_revision = 0 WHERE task_number IS NOT NULL;

      -- Creation mutex + permanent provenance identity: at most ONE task per
      -- source message, FOR THE LIFETIME OF THAT MESSAGE. source_message_id
      -- is a permanent identity key, not a lock held only while the task is
      -- active: once a message has spawned a task, it can never spawn another,
      -- regardless of that task's status. A closed or done task does NOT
      -- release its source — to continue the work you reopen the original
      -- task, you never build a second from the same message. A second create
      -- from the same source hits this unique index and the service maps it to
      -- a typed conflict pointing at the existing task (whatever its status).
      -- NULLs repeat freely in a SQLite unique index, so sourceless tasks and
      -- all non-task rows (source_message_id IS NULL) are unconstrained; the
      -- partial predicate just keeps those NULL rows out of the index.
      CREATE UNIQUE INDEX idx_messages_source_message_id
        ON messages(source_message_id)
        WHERE source_message_id IS NOT NULL;
    `,
  },
  {
    version: 60,
    name: 'tasks_first_class_aggregate',
    up: `
      -- Task-system model redesign. A task becomes a FIRST-CLASS aggregate with
      -- its own table — it is no longer a \`messages\` row carrying task_*
      -- annotations. \`id\` is the identity everywhere (REST/events/mutations);
      -- \`task_number\` is the per-workspace human/CLI resolver.
      --
      -- source_message_id / created_event_message_id are RAW ids (NO foreign
      -- key): a declared source is permanent provenance + the nav discriminant,
      -- so it must survive deletion of the referenced message (navigation then
      -- resolves to 'unavailable') rather than be silently nulled by ON DELETE
      -- SET NULL and reclassify the task to "no source". creator_* are likewise
      -- RAW immutable provenance (the CHECK requires the id present, so a
      -- cascade-null would break it). Only workspace/conversation/assignee are
      -- real FK relationships.
      CREATE TABLE tasks (
        id                       TEXT PRIMARY KEY,
        workspace_id             TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        task_number              INTEGER NOT NULL,
        conversation_id          TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        thread_root_id           TEXT,
        title                    TEXT NOT NULL,
        content                  TEXT NOT NULL DEFAULT '',
        status                   TEXT NOT NULL,
        assignee_agent_id        TEXT REFERENCES agents(id) ON DELETE SET NULL,
        assignee_user_id         TEXT REFERENCES users(id)  ON DELETE SET NULL,
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
        CHECK (status IN ('todo', 'in_progress', 'in_review', 'done', 'closed')),
        CHECK (task_number > 0),
        -- At most one assignee (agent XOR user, or neither = unassigned).
        CHECK (assignee_agent_id IS NULL OR assignee_user_id IS NULL),
        -- Creator kind pairs EXACTLY with its id column (immutable provenance).
        -- 'system' = both ids null: a task whose creating agent/user was since
        -- deleted (from_agent_id/from_user_id are ON DELETE SET NULL — a
        -- supported product path), degraded exactly as the old store did.
        CHECK (
          (creator_kind = 'agent'  AND creator_agent_id IS NOT NULL AND creator_user_id IS NULL) OR
          (creator_kind = 'user'   AND creator_user_id  IS NOT NULL AND creator_agent_id IS NULL) OR
          (creator_kind = 'system' AND creator_agent_id IS NULL     AND creator_user_id  IS NULL)
        ),
        -- Last-editor is all-null (never edited) or a COMPLETE triple, so the
        -- "edited by X · <time>" byline can always be fully rendered.
        CHECK (
          (last_editor_kind IS NULL AND last_editor_id IS NULL AND last_edited_at IS NULL) OR
          (last_editor_kind IN ('agent', 'user') AND last_editor_id IS NOT NULL AND last_edited_at IS NOT NULL)
        ),
        -- closed_at is set IFF the task is in the terminal 'closed' status
        -- (mirrors the service: 'done' does NOT set closed_at).
        CHECK ((status = 'closed') = (closed_at IS NOT NULL))
      );

      -- Per-workspace monotonic numbering (replaces the old
      -- uniq_messages_task_workspace_number invariant).
      CREATE UNIQUE INDEX uniq_tasks_workspace_number ON tasks(workspace_id, task_number);
      -- Permanent 1:1 source→task mutex (moves off messages).
      CREATE UNIQUE INDEX uniq_tasks_source_message ON tasks(source_message_id)
        WHERE source_message_id IS NOT NULL;
      -- Board queries.
      CREATE INDEX idx_tasks_workspace_status ON tasks(workspace_id, status);
      CREATE INDEX idx_tasks_conversation ON tasks(conversation_id);

      -- ── Backfill from the retired "message IS task" rows ────────────────
      -- Migrated tasks REUSE their old anchor message id as tasks.id (that id
      -- WAS the task identity), so every historical reference — including
      -- task system-event metadata.task_message_id — carries forward unchanged.
      -- We copy ONLY the explicit source_message_id column (never guess the
      -- anchor's own id as a source). created_event_message_id is derived ONLY
      -- from the deterministic task_create system event; missing → NULL →
      -- navigation resolves to 'unavailable'. No message rows are deleted.
      INSERT INTO tasks (
        id, workspace_id, task_number, conversation_id, thread_root_id,
        title, content, status, assignee_agent_id, assignee_user_id,
        creator_kind, creator_agent_id, creator_user_id,
        source_message_id, created_event_message_id,
        last_editor_kind, last_editor_id, last_edited_at,
        created_at, closed_at
      )
      SELECT
        m.id,
        COALESCE(m.task_workspace_id, (SELECT c.workspace_id FROM conversations c WHERE c.id = m.conversation_id)),
        m.task_number,
        m.conversation_id,
        m.thread_root_id,
        COALESCE(
          NULLIF(TRIM(m.task_title), ''),
          NULLIF(TRIM(CASE WHEN instr(m.content, char(10)) > 0
                           THEN substr(m.content, 1, instr(m.content, char(10)) - 1)
                           ELSE m.content END), ''),
          '(untitled task)'
        ),
        m.content,
        m.task_status,
        m.task_assignee_agent_id,
        m.task_assignee_user_id,
        -- Creator degrades to 'system' when neither author id survives (the
        -- creating agent/user was deleted — a supported lifecycle), matching
        -- the creator-pairing CHECK and the old store's missing-actor handling.
        CASE WHEN m.from_user_id  IS NOT NULL THEN 'user'
             WHEN m.from_agent_id IS NOT NULL THEN 'agent'
             ELSE 'system' END,
        CASE WHEN m.from_user_id IS NULL THEN m.from_agent_id ELSE NULL END,
        m.from_user_id,
        m.source_message_id,
        (SELECT sm.id FROM messages sm
           WHERE json_extract(sm.metadata, '$.subtype') = 'task_create'
             AND json_extract(sm.metadata, '$.task_message_id') = m.id
           ORDER BY sm.seq, sm.id
           LIMIT 1),
        m.task_last_editor_kind,
        m.task_last_editor_id,
        m.task_last_edited_at,
        m.created_at,
        CASE WHEN m.task_status = 'closed' THEN m.task_closed_at ELSE NULL END
      FROM messages m
      WHERE m.task_number IS NOT NULL;

      -- Add the canonical task_id to existing task system-event metadata. For a
      -- migrated task, tasks.id == the old anchor id == metadata.task_message_id,
      -- so this is a deterministic copy. Legacy partial rows (no task_message_id)
      -- are left untouched and keep rendering via the partial reader.
      UPDATE messages
        SET metadata = json_set(metadata, '$.task_id', json_extract(metadata, '$.task_message_id'))
        WHERE json_extract(metadata, '$.subtype') IN
                ('task_create', 'task_claim', 'task_unclaim', 'task_status')
          AND json_extract(metadata, '$.task_message_id') IS NOT NULL;
    `,
  },
  {
    version: 61,
    name: 'workspace_cascade_child_fk_indexes',
    // #task:1677 — deleting a workspace is ONE row delete that fans out
    // through FK actions (CASCADE / SET NULL). For every parent row the
    // cascade touches, SQLite runs an internal `child_col = ?` lookup on
    // each referencing child table; a child column with no leftmost-prefix
    // index makes that lookup a FULL TABLE SCAN, so a small workspace's
    // delete is amplified by GLOBAL table sizes (measured on production
    // data: 14.3s for 2 conversations / 156 messages; 9.9ms once indexed —
    // #task:1674). This migration indexes every child key on the
    // workspace-delete cascade tree that lacked leftmost coverage; child
    // keys reachable only via other delete paths (agent/user deletes) are
    // deliberately NOT included here.
    //
    // Per-message amplifiers (scanned once per deleted message):
    //   - messages.thread_root_id       (self-FK; existing idx starts with
    //                                    conversation_id — unusable for the
    //                                    bare `thread_root_id = ?` FK probe)
    //   - conversation_inboxes.message_id (only 3rd position in the unique)
    //   - notifications.message_id / source_message_id (SET NULL)
    //   - saved_messages.message_id     (only 2nd position in the unique)
    //   - conversation_reads.thread_root_id / conversation_stream_heads.
    //     thread_root_id (only non-leftmost positions in partial uniques)
    // Per-conversation amplifiers (scanned once per deleted conversation):
    //   - conversation_stream_heads.conversation_id (covered ONLY by two
    //     partial uniques whose predicates a bare FK probe does not imply)
    //   - thread_followers.conversation_id (only 1st col of PK is
    //     thread_root_id; conversation_id uncovered)
    //   - saved_messages.conversation_id
    // Per-workspace amplifiers (scanned once per deleted workspace):
    //   - notifications.workspace_id (CASCADE) / source_workspace_id (SET NULL)
    //
    // Full (non-partial) indexes throughout: FK enforcement probes are
    // internal statements — coverage must not depend on partial-index
    // predicate implication.
    up: `
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
      CREATE INDEX idx_saved_messages_conversation
        ON saved_messages(conversation_id);
      CREATE INDEX idx_saved_messages_message
        ON saved_messages(message_id);
      CREATE INDEX idx_conversation_reads_thread_root
        ON conversation_reads(thread_root_id);
      CREATE INDEX idx_conversation_stream_heads_conversation
        ON conversation_stream_heads(conversation_id);
      CREATE INDEX idx_conversation_stream_heads_thread_root
        ON conversation_stream_heads(thread_root_id);
      CREATE INDEX idx_thread_followers_conversation
        ON thread_followers(conversation_id);
    `,
  },
  {
    version: 62,
    name: 'attachment_content_revision',
    up: `
      -- Attachment update capability (#task:1767, docs/attachments-design.md).
      -- An attachment id is now a STABLE reference whose bytes the creator may
      -- replace in place; it no longer promises id→bytes永久不变. content_revision
      -- is a monotonic freshness/cache-bust token (NOT a version system — no
      -- history is retained, no rollback): it advances on every successful
      -- content replace so a renderer can key its blob cache / inflight fetch on
      -- (id, revision) and an \`attachment_changed\` event can bump the client's
      -- generation. It is NOT an HTTP validator (serve is \`no-store\`) and it is
      -- NOT carried in a message's attachment snapshot (the snapshot stays a
      -- point-in-time {id,filename,size,content_type}; live authority is the DTO).
      -- Every existing row starts at 1 via the DEFAULT so there is no
      -- "missing = 1" compat branch — the column is authoritative from day one.
      ALTER TABLE attachments ADD COLUMN content_revision INTEGER NOT NULL DEFAULT 1;
    `,
  },
  {
    version: 63,
    name: 'message_author_snapshot',
    up: `
      -- #task:1791 — messages.from_agent_id / from_user_id are ON DELETE SET
      -- NULL, so hard-deleting an actor wipes the id the renderer joins on and
      -- every historical message by that actor degrades to a generic "@agent".
      -- These columns are a SEND-TIME author snapshot that OUTLIVES the actor:
      --
      --   author_id            — the authoring actor's id, WITH NO FOREIGN KEY.
      --                          It must NOT be nulled when the actor row is
      --                          deleted; it is the stable, non-collapsing
      --                          aggregation key that lets the server render an
      --                          identifiable, explicitly-"deleted" byline
      --                          instead of a generic one.
      --   author_name          — the actor's @handle at send time.
      --   author_display_name  — the actor's display name at send time.
      --
      -- No avatar bytes are snapshotted (a deleted author renders a neutral
      -- avatar). Server resolution (MessageAuthorDto):
      --   from_*_id NOT NULL            -> 'active'     (FK intact ⇔ actor lives;
      --                                                  read CURRENT name/avatar)
      --   from_*_id NULL + author_id     -> 'deleted'   (snapshot names it)
      --   from_*_id NULL + no author_id  -> 'unresolved'(legacy row whose actor
      --                                                  was deleted before this
      --                                                  snapshot existed; never
      --                                                  guessed as 'deleted')
      ALTER TABLE messages ADD COLUMN author_id           TEXT;
      ALTER TABLE messages ADD COLUMN author_name         TEXT;
      ALTER TABLE messages ADD COLUMN author_display_name TEXT;

      -- Backfill still-resolvable history from the live authority so a message
      -- whose actor still exists gets a snapshot now (and flips from 'active' to
      -- 'deleted' cleanly if that actor is later deleted). NOTE the fidelity
      -- boundary: for messages sent from v63 onward the snapshot is captured at
      -- SEND time (the actor's identity at the moment the row was written); this
      -- one-time backfill instead captures each surviving actor's CURRENT identity
      -- at MIGRATION time — a best-effort reconstruction, since no send-time value
      -- was ever recorded for pre-v63 rows. Rows whose actor was ALREADY
      -- hard-deleted have from_*_id = NULL and nothing to copy from → author_id
      -- stays NULL → 'unresolved' (we cannot fabricate an identity that was never
      -- captured). System rows keep all three NULL. agents.name / users.name /
      -- display_name are all NOT NULL, so a matched actor always yields non-null
      -- author_name / author_display_name.
      UPDATE messages
        SET author_id           = from_agent_id,
            author_name         = (SELECT a.name         FROM agents a WHERE a.id = messages.from_agent_id),
            author_display_name = (SELECT a.display_name FROM agents a WHERE a.id = messages.from_agent_id)
        WHERE from_kind = 'agent' AND from_agent_id IS NOT NULL;

      UPDATE messages
        SET author_id           = from_user_id,
            author_name         = (SELECT u.name         FROM users u WHERE u.id = messages.from_user_id),
            author_display_name = (SELECT u.display_name FROM users u WHERE u.id = messages.from_user_id)
        WHERE from_kind = 'user' AND from_user_id IS NOT NULL;
    `,
  },
  {
    version: 64,
    name: 'drop_workspace_local_binding_columns',
    // relocation-v1 §6/§7 (#task:1807 + #task:1808) — retire BOTH columns that
    // encoded a workspace's host-local binding: `workspace_path` (the legacy
    // filesystem path, denormalized onto every conversation) and
    // `workspaces.workspace_host_id` (the owning-host pointer). A regular
    // workspace no longer owns a working directory OR a home host: its bundle
    // resolves to a TERMINAL deterministic agent-centric coordinate
    // (`<agent home>/workspaces/<workspace_id>`), computed the same way on every
    // host. There is no DB column — and no registry lookup — the backend
    // consults to derive a path or an owning host (invariant I1). Per §6 the two
    // are ONE removal set landing in ONE release tree under ONE triple bump, so
    // they retire in a SINGLE `workspaces` rebuild — never a v64(path)+v65(host)
    // pair that rebuilds the parent twice and leaves an intermediate schema that
    // was never a legal release.
    //
    // This migration is UNPUBLISHED — both drops were forward-fixed IN PLACE
    // across #task:1807 (`workspace_path`) and #task:1808 (`workspace_host_id`)
    // into this one rebuild; history was never amended and no sibling migration
    // was appended. Any such staging marker stays in THIS comment, never inside
    // the DDL string: a comment inside a CREATE TABLE is preserved verbatim in
    // `sqlite_master.sql` and would corrupt the schema fingerprint.
    //
    // Two SQLite table rebuilds (the columns are NOT NULL and can't be dropped
    // in place; template = v54 `drop_conversation_parent_column`): the child
    // `conversations` is rebuilt first, then the parent `workspaces`. With
    // `foreignKeyEscape` the runner disables `foreign_keys` around the whole
    // migration and runs `foreign_key_check` afterward, so dropping the parent
    // never cascade-deletes children and every surviving FK is re-validated.
    // No precondition: we are discarding populated columns, not asserting
    // emptiness. Every surviving column, constraint, and index is reproduced
    // explicitly.
    foreignKeyEscape: true,
    up: `
      CREATE TABLE conversations_new (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        kind                    TEXT NOT NULL CHECK (kind IN ('channel', 'dm_channel')),
        title                   TEXT NOT NULL,
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
            AND (visibility = 'private' OR kind = 'channel')
          )
      );

      INSERT INTO conversations_new (
        id, workspace_id, kind, title, status,
        spawned_by_agent_id, closed_at, close_summary, pinned_at,
        created_at, updated_at, archived_at, latest_message_seq, visibility
      )
        SELECT
          id, workspace_id, kind, title, status,
          spawned_by_agent_id, closed_at, close_summary, pinned_at,
          created_at, updated_at, archived_at, latest_message_seq, visibility
        FROM conversations;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, kind, updated_at DESC);
      CREATE INDEX idx_conversations_status
        ON conversations(status);
      CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;

      CREATE TABLE workspaces_new (
        id                 TEXT PRIMARY KEY,
        name               TEXT NOT NULL,
        description        TEXT,
        workspace_kind     TEXT NOT NULL
          CHECK (workspace_kind IN ('regular', 'agent_home')),
        owner_agent_id     TEXT UNIQUE REFERENCES agents(id) ON DELETE CASCADE,
        archived_at        INTEGER,
        created_at         INTEGER NOT NULL,
        updated_at         INTEGER NOT NULL,
        pinned_at          INTEGER,
        owner_user_id      TEXT REFERENCES users(id),
        CHECK (
          (workspace_kind = 'agent_home' AND owner_agent_id IS NOT NULL)
          OR
          (workspace_kind = 'regular' AND owner_agent_id IS NULL)
        )
      );

      INSERT INTO workspaces_new (
        id, name, description, workspace_kind,
        owner_agent_id, archived_at, created_at, updated_at, pinned_at,
        owner_user_id
      )
        SELECT
          id, name, description, workspace_kind,
          owner_agent_id, archived_at, created_at, updated_at, pinned_at,
          owner_user_id
        FROM workspaces;

      DROP TABLE workspaces;
      ALTER TABLE workspaces_new RENAME TO workspaces;

      CREATE UNIQUE INDEX workspaces_regular_name_unique
        ON workspaces(name) WHERE workspace_kind = 'regular';
      CREATE INDEX idx_workspaces_pinned_at
        ON workspaces(workspace_kind, pinned_at) WHERE pinned_at IS NOT NULL;
    `,
  },
  {
    version: 65,
    name: 'attention_mute_tier_and_workspace_suppressor',
    // #task:1870 (docs/activity-lifecycle-v1.md §6/§8/§9.1) — the three attention
    // tiers. Two schema facts land together because they are ONE product decision
    // and each is meaningless alone:
    //
    //   1. `channel_attention_preferences.mode` gains a third value, `'mute'`.
    //      The channel scope is ONE mutually-exclusive choice, so it stays a
    //      single enum: `mode` + a separate `muted` boolean would only need an
    //      ADD COLUMN, but it would make the illegal state
    //      (`muted` AND ambient on) EXPRESSIBLE and push the invariant from the
    //      type surface onto the render surface. SQLite cannot ALTER a CHECK, so
    //      the price of that guarantee is this table rebuild — paid once.
    //
    //   2. `workspace_attention_preferences` — the per-(viewer, workspace) mute
    //      SUPPRESSOR. Deliberately a BOOLEAN, not a fourth tier and not an
    //      overridable default: it sits ABOVE the channel tier, so un-muting a
    //      workspace restores every channel's own stored mode unchanged. That is
    //      what makes this a pure schema addition: no backfill, no inherit axis,
    //      and not one existing row is re-read or rewritten.
    //
    // The suppressor gets its OWN table rather than a column on `workspace_users`
    // (which governance also writes): it is a viewer's attention decision, so it
    // is owned state under the same 建表准入 test (§4.2) that admitted the channel
    // preference, and it must not inherit membership's lifecycle. Attention now
    // owns exactly TWO write-side aggregates; it still stores no read state and
    // still never touches delivery (§5.2 — no thread_followers, no wake).
    //
    // Scope guards mirror the channel table's `channel_only` backstop: a pref on
    // an `agent_home` workspace is meaningless dead state (a DM is
    // directed-grade — it cannot be muted, and it has no ambient tier to
    // suppress), so the DB fails loud instead of storing it.
    up: `
      CREATE TABLE channel_attention_preferences_new (
        user_id          TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        mode             TEXT NOT NULL CHECK (mode IN ('all', 'mentions', 'mute')),
        updated_at       INTEGER NOT NULL,
        PRIMARY KEY (user_id, conversation_id)
      );

      INSERT INTO channel_attention_preferences_new (
        user_id, conversation_id, mode, updated_at
      )
        SELECT user_id, conversation_id, mode, updated_at
        FROM channel_attention_preferences;

      DROP TABLE channel_attention_preferences;
      ALTER TABLE channel_attention_preferences_new
        RENAME TO channel_attention_preferences;

      CREATE INDEX idx_channel_attention_prefs_conversation
        ON channel_attention_preferences(conversation_id);

      CREATE TRIGGER channel_attention_prefs_channel_only_insert
        BEFORE INSERT ON channel_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT kind FROM conversations WHERE id = NEW.conversation_id), ''
            ) != 'channel'
            THEN RAISE(ABORT, 'channel_attention_preferences.conversation_id must reference a regular channel')
          END;
        END;

      CREATE TRIGGER channel_attention_prefs_channel_only_update
        BEFORE UPDATE OF conversation_id ON channel_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT kind FROM conversations WHERE id = NEW.conversation_id), ''
            ) != 'channel'
            THEN RAISE(ABORT, 'channel_attention_preferences.conversation_id must reference a regular channel')
          END;
        END;

      CREATE TABLE workspace_attention_preferences (
        user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        workspace_id  TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        muted         INTEGER NOT NULL CHECK (muted IN (0, 1)),
        updated_at    INTEGER NOT NULL,
        PRIMARY KEY (user_id, workspace_id)
      );

      CREATE INDEX idx_workspace_attention_prefs_workspace
        ON workspace_attention_preferences(workspace_id);

      CREATE TRIGGER workspace_attention_prefs_regular_only_insert
        BEFORE INSERT ON workspace_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT workspace_kind FROM workspaces WHERE id = NEW.workspace_id), ''
            ) != 'regular'
            THEN RAISE(ABORT, 'workspace_attention_preferences.workspace_id must reference a regular workspace')
          END;
        END;

      CREATE TRIGGER workspace_attention_prefs_regular_only_update
        BEFORE UPDATE OF workspace_id ON workspace_attention_preferences
        BEGIN
          SELECT CASE
            WHEN COALESCE(
              (SELECT workspace_kind FROM workspaces WHERE id = NEW.workspace_id), ''
            ) != 'regular'
            THEN RAISE(ABORT, 'workspace_attention_preferences.workspace_id must reference a regular workspace')
          END;
        END;
    `,
  },
  {
    version: 66,
    name: 'task_board_ordered_access_paths',
    up: `
      -- #task:1884 — the human task board reads one status column at a time as a
      -- keyset window (ORDER BY task_number DESC, id DESC LIMIT n). Bounding the
      -- ROWS the client renders is only half of it: without an index that is
      -- already in that order, every window still sorts the whole column, so the
      -- authority's cost stays proportional to the population, not to the page.
      --
      -- Two access paths because the board has two real query shapes and one
      -- composite cannot serve both: the DEFAULT view is cross-workspace
      -- ("All workspaces"), where a leading workspace_id column is unusable, and
      -- the narrowed view filters one workspace first.
      --
      -- Column order mirrors the queries exactly: status equality, then the sort
      -- key. The trailing id column is the unique tiebreak that makes the keyset
      -- a TOTAL order — task_number is monotonic only per workspace
      -- (uniq_tasks_workspace_number), so on a cross-workspace board two tasks
      -- can share a number and a number-only cursor could skip or repeat rows.
      CREATE INDEX idx_tasks_status_number
        ON tasks(status, task_number DESC, id DESC);

      CREATE INDEX idx_tasks_workspace_status_number
        ON tasks(workspace_id, status, task_number DESC, id DESC);

      -- Superseded: idx_tasks_workspace_status(workspace_id, status) is a strict
      -- PREFIX of the index above, so every lookup it served is served
      -- identically — keeping it would only add a second b-tree to maintain on
      -- every task write.
      DROP INDEX idx_tasks_workspace_status;
    `,
  },
  {
    version: 67,
    name: 'conversation_reads_stream_cursor_lookup',
    up: `
      -- #task:1967 — the unread read-model resolves a cursor per STREAM
      -- (user_id, conversation_id, thread_root_id), where the stream is a
      -- variable, not a literal: the per-conversation and Activity queries walk
      -- a set of streams and look up each one's cursor.
      --
      -- The two partial uniques (uniq_conversation_reads_main /
      -- uniq_conversation_reads_thread) cannot serve that lookup. A partial
      -- index is usable only when the WHERE clause IMPLIES its predicate, and a
      -- dynamic root implies neither "thread_root_id IS NULL" nor
      -- "IS NOT NULL" — so the planner fell back to
      -- idx_conversation_reads_conversation(conversation_id) and filtered rows,
      -- once per candidate message.
      --
      -- Hence a FULL (non-partial) index with the exact column order the lookup
      -- binds. It carries NO uniqueness contract: the two partial uniques keep
      -- owning mainline-NULL / thread-non-NULL uniqueness, and this index is
      -- purely an access path. Leading user_id because every reader is
      -- viewer-scoped; conversation_id then thread_root_id so a per-conversation
      -- prefix scan is available as well as the full three-column probe.
      --
      -- The two existing single-column indexes stay: they cover the FK/cascade
      -- probes (conversation_id, thread_root_id) that this index's leading
      -- user_id cannot serve.
      CREATE INDEX idx_conversation_reads_user_conversation_stream
        ON conversation_reads(user_id, conversation_id, thread_root_id);
    `,
  },
  {
    version: 68,
    name: 'agent_deletion_intent',
    up: `
      -- Durable, fail-forward intent for terminal agent deletion. NULL is the
      -- only active state; the timestamp is also the CAS token/audit fact.
      ALTER TABLE agents ADD COLUMN deletion_started_at INTEGER;
    `,
  },
  {
    version: 69,
    name: 'conversation_placement_and_roster_policy',
    // agent-scoped-session-v1 §4.1 / §8 P0-a (#task:2012) — split the overloaded
    // `conversations.kind` into the two independent axes it was conflating:
    //
    //   placement_kind  WHERE the conversation lives / who its participants are
    //                   derived from: workspace | direct_pair | agent_inbox
    //                   (an exhaustive, mutually exclusive arc set)
    //   roster_policy   WHETHER membership can change: open | fixed
    //
    // `kind` is NOT dropped here and is no longer a written column: it becomes
    // GENERATED ALWAYS ... VIRTUAL over `roster_policy`, so the 118 existing read
    // points and the wire literal keep working UNCHANGED while the DB itself
    // proves the two can never drift — writing `kind` now fails at PARSE time
    // ("cannot INSERT into generated column"), which is why the TS side has no
    // mirrored derivation to keep in sync. Its removal is later one
    // `DROP COLUMN kind` (§8 P6) once the read points are retired.
    //
    // This is a TABLE REBUILD, not `ALTER TABLE ADD COLUMN`: an existing plain
    // column cannot become generated in place (SQLite has no alter-column-to-
    // generated, `ADD COLUMN kind` collides with the existing name, and
    // `DROP COLUMN kind` is refused because the table's `visibility` CHECK
    // references it). Template = the v64 `conversations` rebuild directly above.
    // With `foreignKeyEscape` the runner disables `foreign_keys` around the whole
    // migration and runs `foreign_key_check` afterward, so `DROP TABLE
    // conversations` cannot cascade-delete children and every surviving FK is
    // re-validated. Every surviving column, constraint, and index is reproduced
    // explicitly — a rebuild's failure mode is SILENTLY losing one of them, so
    // the falsifiers assert the full normalized schema, not a sample.
    //
    // Two derived rewrites, both behavior-preserving by construction:
    //   * the `visibility` cross-column CHECK now reads `roster_policy = 'open'`
    //     instead of `kind = 'channel'`. That is the FAITHFUL rewrite: the rule
    //     it encodes is "only an open-roster conversation may be public"
    //     (placement is irrelevant — a public DirectPair is meaningless because
    //     its roster is fixed, not because of where it lives).
    //   * `idx_conversations_workspace` keeps indexing `kind`: indexes on a
    //     VIRTUAL generated column are legal and the read plans that bind
    //     `kind = ?` must not change in this migration.
    //
    // Backfill is total and derived only from the retiring column's own domain
    // (the old CHECK admitted exactly two values): every existing row is a
    // workspace-placed conversation — 'channel' rows have an open roster,
    // 'dm_channel' rows a fixed one. The 14 live `dm_channel` rows therefore stay
    // private with a byte-identical derived `kind`; no row changes any other
    // column, so behavior is row-for-row unchanged.
    //
    // `CHECK (placement_kind = 'workspace')` is deliberate and NOT dead weight:
    // at this version a non-workspace placement is not REPRESENTABLE (there is no
    // `pair_key` / `inbox_agent_id` column yet and `workspace_id` is still
    // NOT NULL), so declaring the wider enum without this gate would make the
    // column's declared domain a lie. It also makes the P0-b/P3 audit
    // unavoidable: the migration that first admits a `direct_pair` or
    // `agent_inbox` row must relax this CHECK, and that is exactly the point at
    // which the 10 `JOIN workspaces` read surfaces have to be re-checked for
    // silently dropped rows.
    foreignKeyEscape: true,
    up: `
      CREATE TABLE conversations_new (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        placement_kind          TEXT NOT NULL
          CHECK (placement_kind IN ('workspace', 'direct_pair', 'agent_inbox')),
        roster_policy           TEXT NOT NULL
          CHECK (roster_policy IN ('open', 'fixed')),
        kind                    TEXT GENERATED ALWAYS AS
          (CASE WHEN roster_policy = 'open' THEN 'channel' ELSE 'dm_channel' END) VIRTUAL,
        title                   TEXT NOT NULL,
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
        CHECK (placement_kind = 'workspace')
      );

      INSERT INTO conversations_new (
        id, workspace_id, placement_kind, roster_policy, title, status,
        spawned_by_agent_id, closed_at, close_summary, pinned_at,
        created_at, updated_at, archived_at, latest_message_seq, visibility
      )
        SELECT
          id, workspace_id,
          'workspace',
          CASE WHEN kind = 'channel' THEN 'open' ELSE 'fixed' END,
          title, status,
          spawned_by_agent_id, closed_at, close_summary, pinned_at,
          created_at, updated_at, archived_at, latest_message_seq, visibility
        FROM conversations;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, kind, updated_at DESC);
      CREATE INDEX idx_conversations_status
        ON conversations(status);
      CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;
    `,
  },
  {
    version: 70,
    name: 'runtime_slots_scoped_authority',
    up: `
      -- #task:2013 (P0-d) — runtime authority is not membership.
      --
      -- session_scope lands dormant: every existing/new product path still
      -- creates conversation-scoped agents, while P2 later activates the
      -- agent-scoped creation/runtime behavior. The DB invariant lands now so
      -- there is never an intermediate schema that can accept a slot whose
      -- scope disagrees with its owning agent.
      ALTER TABLE agents ADD COLUMN session_scope TEXT NOT NULL
        DEFAULT 'conversation'
        CHECK (session_scope IN ('conversation', 'agent'));

      CREATE TRIGGER agents_session_scope_immutable
        BEFORE UPDATE OF session_scope ON agents
        WHEN NEW.session_scope != OLD.session_scope
        BEGIN
          SELECT RAISE(ABORT, 'agents.session_scope is immutable');
        END;

      -- Preserve the runtime payload while rebuilding conversation_agents as
      -- the membership/ACL edge it actually is. The staging table is local to
      -- this migration and is dropped before commit.
      CREATE TEMP TABLE runtime_slot_stage AS
        SELECT conversation_id, agent_id, cli_session_id, pid, status
          FROM conversation_agents;

      CREATE TABLE conversation_agents_new (
        conversation_id TEXT NOT NULL
          REFERENCES conversations(id) ON DELETE CASCADE,
        agent_id        TEXT NOT NULL
          REFERENCES agents(id) ON DELETE CASCADE,
        joined_at       INTEGER NOT NULL,
        PRIMARY KEY (conversation_id, agent_id)
      );

      INSERT INTO conversation_agents_new (
        conversation_id, agent_id, joined_at
      )
        SELECT conversation_id, agent_id, joined_at
          FROM conversation_agents;

      DROP TABLE conversation_agents;
      ALTER TABLE conversation_agents_new RENAME TO conversation_agents;

      CREATE INDEX idx_conversation_agents_agent
        ON conversation_agents(agent_id);

      -- One stable CURRENT slot per runtime scope. daemon_instance_id is an
      -- observation/custody field, never part of identity. Two partial unique
      -- indexes are required because SQLite NULL values do not deduplicate in
      -- a composite UNIQUE constraint.
      CREATE TABLE agent_runtime_slots (
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
        cli_session_id                TEXT,
        FOREIGN KEY (conversation_id, agent_id)
          REFERENCES conversation_agents(conversation_id, agent_id)
          ON DELETE CASCADE
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

      -- Slot scope is identity, not mutable placement. A caller that wants a
      -- different scope creates a different immutable Agent.
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

      INSERT INTO agent_runtime_slots (
        agent_id, conversation_id, generation,
        claimed_by_daemon_instance_id,
        observed_status, observed_pid, cli_session_id
      )
        SELECT agent_id, conversation_id, 0, NULL,
               status, pid, cli_session_id
          FROM runtime_slot_stage;

      -- Future membership rows create their conversation-scoped slot in the
      -- same statement transaction. Agent-scoped memberships deliberately do
      -- not create N conversation slots; their single slot is created with the
      -- Agent row by the next trigger.
      CREATE TRIGGER conversation_agents_create_runtime_slot
        AFTER INSERT ON conversation_agents
        WHEN (SELECT session_scope FROM agents WHERE id = NEW.agent_id)
             = 'conversation'
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

      -- Historical generations and projection transitions are append-only.
      -- Current-slot reads never consult this table.
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

      INSERT INTO agent_runtime_events (
        slot_id, agent_id, conversation_id,
        generation, event_kind, daemon_instance_id,
        observed_status, observed_pid, cli_session_id, occurred_at
      )
        SELECT id, agent_id, conversation_id,
               generation, 'slot_migrated',
               claimed_by_daemon_instance_id,
               observed_status, observed_pid, cli_session_id,
               CAST(strftime('%s', 'now') AS INTEGER) * 1000
          FROM agent_runtime_slots;

      DROP TABLE runtime_slot_stage;
    `,
  },
  {
    version: 71,
    name: 'agent_inbox_placement',
    // agent-scoped-session-v1 §4.1 / §4.2 末 / §5 / §8 P0-b (#task:2014) — the
    // `Mentions` inbox becomes a first-class placement instead of a DM that
    // borrows an `agent_home` workspace, and the mention CC copy becomes part of
    // the source message's own transaction.
    //
    // Version 71, not 70: 70 is reserved by the parallel P0-d runtime-slot lane
    // (#task:2013). Whichever of the two lands second renumbers so the SHIPPED
    // sequence stays dense — a gap would let a DB record 71 and then never apply
    // 70 (the runner only replays versions above the ledger).
    //
    // Three things change, and only the first needs a rebuild:
    //
    // 1. `conversations`: `workspace_id` becomes NULLABLE, `inbox_agent_id`
    //    appears, and v69's staging gate `CHECK (placement_kind = 'workspace')`
    //    is replaced by the EXHAUSTIVE MUTUALLY EXCLUSIVE ARC — each placement
    //    names exactly the columns it owns and NULLs every other arc's columns.
    //    `direct_pair` stays fail-closed (its endpoint columns arrive in P3):
    //    admitting the value without its columns would be a half-modelled arc,
    //    which is the same lie v69's gate existed to prevent.
    //    A rebuild is forced twice over: `workspace_id NOT NULL` cannot be
    //    relaxed in place, and the retiring table-level CHECK cannot be dropped
    //    in place either.
    // 2. `messages.cc_source_message_id`: plain nullable ADD COLUMN plus a
    //    PARTIAL UNIQUE index — the dedupe proof for "exactly one copy per
    //    (source message, inbox)". It deliberately carries NO foreign key
    //    (§4.2 末): the copy is an independent fact in the recipient's inbox, so
    //    deleting the source conversation must not delete notifications other
    //    principals already received.
    // 3. One BRAND-NEW EMPTY inbox per existing agent. The legacy `Mentions`
    //    DMs are NOT converted and NOT touched — every column, message,
    //    attachment, task and roster row of theirs survives this migration
    //    byte-for-byte (@Cindy 裁定 (b), 2026-08-14).
    //
    //    Converting them was the obvious move and it is wrong on the live data.
    //    Measured before deciding: the three `Mentions` rows hold 137 messages,
    //    7 attachments, 9 tasks and 3 human roster rows, and one of them (a
    //    human ↔ agent private line, last active 2026-08-09, 32 human messages)
    //    is a WORKING SURFACE, not the notification stream §4.2 models. An
    //    `agent_inbox` has no human participant by construction, so converting
    //    that row would cut its human out of his own conversation and
    //    retroactively reclassify 9 tasks. The general rule this follows is the
    //    one the doc already states: legacy stays legacy, a new concept takes
    //    only new traffic. The legacy rows keep their formal home (an ordinary
    //    `workspace` + `fixed` conversation) permanently — P3 gives the private
    //    line a DirectPair for NEW traffic and still does not migrate these rows.
    //
    //    So there is no title heuristic, no `agent_home` derivation and no
    //    oldest-per-agent tie-break here: the inbox is an INVARIANT OF THE
    //    `agents` ROW, so its existence is derived from `agents`, and every agent
    //    gets exactly one.
    //
    //    THE ROW, NOT THE ACTIVE GRAPH — which is why this SELECT has no
    //    `materialization_status` filter. The lifecycle admits
    //    `pending_materialization | active | failed`, and the inbox depends on
    //    NONE of what materialization produces: no daemon ack, no agent dir, no
    //    host, no workspace. What it does depend on is the thing the reserve
    //    INSERT already established — a name in the global @handle namespace,
    //    which `AgentRepo.getByName` resolves regardless of status. So a mention
    //    of a reserved-but-failed handle must have a destination, and the row
    //    that CASCADEs off `agents(id)` must be born with the row it dies with.
    //    The reserve wrote the pair in ONE transaction (never the `pending →
    //    active` flip: a `failed` row keeps its inbox, so a retry's dependent-row
    //    tx must not try to insert a second one); this backfill is that same
    //    statement for the agents that already exist. Both halves of THAT
    //    sentence are history as of #task:2072 — the reserve is now a single
    //    `agents` INSERT (`reserveAgentRow`) and no arc of the create path mints
    //    an inbox — so v71's population is closed at exactly the agents that
    //    existed on upgrade day. What follows explains the rows this migration
    //    made; it is not a rule any newer row obeys.
    //
    //    `created_at`/`updated_at` = the agent's own `created_at`, the SAME rule
    //    the aggregate factory applies: this timestamp expresses AGENT-AGGREGATE
    //    BIRTH, not when a dependent row happened to land, so an upgraded and a
    //    freshly created inbox are indistinguishable. A truthful old timestamp
    //    also keeps an empty inbox at the bottom of `updated_at DESC` lists
    //    instead of jumping every roster to the top on upgrade day.
    //
    //    The id is a NORMAL conversation id, minted by the production generator
    //    in the precondition and staged in a temp table for `up` to join. It is
    //    deliberately NOT derived from the agent id: `conversations.id` is a
    //    domain value object — 12 random bytes as 16 chars of base64url — and the
    //    canonical `#conv:<id>` grammar accepts exactly `[A-Za-z0-9_-]{16}`, so a
    //    22-char `inbox-<agentId>` would be a conversations row that the system's
    //    own conversation reference cannot express (invisible only while the inbox
    //    has no discovery surface; #task:2019 gives it one). The `wsah_<agentId>`
    //    precedent does not transfer: that convention exists because the daemon
    //    must RECOGNIZE an agent-home id structurally with no kind column and no
    //    path, whereas nothing recognizes an inbox from its id —
    //    `placement_kind` + `inbox_agent_id` + the partial UNIQUE carry every
    //    fact, including one-per-agent. Replayability to identical bytes was never
    //    a requirement either: a migration is applied once per DB, inside one
    //    transaction, and the ledger row is written with it.
    //    NO `conversation_agents` row is written: on this arc the participant IS
    //    `inbox_agent_id` (§4.2 表). A projected roster row would be a second
    //    authority for the same fact and would re-create the cascade conflict
    //    that table's triggers caused.
    //
    // `inbox_agent_id ... ON DELETE CASCADE` is an OWNERSHIP edge, not a
    // historical coordinate: an inbox has exactly one owner and no survivor, so
    // it is part of the Agent aggregate and dies with it. This does not add a
    // cascade — it SHORTENS today's chain by one hop (`workspaces.owner_agent_id`
    // → `conversations.workspace_id` → messages), which is how deleting an agent
    // already removed its Mentions. Now that an inbox has no workspace, that
    // cascade has to come from the owner edge itself.
    foreignKeyEscape: true,
    // Mint one conversation id per agent with the PRODUCTION generator and stage
    // the pairs for `up` to join. SQL cannot call the generator, and the id
    // domain must not grow a second one (see the note above), so the ids are
    // made here — inside the migration's own transaction, on the same
    // connection, before the rebuild — and consumed by the INSERT below. A
    // rollback discards the temp table with everything else; `DROP … IF EXISTS`
    // makes the next boot's retry start clean.
    precondition: (db) => {
      const handle = db as {
        exec: (sql: string) => void;
        prepare: (sql: string) => {
          all: () => { id: string }[];
          run: (...params: unknown[]) => void;
        };
      };
      handle.exec(
        `DROP TABLE IF EXISTS temp.agent_inbox_backfill;
         CREATE TEMP TABLE agent_inbox_backfill (
           agent_id        TEXT PRIMARY KEY,
           conversation_id TEXT NOT NULL UNIQUE
         );`,
      );
      const stage = handle.prepare(
        `INSERT INTO temp.agent_inbox_backfill (agent_id, conversation_id)
         VALUES (?, ?)`,
      );
      for (const agent of handle.prepare('SELECT id FROM agents').all()) {
        stage.run(agent.id, randomBytes(12).toString('base64url'));
      }
    },
    up: `
      CREATE TABLE conversations_new (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
        inbox_agent_id          TEXT REFERENCES agents(id) ON DELETE CASCADE,
        placement_kind          TEXT NOT NULL
          CHECK (placement_kind IN ('workspace', 'direct_pair', 'agent_inbox')),
        roster_policy           TEXT NOT NULL
          CHECK (roster_policy IN ('open', 'fixed')),
        kind                    TEXT GENERATED ALWAYS AS
          (CASE WHEN roster_policy = 'open' THEN 'channel' ELSE 'dm_channel' END) VIRTUAL,
        title                   TEXT NOT NULL,
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
        CHECK (
          CASE placement_kind
            WHEN 'workspace'
              THEN workspace_id IS NOT NULL AND inbox_agent_id IS NULL
            WHEN 'agent_inbox'
              THEN inbox_agent_id IS NOT NULL AND workspace_id IS NULL
                   AND roster_policy = 'fixed'
            ELSE 0
          END
        )
      );

      INSERT INTO conversations_new (
        id, workspace_id, inbox_agent_id, placement_kind, roster_policy,
        title, status, spawned_by_agent_id, closed_at, close_summary, pinned_at,
        created_at, updated_at, archived_at, latest_message_seq, visibility
      )
        SELECT
          c.id, c.workspace_id, NULL, 'workspace', c.roster_policy,
          c.title, c.status, c.spawned_by_agent_id, c.closed_at, c.close_summary,
          c.pinned_at, c.created_at, c.updated_at, c.archived_at,
          c.latest_message_seq, c.visibility
        FROM conversations c;

      INSERT INTO conversations_new (
        id, workspace_id, inbox_agent_id, placement_kind, roster_policy,
        title, status, spawned_by_agent_id, closed_at, close_summary, pinned_at,
        created_at, updated_at, archived_at, latest_message_seq, visibility
      )
        SELECT
          b.conversation_id, NULL, a.id, 'agent_inbox', 'fixed',
          'Inbox', 'idle', NULL, NULL, NULL, NULL,
          a.created_at, a.created_at, NULL, 0, 'private'
        FROM agents a
        JOIN temp.agent_inbox_backfill b ON b.agent_id = a.id;

      DROP TABLE temp.agent_inbox_backfill;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, kind, updated_at DESC);
      CREATE INDEX idx_conversations_status
        ON conversations(status);
      CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;
      CREATE UNIQUE INDEX idx_conversations_inbox_agent
        ON conversations(inbox_agent_id) WHERE placement_kind = 'agent_inbox';

      ALTER TABLE messages ADD COLUMN cc_source_message_id TEXT;
      CREATE UNIQUE INDEX idx_messages_cc_dedupe
        ON messages(cc_source_message_id, conversation_id)
        WHERE cc_source_message_id IS NOT NULL;
    `,
  },
  {
    version: 72,
    name: 'conversation_title_address',
    // agent-scoped-session-v1 §8 P0-c (#task:2016) — a workspace conversation's
    // DISPLAY TITLE becomes its ADDRESS. There is no second, semantic name: the
    // thing a person reads in the sidebar is the thing `<silverhand://…>`
    // resolves, so a channel can never show one name and answer to another
    // (shiki, 2026-08-15 — the ruling that retired this migration's first design,
    // an allocation table of slugs).
    //
    // The address is therefore not an entity. It is a UNIQUENESS RULE over a
    // column that already exists, and the whole rule is:
    //
    //   title_key = NFC(foldCase(NFC(trim(title))))
    //   UNIQUE (workspace_id, title_key) WHERE placement_kind = 'workspace'
    //
    // `title_key` is a PLAIN column, not GENERATED: SQLite has no Unicode case
    // folding, no NFC, and no Unicode whitespace class, so the fold cannot be
    // written as a SQL expression at all. That is the load-bearing consequence of
    // this design and it is stated here rather than discovered later — the index
    // proves only that STORED KEYS are unique. "Two channels in a workspace
    // cannot display the same title" is a conjunction of three properties, and
    // only the middle one is a database fact:
    //
    //   1. ONE canonicalizer (`canonicalizeStoredConversationTitle` in shared;
    //      the fold table is pinned data, versioned, not the host's ICU);
    //   2. a TERMINAL WRITER — title and key are writable only as a branded pair
    //      the caller cannot forge, so no write path can bring its own fold;
    //   3. a half-state that is DETECTABLE AT RUNTIME —
    //      `auditConversationTitleAddresses`, run by the precondition below, at
    //      boot, and on demand.
    //
    // A rename RELEASES the old title immediately, and nothing here remembers it:
    // no retired allocations, no reuse ban. Another channel may take it, and so
    // may the one that gave it up. `#conv:<id>` remains a permanent address for
    // every conversation, which is what makes the by-name address safe to lose.
    //
    // ARCHIVING DOES NOT RELEASE A TITLE. The index has no `archived_at IS NULL`
    // clause on purpose: an archived channel is restorable, and a restore that
    // could fail — or worse, silently collide — because someone took its name is
    // a worse surprise than "that name is still in use by an archived channel".
    //
    // Only WORKSPACE placements have an address. `title_key` is NULL for an
    // `agent_inbox` row and the arc CHECK below enforces exactly that, so a
    // resolver that forgot its placement predicate still cannot address an inbox
    // — the same reason v69 declared `CHECK (placement_kind = 'workspace')`
    // instead of trusting the writer.
    //
    // A TABLE REBUILD is required: the new column is NOT NULL for workspace rows,
    // it participates in the placement arc CHECK, and SQLite cannot alter a
    // CHECK. Template = the v69 / v71 rebuilds directly above; every surviving
    // column, constraint and index is reproduced explicitly, because a rebuild's
    // failure mode is silently losing one of them, and the falsifiers assert the
    // full normalized schema rather than a sample. `foreignKeyEscape` disables
    // `foreign_keys` around the migration and runs `foreign_key_check`
    // afterwards, so `DROP TABLE conversations` cannot cascade-delete children.
    //
    // NAMED BEHAVIOR — THIS MIGRATION CHANGES EXISTING DISPLAY TITLES. Where the
    // stored `title` was padded or not in NFC, the backfill writes the CANONICAL
    // form back: `title` is not byte-identical across v72 for those rows, and the
    // change is user-visible in every read surface (sidebar, header, export,
    // stdin) as well as to any external consumer that pinned the old bytes.
    // Concretely: `"  design  "` becomes `"design"`, and a decomposed `"café"`
    // (`e` + U+0301) becomes the composed one. Nothing is lost — no character is
    // dropped but edge whitespace, and NFC is canonical equivalence, so the two
    // spellings render identically in a conforming renderer.
    //
    // It is not a repair we could skip. Keying without it leaves rows where
    // `title <> canonical(title)`, which is a defect class the integrity scan must
    // then TOLERATE FOREVER on legacy data — and a tolerated class is a class the
    // fail-closed boot cannot use, so the one half-state that means "shows one
    // name, answers to another" would have to be judged by a scan that already
    // shrugs at un-normalized titles. Writing it back once, here, is what makes
    // `title = canonical(title)` a post-migration invariant with no legacy
    // exemption. RESIDUAL RISK: a row whose title differs only in normalization
    // changes bytes with no operator prompt; the migration's own backup (see
    // `onMigrationResult`'s `backupPath`) is the recovery path.
    foreignKeyEscape: true,
    precondition: (db) => {
      const handle = db as {
        exec: (sql: string) => void;
        prepare: (sql: string) => {
          all: () => {
            id: string;
            workspace_id: string;
            title: string;
          }[];
          run: (...params: unknown[]) => void;
        };
      };
      const rows = handle
        .prepare(
          `SELECT id, workspace_id, title
             FROM conversations
            WHERE placement_kind = 'workspace'
            ORDER BY workspace_id, created_at, id`,
        )
        .all();

      // FAIL LOUD, BEFORE the index exists. Two channels whose titles fold
      // together are legal data below this version, and the UNIQUE index is
      // about to refuse them with `UNIQUE constraint failed:
      // conversations.workspace_id, conversations.title_key` — a message that
      // names neither channel and no title. This precondition is the same
      // detector the boot scan and `admin scan-title-addresses` use, so an
      // operator can run it on a copy BEFORE upgrading and rename at leisure;
      // throwing here rolls the transaction back and leaves no `_migrations`
      // row, so the upgrade retries on the next boot once the data is clean.
      const audit = auditConversationTitleAddresses(
        rows.map((row) => ({
          id: row.id,
          workspaceId: row.workspace_id,
          title: row.title,
          titleKey: null,
        })),
        { keysExpected: false },
      );
      if (audit.defects.length > 0) {
        throw new Error(
          `migration v72 (#task:2016): a workspace holds conversations whose ` +
            `titles fold to one address. Rename one of each pair, then restart. ` +
            `\n${describeConversationTitleAudit(audit)}`,
        );
      }

      // The keys are minted HERE, in the migration's own transaction, on the
      // same connection, before `up` — SQL cannot fold, and a second
      // implementation of the fold is the one thing this design cannot afford
      // (an upgraded database and a freshly created channel would disagree about
      // what a title addresses). The staged `title` travels with it because
      // canonicalization may rewrite the display form.
      handle.exec(
        `DROP TABLE IF EXISTS temp.conversation_title_backfill;
         CREATE TEMP TABLE conversation_title_backfill (
           conversation_id TEXT PRIMARY KEY,
           title           TEXT NOT NULL,
           title_key       TEXT NOT NULL
         );`,
      );
      const stage = handle.prepare(
        `INSERT INTO temp.conversation_title_backfill
           (conversation_id, title, title_key)
         VALUES (?, ?, ?)`,
      );
      for (const row of rows) {
        // TOTAL, and deliberately not the validator: rows written under the old
        // `trim().min(1).max(128)` rule may hold titles the new domain refuses
        // (a ZWJ emoji sequence, say). Keying every one of them is what keeps
        // the migration from failing on legal existing data; the validator gates
        // new writes only.
        const canonical = canonicalizeStoredConversationTitle(row.title);
        stage.run(row.id, canonical.title, canonical.key);
      }
    },
    up: `
      -- The CHECK on \`title_key\` is deliberately WEAK — it is a spelling guard,
      -- not the fold. Only four things are decidable in SQLite: non-empty, no
      -- ASCII uppercase, and no edge space. It exists to catch a write path that
      -- forgot to fold or to trim (the classic being \`title\` copied straight
      -- into \`title_key\`), and it is asserted UNREACHABLE from the
      -- canonicalizer's own output in shared's tests — otherwise a legal title
      -- would become an insert failure. Everything the CHECK cannot see is the
      -- integrity scan's job.
      --
      -- There is deliberately NO "title changed => key must change" trigger:
      -- renaming \`General\` to \`general\` is a legal rename with an unchanged
      -- key, so such a trigger would refuse a rename the product allows.
      --
      -- Comments live OUTSIDE the statements in this migration on purpose:
      -- SQLite keeps a statement's text verbatim from CREATE onward in the schema
      -- catalog, so a rationale written inside the parentheses would be replayed
      -- into the normalized schema oracle.
      CREATE TABLE conversations_new (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
        inbox_agent_id          TEXT REFERENCES agents(id) ON DELETE CASCADE,
        placement_kind          TEXT NOT NULL
          CHECK (placement_kind IN ('workspace', 'direct_pair', 'agent_inbox')),
        roster_policy           TEXT NOT NULL
          CHECK (roster_policy IN ('open', 'fixed')),
        kind                    TEXT GENERATED ALWAYS AS
          (CASE WHEN roster_policy = 'open' THEN 'channel' ELSE 'dm_channel' END) VIRTUAL,
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
        CHECK (
          CASE placement_kind
            WHEN 'workspace'
              THEN workspace_id IS NOT NULL AND inbox_agent_id IS NULL
                   AND title_key IS NOT NULL
            WHEN 'agent_inbox'
              THEN inbox_agent_id IS NOT NULL AND workspace_id IS NULL
                   AND roster_policy = 'fixed'
                   AND title_key IS NULL
            ELSE 0
          END
        )
      );

      INSERT INTO conversations_new (
        id, workspace_id, inbox_agent_id, placement_kind, roster_policy,
        title, title_key, status, spawned_by_agent_id, closed_at, close_summary,
        pinned_at, created_at, updated_at, archived_at, latest_message_seq,
        visibility
      )
        SELECT
          c.id, c.workspace_id, c.inbox_agent_id, c.placement_kind,
          c.roster_policy,
          COALESCE(b.title, c.title), b.title_key,
          c.status, c.spawned_by_agent_id, c.closed_at, c.close_summary,
          c.pinned_at, c.created_at, c.updated_at, c.archived_at,
          c.latest_message_seq, c.visibility
        FROM conversations c
        LEFT JOIN temp.conversation_title_backfill b
          ON b.conversation_id = c.id;

      DROP TABLE temp.conversation_title_backfill;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, kind, updated_at DESC);
      CREATE INDEX idx_conversations_status
        ON conversations(status);
      CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;
      CREATE UNIQUE INDEX idx_conversations_inbox_agent
        ON conversations(inbox_agent_id) WHERE placement_kind = 'agent_inbox';

      -- THE address rule. Partial on placement, and NOT on \`archived_at\`: an
      -- archived channel still occupies its title (see the header).
      CREATE UNIQUE INDEX idx_conversations_title_key
        ON conversations(workspace_id, title_key)
        WHERE placement_kind = 'workspace';
    `,
  },
  {
    version: 73,
    name: 'direct_pair_endpoint_identity',
    // agent-scoped-session-v1 §4.2 / §8 P0-e (#task:2017) — the DirectPair
    // placement gains its two ENDPOINTS, and with them the one property the whole
    // placement rests on: a DM must survive the deletion of the principal on the
    // other side.
    //
    // An endpoint is a HISTORICAL PRINCIPAL COORDINATE, not a foreign key. With
    // an FK, deleting an agent would either ABORT (RESTRICT) or take the
    // conversation and its history with it (CASCADE), and both answers are wrong:
    // the survivor's DM is their record of a conversation that really happened.
    // Dropping the FK is also what removes birth validation, so
    // `trg_conversations_endpoints_birth` performs it explicitly — and more
    // precisely than an FK could, since which table an id must exist in depends
    // on the sibling `_kind` column (a conditional reference no FK can express).
    //
    // Existence is therefore derived by LOOKUP, and `endpoint_*_deleted_at`
    // records only WHEN a principal disappeared. That split is deliberate: birth
    // validation plus endpoint immutability mean a lookup miss ALREADY implies
    // "this principal existed and was deleted", so every read surface phrases the
    // question as `tombstone IS NOT NULL OR the lookup misses`. A delete path
    // that forgot to stamp the tombstone therefore cannot hand write access back
    // to the survivor — the same shape as v72's `conversation_gone` guard, where
    // forgetting to retire a name still cannot resolve a dangling id. The column
    // is a denormalized timestamp; correctness never rests on it.
    //
    // The stamp is written by BEFORE DELETE triggers on `agents` and `users`
    // rather than by the teardown use case, because that is the only form that
    // covers EVERY delete path, present and future — including FK cascades, which
    // do fire the deleted table's own BEFORE DELETE trigger (probed, and pinned
    // by a falsifier in the migration test). It also means the human side needs
    // no repo face and no product delete entry point: `users` has no deletion
    // path today (`user-repo.ts`: handles are never recycled), and this model
    // does not require inventing one for the endpoint identity to be complete.
    // These triggers TRANSFER a fact; they never RAISE — the alternative the
    // design doc already rejected (projected member rows guarded by refusing
    // triggers) turned `DELETE FROM agents` into an abort instead of a tombstone.
    // The `WHERE … IS NULL` filter keeps the transition monotonic, so the stamp is
    // legal under the monotonicity trigger and re-running it is a no-op. No
    // endpoint index accompanies it: this is the only endpoint-keyed statement in
    // P0-e and it runs on a rare hard delete, so the endpoint read faces (and the
    // indexes they need) stay with P3.
    //
    // A TABLE REBUILD is required rather than `ALTER TABLE ADD COLUMN`: the arc
    // CHECK's `ELSE 0` currently makes a `direct_pair` row UNCONSTRUCTIBLE, and
    // SQLite cannot alter a CHECK. Template = the v69 / v71 rebuilds; every
    // surviving column, constraint, and index is reproduced explicitly and the
    // falsifiers assert the full normalized schema rather than a sample, because a
    // rebuild's failure mode is silently losing one of them. `foreignKeyEscape`
    // disables `foreign_keys` around the migration and runs `foreign_key_check`
    // afterwards, so `DROP TABLE conversations` cannot cascade-delete children.
    //
    // There is NO data migration: no `direct_pair` row can exist below this
    // version, so every existing row keeps every column byte-for-byte.
    //
    // `title_key`, its spelling CHECK, its place in the arc CHECK and
    // `idx_conversations_title_key` are CARRIED FORWARD from v72 (#task:2016)
    // unchanged. A rebuild reproduces the whole table, so the address rule has to
    // be re-declared here or this migration would silently drop it; the
    // `direct_pair` branch keeps it NULL for the same reason `agent_inbox` does —
    // only a workspace placement has a title address.
    //
    // What is deliberately NOT here (P3): `pair_key` and its unique index,
    // get-or-create, the wire DTO, delivery and board surfaces. The CANONICAL
    // ORDER check IS here, though, on the same principle that made v69 declare
    // `CHECK (placement_kind = 'workspace')`: a declared domain that nothing
    // enforces is a lie. It also rules out a self-pair for free, and it lets P3
    // derive `pair_key` as a pure function of two already-ordered endpoints
    // instead of normalizing rows it inherited.
    foreignKeyEscape: true,
    up: `
      CREATE TABLE conversations_new (
        id                      TEXT PRIMARY KEY,
        workspace_id            TEXT REFERENCES workspaces(id) ON DELETE CASCADE,
        inbox_agent_id          TEXT REFERENCES agents(id) ON DELETE CASCADE,
        endpoint_a_kind         TEXT,
        endpoint_a_id           TEXT,
        endpoint_b_kind         TEXT,
        endpoint_b_id           TEXT,
        endpoint_a_deleted_at   INTEGER,
        endpoint_b_deleted_at   INTEGER,
        placement_kind          TEXT NOT NULL
          CHECK (placement_kind IN ('workspace', 'direct_pair', 'agent_inbox')),
        roster_policy           TEXT NOT NULL
          CHECK (roster_policy IN ('open', 'fixed')),
        kind                    TEXT GENERATED ALWAYS AS
          (CASE WHEN roster_policy = 'open' THEN 'channel' ELSE 'dm_channel' END) VIRTUAL,
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
              THEN workspace_id IS NOT NULL AND inbox_agent_id IS NULL
                   AND title_key IS NOT NULL
                   AND endpoint_a_kind IS NULL AND endpoint_a_id IS NULL
                   AND endpoint_b_kind IS NULL AND endpoint_b_id IS NULL
                   AND endpoint_a_deleted_at IS NULL
                   AND endpoint_b_deleted_at IS NULL
            WHEN 'agent_inbox'
              THEN inbox_agent_id IS NOT NULL AND workspace_id IS NULL
                   AND roster_policy = 'fixed'
                   AND title_key IS NULL
                   AND endpoint_a_kind IS NULL AND endpoint_a_id IS NULL
                   AND endpoint_b_kind IS NULL AND endpoint_b_id IS NULL
                   AND endpoint_a_deleted_at IS NULL
                   AND endpoint_b_deleted_at IS NULL
            WHEN 'direct_pair'
              THEN workspace_id IS NULL AND inbox_agent_id IS NULL
                   AND roster_policy = 'fixed'
                   AND title_key IS NULL
                   AND endpoint_a_kind IS NOT NULL AND endpoint_a_id IS NOT NULL
                   AND endpoint_b_kind IS NOT NULL AND endpoint_b_id IS NOT NULL
            ELSE 0
          END
        )
      );

      INSERT INTO conversations_new (
        id, workspace_id, inbox_agent_id, placement_kind, roster_policy,
        title, title_key, status, spawned_by_agent_id, closed_at, close_summary,
        pinned_at, created_at, updated_at, archived_at, latest_message_seq,
        visibility
      )
        SELECT
          c.id, c.workspace_id, c.inbox_agent_id, c.placement_kind, c.roster_policy,
          c.title, c.title_key, c.status, c.spawned_by_agent_id, c.closed_at,
          c.close_summary,
          c.pinned_at, c.created_at, c.updated_at, c.archived_at,
          c.latest_message_seq, c.visibility
        FROM conversations c;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, kind, updated_at DESC);
      CREATE INDEX idx_conversations_status
        ON conversations(status);
      CREATE INDEX idx_conversations_archived_at
        ON conversations(workspace_id) WHERE archived_at IS NOT NULL;
      CREATE UNIQUE INDEX idx_conversations_inbox_agent
        ON conversations(inbox_agent_id) WHERE placement_kind = 'agent_inbox';
      CREATE UNIQUE INDEX idx_conversations_title_key
        ON conversations(workspace_id, title_key)
        WHERE placement_kind = 'workspace';

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
    `,
  },
  {
    version: 74,
    name: 'direct_pair_pair_key',
    // agent-scoped-session-v1 §4.1 / §4.2 / §8 P3 (#task:2020) — the DirectPair
    // placement gains the two things that make it an AGGREGATE rather than a pair
    // of columns: a single scalar identity, and a DB-carried uniqueness that
    // survives two concurrent get-or-creates.
    //
    // `pair_key` IS A CONTRACT, not just a dedup device. P0-f keys the task-board
    // numbering space on `board_key = 'pair:' || pair_key` (§4.4), so this string
    // is read by a surface outside this table and its BYTES ARE LOCKED HERE. It is
    // derived from the endpoint COORDINATES only — never from a handle, a title or
    // anything else a human can rename — so renaming an agent or a user cannot
    // renumber their board.
    //
    // FORMAT: `<kind-initial><len>:<id>` per endpoint, `a` side then `b` side, e.g.
    // `a3:AG1h5:local`. It is PROVABLY INJECTIVE on the current (and any future)
    // endpoint domain, which is the whole reason for the shape:
    //   - the kind is one character out of a CLOSED enum whose two members differ
    //     in their first letter (`human` / `agent`, pinned by the v73 CHECK), so
    //     the leading byte decodes the kind with no delimiter;
    //   - each id is a NETSTRING. Decoding reads decimal digits up to the FIRST
    //     `:` — a length field never contains `:`, so that colon is unambiguously
    //     the delimiter — then takes exactly that many characters. An id may
    //     therefore itself contain `:` or leading digits without creating a second
    //     parse (`a4:a:5x` + `a5:b7:zz` round-trips), which is why this needs no
    //     id-charset CHECK to stay injective and cannot be broken by a later
    //     widening of the id alphabet.
    // Injectivity is not merely argued: `idx_conversations_pair_key` makes the DB
    // reject any two `direct_pair` rows that collide, so an edit to the expression
    // that lost injectivity would fail loudly instead of silently giving two DMs
    // one shared task board.
    //
    // GENERATED, not a column plus a trigger. §4.2 requires `UPDATE pair_key` to
    // ABORT; a generated column is UNWRITABLE BY CONSTRUCTION ("cannot UPDATE
    // generated column"), which discharges that rule with no load-bearing trigger
    // to forget, and it cannot drift from the endpoints it is derived from. It also
    // matches how `kind` is already derived on this table. VIRTUAL (not STORED)
    // because `ALTER TABLE ADD COLUMN` accepts only VIRTUAL — which is precisely
    // what lets P3 skip a table rebuild, so every existing row keeps every byte and
    // the v73 constraints and triggers survive untouched. The `CASE` guard makes
    // `pair_key IS NULL` ⟺ `placement_kind <> 'direct_pair'` a statement about
    // this expression alone, rather than one that has to borrow the arc CHECK.
    //
    // UNIQUENESS SITS ON BOTH KEYS, deliberately:
    //   - `idx_conversations_direct_pair_endpoints` is the IDENTITY constraint and
    //     the get-or-create lookup path. Looking a pair up by its four ordered
    //     coordinates means the byte format is never re-implemented in TypeScript:
    //     the DB stays the single authority for it.
    //   - `idx_conversations_pair_key` enforces injectivity (above) and is the
    //     lookup path for callers that hold only the scalar — P0-f's board key.
    // Both are PARTIAL on `placement_kind = 'direct_pair'`: workspace and inbox
    // rows have NULL endpoints, and while SQLite would treat those NULLs as
    // distinct, a partial index states the intent and keeps the entries out.
    //
    // There is NO data migration and no backfill: v73 left the placement
    // unconstructible until it landed, so no `direct_pair` row can exist yet.
    up: `
      ALTER TABLE conversations ADD COLUMN pair_key TEXT
        GENERATED ALWAYS AS (
          CASE
            WHEN placement_kind <> 'direct_pair' THEN NULL
            ELSE substr(endpoint_a_kind, 1, 1)
              || length(endpoint_a_id) || ':' || endpoint_a_id
              || substr(endpoint_b_kind, 1, 1)
              || length(endpoint_b_id) || ':' || endpoint_b_id
          END
        ) VIRTUAL;

      CREATE UNIQUE INDEX idx_conversations_direct_pair_endpoints
        ON conversations(
          endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id
        )
        WHERE placement_kind = 'direct_pair';

      CREATE UNIQUE INDEX idx_conversations_pair_key
        ON conversations(pair_key)
        WHERE placement_kind = 'direct_pair';
    `,
  },
  {
    version: 75,
    name: 'runtime_slots_placement_aware',
    // agent-scoped-session-v1 §4.2 / §8 P3 (#task:2020, @Cindy B3) — A RUNTIME
    // SEAT BELONGS TO PARTICIPATION, AND PARTICIPATION IS PLACEMENT-DERIVED.
    //
    // v70 landed the slot table while `conversation_agents` was still the only
    // participation edge in the schema, so it wrote that assumption into the
    // structure twice:
    //
    //   1. `FOREIGN KEY (conversation_id, agent_id) REFERENCES
    //      conversation_agents(...)` — a ConversationSlot could not EXIST without
    //      a roster row. §4.2 gives `direct_pair` and `agent_inbox` zero roster
    //      rows forever, so on those placements the slot was UNREPRESENTABLE, and
    //      the only way to make one was to insert the roster row the placement
    //      rule says must never exist (which is exactly the contradiction that
    //      failed review).
    //   2. `conversation_agents_create_runtime_slot` — the only creation path for
    //      a ConversationSlot was a roster INSERT, so even with the FK relaxed no
    //      DM or inbox slot would ever be born.
    //
    // Consequence while both stood: a DM session was invisible to every control
    // plane that reads slots (switch / stop / teardown / management), because the
    // row it would read cannot be created. This migration moves the seat's parent
    // to the pair it is actually a seat OF — `(agents, conversations)` — and gives
    // each placement its own creation edge, so "one seat per participation" holds
    // on all three arms instead of one.
    //
    // The roster keeps its arm and ONLY its arm: the workspace trigger now also
    // requires `placement_kind = 'workspace'`, so a stray roster row on a DM or an
    // inbox stays INERT here too — the same rule `agentParticipates` applies in
    // SQL and `resolveParticipation` applies in code.
    //
    // A DELETE trigger replaces what the composite FK used to do. Dropping the FK
    // would otherwise leave a seat behind when an agent LEAVES a channel, and a
    // leftover seat is not inert: stop / teardown would target a session in a
    // conversation the agent no longer belongs to. It deletes by the slot's own
    // coordinate, so an agent-scoped agent's single AgentSlot (NULL conversation)
    // is untouched by leaving any channel.
    //
    // `foreignKeyEscape` because this is a table REBUILD (SQLite cannot drop a
    // table-level FK in place) and the runner's `foreign_key_check` afterwards is
    // the acceptance evidence that the copy preserved every reference. Slot `id`s
    // are copied VERBATIM: `agent_runtime_events.slot_id` is a historical
    // coordinate with no FK, so renumbering would silently re-parent the
    // append-only history instead of failing.
    //
    // ## What replaces the FK as the INVARIANT (@Cindy B10)
    //
    // The composite FK did two jobs, and only one of them was wrong. Requiring a
    // ROSTER row was wrong; requiring PARTICIPATION was right, and it was the
    // FK — not the creation triggers — that made it a fact. Three AFTER INSERT
    // triggers say "a participation edge mints a seat"; they say nothing about a
    // seat that arrives by any other route, and with the FK gone two plain FKs
    // plus the partial UNIQUEs would happily accept
    // `(conversation-scoped agent, a conversation it does not participate in)`.
    // Such a row is not inert: it is a forged control coordinate that stop /
    // switch / teardown would act on. So the invariant is restated directly as
    // `agent_runtime_slots_participation_insert`, over the SAME placement
    // authority the three creation triggers use — and unknown placements fail
    // closed, because a future arm has no matching branch.
    //
    // ## Why the participation EDGES are frozen (@Cindy B11)
    //
    // A seat is derived from an edge, so an edge that can move invalidates the
    // derivation without touching either table's triggers: SQLite lets you
    // `UPDATE conversation_agents SET conversation_id = ...`, which is neither an
    // INSERT nor a DELETE, so the old seat stays where it was and the new
    // membership gets none. One level up, an `UPDATE conversations SET
    // placement_kind / workspace_id / inbox_agent_id / roster_policy` could
    // migrate a live row between arms while satisfying every CHECK, bypassing
    // v72's name allocation and both halves of this migration's seat lifecycle.
    // Nothing in production writes those columns after birth (placement is
    // decided by the constructor), so they are frozen here rather than given
    // transition rules — a real conversion would have to reallocate a name and
    // re-derive seats, which is a migration's job, not an UPDATE's.
    foreignKeyEscape: true,
    up: `
      CREATE TABLE agent_runtime_slots_new (
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

      INSERT INTO agent_runtime_slots_new (
        id, agent_id, conversation_id, generation,
        claimed_by_daemon_instance_id,
        observed_status, observed_pid, cli_session_id
      )
        SELECT id, agent_id, conversation_id, generation,
               claimed_by_daemon_instance_id,
               observed_status, observed_pid, cli_session_id
          FROM agent_runtime_slots;

      DROP TRIGGER conversation_agents_create_runtime_slot;
      DROP TRIGGER agents_create_agent_runtime_slot;
      DROP TABLE agent_runtime_slots;
      ALTER TABLE agent_runtime_slots_new RENAME TO agent_runtime_slots;

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

      -- The invariant the composite FK used to carry, restated over placement
      -- instead of over the roster: a ConversationSlot exists only where its
      -- agent PARTICIPATES. Every arm resolves its participants the way §4.2
      -- says it does, so this is the same authority the creation triggers below
      -- fire on — asserted here for EVERY writer, not just for them. A slot with
      -- no conversation is an AgentSlot: its participation is agent-level, and
      -- the scope trigger above already pins it to session_scope = 'agent'.
      -- An unknown placement matches no branch and is REFUSED: a future arm must
      -- name its participants here before it can hold a seat.
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
                          AND c.endpoint_b_id = NEW.agent_id)))
                OR (c.placement_kind = 'agent_inbox'
                    AND c.inbox_agent_id = NEW.agent_id))
         )
        BEGIN
          SELECT RAISE(
            ABORT,
            'runtime slot requires participation in its conversation'
          );
        END;

      -- A seat is DERIVED from an edge, so the edge may not move: an UPDATE is
      -- neither the INSERT that mints a seat nor the DELETE that releases it, so
      -- relocating a roster row would leave the seat on the old conversation and
      -- the new membership with none. Membership changes by leaving and joining.
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

      -- Placement is decided at birth by the constructor and read by every
      -- derivation downstream of it (participant set, name allocation, seat
      -- lifecycle). A coordinated UPDATE across these four columns satisfies the
      -- arm CHECKs while bypassing all three, so the columns are frozen: an arm
      -- conversion is a migration, not an UPDATE.
      CREATE TRIGGER conversations_placement_immutable
        BEFORE UPDATE OF
          placement_kind, workspace_id, inbox_agent_id, roster_policy
        ON conversations
        WHEN NOT (
          NEW.placement_kind IS OLD.placement_kind
          AND NEW.workspace_id IS OLD.workspace_id
          AND NEW.inbox_agent_id IS OLD.inbox_agent_id
          AND NEW.roster_policy IS OLD.roster_policy
        )
        BEGIN
          SELECT RAISE(
            ABORT,
            'conversations: placement is immutable from birth'
          );
        END;

      -- Workspace arm: the roster row IS the participation edge, and only there.
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

      -- Unchanged from v70, recreated verbatim: the AgentSlot is still born with
      -- the Agent row and still has no conversation. It is dropped and remade
      -- rather than left in place so that "which triggers may insert a slot" can
      -- be read off ONE migration instead of two.
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

      -- Losing the roster edge releases the seat it created. This is the
      -- composite FK's ON DELETE CASCADE, re-expressed where the roster is only
      -- one of three participation edges. Placement-gated like its creation
      -- counterpart: on a DM or an inbox the seat is owned by the endpoint /
      -- inbox edge, so a stray roster row's removal must not take it away.
      -- Deleting the CONVERSATION needs no help from this trigger — the slot's
      -- own conversation_id FK cascades — which is why a NULL placement lookup
      -- (row already gone) leaving this inert is correct rather than a hole.
      CREATE TRIGGER conversation_agents_delete_runtime_slot
        AFTER DELETE ON conversation_agents
        WHEN (SELECT placement_kind FROM conversations WHERE id = OLD.conversation_id)
             = 'workspace'
        BEGIN
          DELETE FROM agent_runtime_slots
            WHERE conversation_id = OLD.conversation_id
              AND agent_id = OLD.agent_id;
        END;

      -- DirectPair arm: the two endpoints ARE the participants (§4.2), and they
      -- are immutable columns of the row being inserted, so the conversation's
      -- birth is the participation edge. One statement covers both sides; an
      -- endpoint that is not an agent, or an agent-scoped agent (whose one
      -- AgentSlot already serves every conversation), selects zero rows.
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

      -- AgentInbox arm: exactly one participant, named by the row itself.
      CREATE TRIGGER conversations_create_agent_inbox_runtime_slot
        AFTER INSERT ON conversations
        WHEN NEW.placement_kind = 'agent_inbox'
        BEGIN
          INSERT INTO agent_runtime_slots (
            agent_id, conversation_id, generation,
            observed_status, observed_pid, cli_session_id
          )
            SELECT a.id, NEW.id, 0, 'idle', NULL, NULL
              FROM agents a
             WHERE a.id = NEW.inbox_agent_id
               AND a.session_scope = 'conversation';
        END;

      -- Backfill the two arms that could never have a seat before this
      -- migration. Guarded by NOT EXISTS rather than trusted to be empty: v71
      -- creates an inbox with every agent, so these rows are REAL on any dogfood
      -- database, and a re-run must not duplicate what it already made.
      INSERT INTO agent_runtime_slots (
        agent_id, conversation_id, generation,
        observed_status, observed_pid, cli_session_id
      )
        SELECT a.id, c.id, 0, 'idle', NULL, NULL
          FROM conversations c
          JOIN agents a
            ON a.session_scope = 'conversation'
           AND ((c.placement_kind = 'direct_pair'
                 AND ((c.endpoint_a_kind = 'agent' AND a.id = c.endpoint_a_id)
                   OR (c.endpoint_b_kind = 'agent' AND a.id = c.endpoint_b_id)))
             OR (c.placement_kind = 'agent_inbox' AND a.id = c.inbox_agent_id))
         WHERE NOT EXISTS (
           SELECT 1 FROM agent_runtime_slots s
            WHERE s.agent_id = a.id AND s.conversation_id = c.id
         );

      -- NO event is written for the seats born above (@Cindy B12). The earlier
      -- draft stamped every unstamped slot with slot_migrated under the banner
      -- "every slot has a birth event", and that contract cannot hold: the four
      -- creation triggers insert a slot and nothing else, so it would be true for
      -- the instant of the upgrade and false for the next roster join. Of the two
      -- ways out, writing a synthetic birth row per seat is the wrong one — every
      -- kind this log records (generation_claimed, projection_updated,
      -- generation_finalized) is a RUNTIME GENERATION, and a seat at generation 0
      -- that no daemon has claimed has no generation to report. So the log's
      -- domain stays "history from first claim", and slot_migrated keeps the
      -- narrower meaning v70 gave it: PROVENANCE for projection state that arrived
      -- without a claim (v70 moved live pid / status / cli_session_id off the
      -- roster row, so there was something to account for). The seats this
      -- migration mints carry no such state — generation 0, idle, NULL, NULL,
      -- identical to a trigger-born seat — so there is nothing to record, and
      -- inventing a row would make the log a worse account of the lifecycle
      -- rather than a complete one.
    `,
  },
  {
    version: 76,
    name: 'task_board_key',
    // agent-scoped-session-v1 §4.4 / §8 P0-f (#task:2022) — the task numbering
    // namespace becomes a first-class BOARD coordinate:
    //
    //   Board = WorkspaceBoard('ws:'||workspace_id)
    //         | DirectPairBoard('pair:'||pair_key)
    //
    // derived from the owning conversation's placement, never supplied as an
    // independent caller coordinate. `agent_inbox` has NO board: creating a
    // task there is refused by the DB, not silently filed somewhere.
    //
    // Table REBUILD, not ALTER: `tasks.workspace_id` was NOT NULL and SQLite
    // has no path to relax a column's nullability in place. The rebuild
    // follows the v69/v71 idiom (new table, row copy, DROP + RENAME, indexes
    // and triggers recreated afterwards) and its acceptance is the same
    // three-part, no-sampling check: full schema fingerprint, per-row data
    // equality by id, and an empty foreign_key_check.
    //
    // BOARD IDENTITY IS PROVEN, NOT TRUSTED (@Cindy, #task:2022 review):
    //  - the two arc CHECKs tie the key's prefix to the workspace column's
    //    null-ness in BOTH directions; together they also make any foreign
    //    prefix unsatisfiable (neither biconditional can hold);
    //  - the INSERT trigger requires NEW.board_key to EQUAL the value derived
    //    from the conversation's CURRENT placement — a well-formed but forged
    //    key (a `pair:` key on a workspace task, or another workspace's legal
    //    `ws:` key) is rejected, which NOT NULL + prefix shape + immutability
    //    alone cannot do. The trigger enumerates only the two registered Board
    //    arms, so `agent_inbox` and any future unregistered placement fail loud;
    //  - birth coordinates (conversation_id, workspace_id, board_key) are
    //    immutable: the derivation is checked at INSERT, so every coordinate
    //    it read must stay what it was (conversations.placement is already
    //    immutable from birth — v74's trigger).
    //
    // `board_key` backfill is the pure function 'ws:'||workspace_id, so the
    // legacy rows keep their numbers verbatim: (board_key, task_number)
    // resolves row-for-row identically to the old (workspace_id, task_number)
    // business key, and uniq_tasks_workspace_number is REPLACED by
    // uniq_tasks_board_number rather than kept alongside (two unique axes
    // over one namespace would be a second authority).
    foreignKeyEscape: true,
    // PROVE THE INNER-JOIN BACKFILL IS TOTAL before any DDL (review B1/B5).
    // v75 never constrained `tasks.workspace_id = conversations.workspace_id`,
    // and an FK-off half-state may also carry an owner conversation id that no
    // longer exists. The backfill below intentionally INNER JOINs the placement
    // authority; without this LEFT-JOIN precondition either shape would be
    // dangerous: a divergence freezes the wrong board, while an orphan simply
    // vanishes when the old table is dropped. Refuse both before the rebuild.
    precondition: (db) => {
      const handle = db as {
        prepare: (sql: string) => {
          all: () => { id: string; issue: 'missing_owner' | 'workspace_mismatch' }[];
          get: () => { n: number };
        };
      };
      const invalid = handle
        .prepare(
          `SELECT t.id,
                  CASE WHEN c.id IS NULL
                       THEN 'missing_owner'
                       ELSE 'workspace_mismatch'
                   END AS issue
             FROM tasks t LEFT JOIN conversations c ON c.id = t.conversation_id
            WHERE c.id IS NULL OR t.workspace_id IS NOT c.workspace_id
            LIMIT 5`,
        )
        .all();
      if (invalid.length > 0) {
        const count = handle
          .prepare(
            `SELECT COUNT(*) AS n
               FROM tasks t LEFT JOIN conversations c ON c.id = t.conversation_id
              WHERE c.id IS NULL OR t.workspace_id IS NOT c.workspace_id`,
          )
          .get().n;
        throw new Error(
          `v76 task_board_key: ${count} task row(s) cannot derive a board from their ` +
            `owner conversation (samples: ${invalid.map((row) => `${row.id}:${row.issue}`).join(', ')}). ` +
            'Resolve missing owners or workspace divergence before upgrading; refusing ' +
            'to drop an orphan or freeze a wrong board.',
        );
      }
    },
    up: `
      CREATE TABLE tasks_new (
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

      INSERT INTO tasks_new (
        id, workspace_id, task_number, conversation_id, thread_root_id,
        title, content, status, assignee_agent_id, assignee_user_id,
        creator_kind, creator_agent_id, creator_user_id,
        source_message_id, created_event_message_id,
        last_editor_kind, last_editor_id, last_edited_at,
        created_at, closed_at, board_key
      )
      SELECT
        t.id, t.workspace_id, t.task_number, t.conversation_id, t.thread_root_id,
        t.title, t.content, t.status, t.assignee_agent_id, t.assignee_user_id,
        t.creator_kind, t.creator_agent_id, t.creator_user_id,
        t.source_message_id, t.created_event_message_id,
        t.last_editor_kind, t.last_editor_id, t.last_edited_at,
        t.created_at, t.closed_at,
        -- Derived from the CONVERSATION (the board authority), not from the
        -- task's own column; the precondition above proved the two agree on
        -- every legacy row, so this is the same value — stated from the
        -- source that defines it.
        'ws:' || c.workspace_id
      FROM tasks t JOIN conversations c ON c.id = t.conversation_id;

      DROP TABLE tasks;
      ALTER TABLE tasks_new RENAME TO tasks;

      CREATE INDEX idx_tasks_conversation ON tasks(conversation_id);
      CREATE INDEX idx_tasks_status_number
        ON tasks(status, task_number DESC, id DESC);
      CREATE INDEX idx_tasks_workspace_status_number
        ON tasks(workspace_id, status, task_number DESC, id DESC);
      CREATE UNIQUE INDEX uniq_tasks_source_message
        ON tasks(source_message_id) WHERE source_message_id IS NOT NULL;
      CREATE UNIQUE INDEX uniq_tasks_board_number
        ON tasks(board_key, task_number);

      -- The board is DERIVED, never asserted — and the proof covers the FULL
      -- coordinate (review B1): board_key must equal what the conversation's
      -- placement yields RIGHT NOW, and the task's own workspace_id must be
      -- exactly the conversation's (workspace arm) or NULL (pair arm). A
      -- correct board_key with a foreign workspace_id would otherwise split
      -- the unique-numbering axis from every workspace-filtered read. No arm
      -- matches for agent_inbox / unknown placements / a missing conversation,
      -- which is the DB-level "the inbox has no board" rule.
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

      -- Birth coordinates are immutable: the derivation above is proven at
      -- INSERT, so everything it read must stay put. (conversations.placement
      -- is immutable since v74; this pins the task's side of the same fact.)
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
    `,
  },
  {
    version: 77,
    name: 'agent_runtime_control_audit',
    // #task:2025 c2 — a user clearing an AgentSlot is a control-plane fact,
    // not a runtime projection and not a conversation message. The historical
    // coordinates intentionally carry NO FK: deleting the Agent or actor must
    // neither erase nor block this append-only audit. There is deliberately no
    // conversation/focus column; the Agent aggregate owns the action and the
    // focus visible at dispatch time has no authority over it.
    up: `
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

      -- A daemon claim is immutable inside one generation. Rebinding a live
      -- generation to another daemon would make a generation-only terminal
      -- settlement ambiguous: only a generation increment may introduce a
      -- different non-null daemon, and finalization may only clear the claim.
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
    `,
  },
  {
    version: 78,
    name: 'workspace_display_name_address',
    // #task:2027 — a REGULAR workspace's display name is the root segment of
    // its readable delivery target. There is no independent slug and no retired
    // allocation: a rename releases the old folded key immediately. Agent-home
    // workspaces are deliberately outside this namespace and carry NULL here.
    //
    // As with v72, SQLite cannot compute the pinned Unicode fold. The migration
    // stages canonical display bytes and keys with the shared canonicalizer,
    // rebuilds the table so the kind/key arc is a DB fact, and then creates the
    // unique index over CURRENT regular rows. The precondition fails before DDL
    // on a folded collision, naming the rows instead of letting UNIQUE fail with
    // an opaque pair of column names.
    foreignKeyEscape: true,
    precondition: (db) => {
      const handle = db as {
        exec: (sql: string) => void;
        prepare: (sql: string) => {
          all: () => {
            id: string;
            workspace_kind: 'regular' | 'agent_home';
            owner_agent_id: string | null;
            name: string;
          }[];
          run: (...params: unknown[]) => void;
        };
      };
      const rows = handle
        .prepare(
          `SELECT id, workspace_kind, owner_agent_id, name
             FROM workspaces
            ORDER BY CASE workspace_kind WHEN 'regular' THEN 0 ELSE 1 END,
                     created_at, id`,
        )
        .all();
      const audit = auditWorkspaceNameAddresses(
        rows.map((row) => ({
          id: row.id,
          ownerAgentId: row.owner_agent_id,
          name: row.name,
          nameKey: null,
        })),
        { keysExpected: false },
      );
      if (audit.defects.length > 0) {
        throw new Error(
          `migration v78 (#task:2027): regular workspace display names fold to ` +
            `one current address. Rename one of each pair, then restart.\n` +
            describeWorkspaceNameAudit(audit),
        );
      }

      handle.exec(
        `DROP TABLE IF EXISTS temp.workspace_name_backfill;
         CREATE TEMP TABLE workspace_name_backfill (
           workspace_id TEXT PRIMARY KEY,
           name         TEXT NOT NULL,
           name_key     TEXT NOT NULL
         );`,
      );
      const stage = handle.prepare(
        `INSERT INTO temp.workspace_name_backfill
           (workspace_id, name, name_key)
         VALUES (?, ?, ?)`,
      );
      for (const row of rows) {
        if (row.workspace_kind !== 'regular') continue;
        const canonical = canonicalizeStoredWorkspaceName(row.name);
        stage.run(row.id, canonical.name, canonical.key);
      }
    },
    up: `
      CREATE TABLE workspaces_new (
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
        workspace_kind     TEXT NOT NULL
          CHECK (workspace_kind IN ('regular', 'agent_home')),
        owner_agent_id     TEXT UNIQUE REFERENCES agents(id) ON DELETE CASCADE,
        archived_at        INTEGER,
        created_at         INTEGER NOT NULL,
        updated_at         INTEGER NOT NULL,
        pinned_at          INTEGER,
        owner_user_id      TEXT REFERENCES users(id),
        CHECK (
          (workspace_kind = 'agent_home' AND owner_agent_id IS NOT NULL)
          OR
          (workspace_kind = 'regular' AND owner_agent_id IS NULL)
        ),
        CHECK (
          CASE workspace_kind
            WHEN 'regular' THEN name_key IS NOT NULL
            WHEN 'agent_home' THEN name_key IS NULL
            ELSE 0
          END
        )
      );

      INSERT INTO workspaces_new (
        id, name, name_key, description, workspace_kind, owner_agent_id,
        archived_at, created_at, updated_at, pinned_at, owner_user_id
      )
        SELECT
          w.id, COALESCE(b.name, w.name), b.name_key, w.description,
          w.workspace_kind, w.owner_agent_id, w.archived_at, w.created_at,
          w.updated_at, w.pinned_at, w.owner_user_id
        FROM workspaces w
        LEFT JOIN temp.workspace_name_backfill b ON b.workspace_id = w.id;

      DROP TABLE temp.workspace_name_backfill;
      DROP TABLE workspaces;
      ALTER TABLE workspaces_new RENAME TO workspaces;

      CREATE UNIQUE INDEX idx_workspaces_name_key
        ON workspaces(name_key) WHERE workspace_kind = 'regular';
      CREATE INDEX idx_workspaces_pinned_at
        ON workspaces(workspace_kind, pinned_at) WHERE pinned_at IS NOT NULL;
    `,
  },
  {
    version: 79,
    name: 'retire_conversation_kind_bridge',
    // #task:2023/#task:2032 — placement_kind × roster_policy are now the only
    // conversation taxonomy. `kind` was a v69–v78 GENERATED compatibility
    // bridge and has no remaining runtime or wire reader, so remove it rather
    // than preserving its two-value projection under another name.
    //
    // The only persisted value that embedded the old labels was the Favorites
    // ordering key. Rewrite exactly that namespace to `conversation:<id>`.
    // The precondition is deliberately before every DDL/DML statement and
    // rejects any two rows that would collapse onto one primary key; production
    // cannot create that state because roster_policy is immutable, but a loud
    // proof here is cheaper than silent rank loss in a forged/legacy database.
    precondition: (db) => {
      const handle = db as {
        prepare: (sql: string) => {
          all: () => Array<{
            user_id: string;
            canonical_key: string;
            source_keys: string;
          }>;
        };
      };
      const collisions = handle
        .prepare(
          `SELECT user_id, canonical_key,
                GROUP_CONCAT(entity_id, ',') AS source_keys
           FROM (
             SELECT user_id, entity_id,
                    CASE
                      WHEN SUBSTR(entity_id, 1, 8) = 'channel:'
                        THEN 'conversation:' || SUBSTR(entity_id, 9)
                      WHEN SUBSTR(entity_id, 1, 11) = 'dm_channel:'
                        THEN 'conversation:' || SUBSTR(entity_id, 12)
                      ELSE entity_id
                    END AS canonical_key
               FROM user_sort_entry
              WHERE list_key = 'favorites'
              ORDER BY user_id, entity_id
           )
          GROUP BY user_id, canonical_key
         HAVING COUNT(*) > 1
          ORDER BY user_id, canonical_key`,
        )
        .all();
      if (collisions.length > 0) {
        const sample = collisions
          .map((row) => `${row.user_id}:${row.canonical_key}<-[${row.source_keys}]`)
          .join('; ');
        throw new Error(
          `migration v79 (#task:2032): favorite keys would collapse during ` +
            `conversation-kind retirement (${collisions.length} collision(s)): ${sample}`,
        );
      }
    },
    up: `
      UPDATE user_sort_entry
         SET entity_id = CASE
           WHEN SUBSTR(entity_id, 1, 8) = 'channel:'
             THEN 'conversation:' || SUBSTR(entity_id, 9)
           ELSE 'conversation:' || SUBSTR(entity_id, 12)
         END
       WHERE list_key = 'favorites'
         AND (
           SUBSTR(entity_id, 1, 8) = 'channel:'
           OR SUBSTR(entity_id, 1, 11) = 'dm_channel:'
         );

      DROP TRIGGER channel_attention_prefs_channel_only_insert;
      DROP TRIGGER channel_attention_prefs_channel_only_update;
      DROP INDEX idx_conversations_workspace;

      ALTER TABLE conversations DROP COLUMN kind;

      -- The active workspace/member reads constrain workspace_id and order by
      -- updated_at. Keep that access path without the retired classifier in the
      -- middle; the v79 plan falsifier EXPLAINs the exported production SQL.
      CREATE INDEX idx_conversations_workspace
        ON conversations(workspace_id, updated_at DESC);

      -- Preserve the old fail-closed behavior: a missing conversation and every
      -- fixed-roster placement both map to the empty sentinel and are rejected.
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
    `,
  },
  {
    version: 80,
    name: 'direct_pair_endpoint_b_index',
    // #task:2035 — SEEK A PAIR FROM EITHER END.
    //
    // v74 gave `direct_pair` two access paths, and both are keyed from slot A:
    // the endpoints index leads with `endpoint_a_kind, endpoint_a_id`, and
    // `pair_key` folds the canonical order into one string. That is enough to
    // answer "does this pair exist" (the writer always knows both ends), but not
    // "which pairs does this principal have" — and the second question is the one
    // a viewer's Direct Messages list asks on every load.
    //
    // The asymmetry is structural, not incidental: `canonicalEndpointOrder`
    // sorts by kind before id and 'agent' < 'human', so in EVERY agent<->human
    // pair the agent takes slot A and the person takes slot B. The human arm of
    // the viewer read is therefore exactly the arm with no seekable prefix, and
    // without this index it degrades to a scan of `conversations`.
    //
    // Deliberately narrow. `placement_kind` stays out of the key because the
    // partial predicate already fixes it, and `updated_at` stays out because a
    // wider index buys write amplification for an ordering a bounded per-viewer
    // pair set can do in the sort. Canonical order and `pair_key` semantics are
    // untouched: making the read symmetric is an index concern, and reshaping the
    // canonical order would turn it into an identity/uniqueness migration.
    //
    // No data migration: an index over existing rows, no column and no backfill.
    up: `
      CREATE INDEX idx_conversations_direct_pair_endpoint_b
        ON conversations(endpoint_b_kind, endpoint_b_id)
        WHERE placement_kind = 'direct_pair';
    `,
  },
  {
    version: 81,
    name: 'saved_messages_drop_workspace_id',
    // #task:2050 (M2 delta 2) — DROP the denormalized `saved_messages.workspace_id`.
    //
    // The column was a NOT NULL copy of `conversations.workspace_id`, kept only
    // for the `?workspace_id=` list filter. It forced Saved to be a
    // workspace-only artifact (a `direct_pair` conversation has NULL
    // `workspace_id`, so a DM save was not even expressible) and — worse — it was
    // an UNCONSTRAINABLE duplicate: SQLite has no cross-table CHECK, so nothing
    // could prove `sm.workspace_id` stayed in step with `conv.workspace_id`, and
    // "NULL means direct_pair" would have been a second, un-falsifiable encoding
    // of placement. The canonical routing coordinate `conversation_id` is already
    // stored NOT NULL; the read model derives `workspace_id` from
    // `conversations.workspace_id` via a join it already performs, and the filter
    // becomes `conv.workspace_id = ?`. (Contrast `tasks`, which safely keeps a
    // nullable `workspace_id` because its `board_key` CHECK makes the
    // contradiction unconstructable — Saved has no equivalent guard.)
    //
    // SQLite cannot DROP a column that carries an index in place, so this is a
    // full table rebuild → foreignKeyEscape so the runner toggles foreign_keys
    // OFF + legacy_alter_table ON and runs foreign_key_check afterward. Dropping
    // the old table drops ALL its indexes; every index EXCEPT
    // `idx_saved_messages_workspace` is recreated verbatim. That index is not
    // recreated: it was already dead for reads (`listForUser` always filters
    // `user_id` and is driven by `idx_saved_messages_user_saved_at`), and its only
    // remaining job — the workspace-delete FK cascade probe — is served by
    // `idx_saved_messages_conversation`, which the cascade now traverses
    // (`workspaces → conversations → saved_messages.conversation_id`).
    foreignKeyEscape: true,
    // Losslessness proof (#task:2050 falsifier 4): the drop discards nothing only
    // if every existing row's `workspace_id` equals its conversation's — i.e. the
    // duplicate never drifted and every saved row is workspace-placed (DM saves
    // could not exist under the old constraint). Fail LOUDLY inside the migration
    // transaction if that invariant is violated, rather than silently discard a
    // column that had diverged.
    precondition: (db) => {
      const handle = db as {
        prepare: (sql: string) => { get: () => { n: number } };
      };
      const drift = handle
        .prepare(
          `SELECT COUNT(*) AS n
             FROM saved_messages sm
             JOIN conversations c ON c.id = sm.conversation_id
            WHERE sm.workspace_id IS NOT c.workspace_id`,
        )
        .get();
      if (drift.n > 0) {
        throw new Error(
          `migration v81: ${drift.n} saved_messages row(s) have a workspace_id ` +
            `that disagrees with their conversation's workspace_id; the column ` +
            `is not a safe-to-drop duplicate. Investigate before dropping.`,
        );
      }
    },
    up: `
      CREATE TABLE saved_messages_new (
        id               TEXT PRIMARY KEY,
        message_id       TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
        conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        saved_at         INTEGER NOT NULL,
        user_id          TEXT REFERENCES users(id) ON DELETE CASCADE
      );

      INSERT INTO saved_messages_new (id, message_id, conversation_id, saved_at, user_id)
        SELECT id, message_id, conversation_id, saved_at, user_id FROM saved_messages;

      DROP TABLE saved_messages;
      ALTER TABLE saved_messages_new RENAME TO saved_messages;

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
    `,
  },
  {
    version: 82,
    name: 'agent_last_activity_at',
    // #task:2049 — durable per-agent presence clock feeding the `/presence`
    // read face's `away_at` deadline.
    //
    // NULL is the never-active state; it is intentionally NOT backfilled. There
    // is no honest activity timestamp to fabricate for an agent that has never
    // been active, and seeding one would invent a false deadline. NULL projects
    // to Away (the renderer requires a positive live deadline for Online), so
    // with no backfill every agent correctly reads Away until its first real
    // activity — the alternative (a fabricated deadline) would falsely render
    // every agent Online on deploy day. The
    // column is written ONLY by the backend at admitted runtime-activity edges
    // (parsed event / delivery consumed) with the BACKEND clock, and only ever
    // moved forward (monotonic max). The host-authored frame `ts` is NEVER
    // written here (the host clock is untrusted for a durable, monotonic value).
    up: `
      ALTER TABLE agents ADD COLUMN last_activity_at INTEGER;
    `,
  },
  {
    version: 83,
    name: 'workspaces_drop_workspace_kind',
    // #task:2080 — drop the redundant `workspaces.workspace_kind` column (and the
    // vestigial `pinned_at`), retiring the last storage-layer spelling of a fact
    // `owner_agent_id` already carries. Two CHECKs pressed kind and ownership into
    // a single bit (`owner_agent_id IS NOT NULL` ⇔ `workspace_kind = 'agent_home'`),
    // so every read of the word is a rewrite to the relation with no behavior to
    // change — #task:2075 stopped publishing it, #task:2082 removed the last
    // decision that read it, #task:2081 retired the last wire that carried it.
    //
    // SQLite cannot DROP a column referenced by a CHECK/index/trigger, so this is a
    // full table rebuild. All eight dependents move in ONE statement list: a split
    // would leave the four cross-table triggers (they live on workspace_members /
    // workspace_attention_preferences, NOT on workspaces, so a workspaces rebuild
    // does not touch them) pointing at a column that no longer exists, and because
    // triggers compile lazily the migration would pass while the NEXT write to
    // either table failed with `no such column: workspace_kind`. The mapping:
    //   CHECK (kind ⇔ owner)              → dropped (it WAS the redundancy proof)
    //   CHECK (kind ⇒ name_key nullity)   → the single-line F2 form below, which
    //        keeps the SAME owner_agent_id ↔ name_key pairing #2082 relies on:
    //        old #1 ∧ #2 together imply `(owner_agent_id IS NULL) = (name_key IS
    //        NOT NULL)` for every existing row, so no precondition / data transform.
    //   idx_workspaces_name_key           → WHERE owner_agent_id IS NULL (row set
    //        identical to WHERE kind='regular' by the dropped #1)
    //   idx_workspaces_pinned_at          → dropped, not re-axised: `pinned_at` has
    //        no production reader left (F1), so the column and its index both go.
    //   4 cross-table triggers            → DROP + CREATE asking owner_agent_id,
    //        preserving each trigger's missing-workspace behavior verbatim.
    foreignKeyEscape: true,
    up: `
      DROP TRIGGER workspace_members_no_agent_home_insert;
      DROP TRIGGER workspace_members_no_agent_home_update;
      DROP TRIGGER workspace_attention_prefs_regular_only_insert;
      DROP TRIGGER workspace_attention_prefs_regular_only_update;

      CREATE TABLE workspaces_new (
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

      INSERT INTO workspaces_new (
        id, name, name_key, description, owner_agent_id,
        archived_at, created_at, updated_at, owner_user_id
      )
        SELECT
          id, name, name_key, description, owner_agent_id,
          archived_at, created_at, updated_at, owner_user_id
        FROM workspaces;

      DROP TABLE workspaces;
      ALTER TABLE workspaces_new RENAME TO workspaces;

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
    `,
  },
  {
    version: 84,
    name: 'retire_agent_inbox_rows',
    // #task:2072 c4 — REMOVE THE POPULATION; v85 removes the shape.
    //
    // `agent_inbox` was the single-participant container a v71 backfill minted one
    // of per agent, and which the agent-creation aggregate kept minting until c1
    // shrank the reserve to a single `agents` INSERT. Nothing writes one any more
    // (c1), the last read that needed one moved to the creator's direct line (c2),
    // and the concept itself is retired: waking an agent must happen in a
    // conversation that has a second participant.
    //
    // This version deliberately does NOT set `foreignKeyEscape`. Its correctness IS
    // the cascade: `foreignKeyEscape` disables foreign keys for the WHOLE migration,
    // so a row deletion placed in v85's rebuild would silently skip every dependent
    // cascade and leave orphans — and the escape's closing `foreign_key_check`
    // would not catch them, because at that moment the surviving declaration is
    // still the one that permits the parent. Hence the split, in this order: delete
    // rows with foreign keys ON, then rebuild.
    //
    // "with foreign keys ON" is a fact about the runner, not a hope: `openDatabase`
    // sets `foreign_keys = ON` before `runMigrations`, and the runner only turns
    // them off around a `foreignKeyEscape` migration (backend `db.ts`). A
    // non-escape version therefore runs with the cascades armed.
    //
    // Dependents are left to the cascade on purpose. Every table that REFERENCES
    // conversations(id) declares CASCADE or SET NULL, so hand-rolling a dependent
    // list here would be a carrier-keyed enumeration whose only failure mode is a
    // silent hole. The ONE exception is `notifications`, and only because SET NULL
    // is the wrong outcome there rather than an incomplete one: a notification
    // whose `conversation_id` AND `message_id` both go NULL still lists in the tray
    // and navigates nowhere. Its `source_conversation_id` (provenance) is a
    // different case — NULLing it degrades where the mention came from while the
    // notification's own target stays reachable, so those rows stay. The line is
    // "a null TARGET kills the row, a null PROVENANCE does not".
    //
    // `agent_runtime_slots` seats cascade (the inbox seat was minted by the v71-era
    // birth trigger v85 removes). The append-only runtime history is untouched and
    // cannot block this: `agent_runtime_events` / `agent_runtime_control_events`
    // guard DELETE with RAISE(ABORT) but carry NO foreign key by deliberate design
    // (see their column comments), so the cascade never reaches them — at the
    // documented cost that their historical `conversation_id` / `slot_id` stop
    // resolving.
    up: `
      DELETE FROM notifications
       WHERE conversation_id IN (
               SELECT id FROM conversations WHERE placement_kind = 'agent_inbox'
             );

      DELETE FROM conversations WHERE placement_kind = 'agent_inbox';
    `,
  },
  {
    version: 85,
    name: 'conversations_drop_inbox_agent',
    // #task:2072 c4 — retire the SHAPE, once v84 removed the population.
    //
    // Three things go, and they must go together: the `inbox_agent_id` column, the
    // `'agent_inbox'` member of the placement alphabet, and the arc CHECK's
    // `WHEN 'agent_inbox'` branch. Keeping the alphabet member after the rows are
    // gone would leave a value that is legal, unreachable and unenforced by
    // anything — a phantom fence that reads like a supported placement.
    //
    // SQLite cannot DROP a column named by a CHECK, an index or a trigger, and it
    // cannot alter a CHECK at all, so this is a full table rebuild. Three rebuild
    // hazards, each silent if missed:
    //   1. `pair_key` is an INLINE VIRTUAL generated column and the sole carrier of
    //      the direct_pair uniqueness index — a rebuild that forgets to re-declare
    //      it drops the column and the constraint without an error.
    //   2. `conversations` owns SEVEN triggers; DROP TABLE takes all of them. Six
    //      are re-created below (`conversations_placement_immutable` minus
    //      `inbox_agent_id` from both its UPDATE OF list and its WHEN), and
    //      `conversations_create_agent_inbox_runtime_slot` is NOT — it was the birth
    //      trigger of the retired placement. `test_seed_conversation_users` is a
    //      TEST fixture trigger (backend `tests/utils/db.ts`), not part of the
    //      production schema, and must not appear here.
    //   3. `agent_runtime_slots_participation_insert` lives on
    //      `agent_runtime_slots`, so the conversations rebuild does NOT touch it,
    //      and its participation arc still names `c.inbox_agent_id`. Triggers
    //      compile lazily: left alone, this migration would pass and the NEXT
    //      runtime-slot insert would fail with `no such column: c.inbox_agent_id`.
    //      DROP + CREATE it without the inbox arm.
    //
    // The `ELSE 0` arm of the arc CHECK stays: it is what makes any future
    // unregistered placement fail loud instead of skipping the arc.
    foreignKeyEscape: true,
    up: `
      DROP TRIGGER agent_runtime_slots_participation_insert;

      CREATE TABLE conversations_new (
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

      INSERT INTO conversations_new (
        id, workspace_id,
        endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id,
        endpoint_a_deleted_at, endpoint_b_deleted_at,
        placement_kind, roster_policy, title, title_key, status,
        spawned_by_agent_id, closed_at, close_summary, pinned_at,
        created_at, updated_at, archived_at, latest_message_seq, visibility
      )
        SELECT
          id, workspace_id,
          endpoint_a_kind, endpoint_a_id, endpoint_b_kind, endpoint_b_id,
          endpoint_a_deleted_at, endpoint_b_deleted_at,
          placement_kind, roster_policy, title, title_key, status,
          spawned_by_agent_id, closed_at, close_summary, pinned_at,
          created_at, updated_at, archived_at, latest_message_seq, visibility
        FROM conversations;

      DROP TABLE conversations;
      ALTER TABLE conversations_new RENAME TO conversations;

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
    `,
  },
  {
    version: 86,
    name: 'user_list_sort_mode_rail_scope_agent_space',
    // #task:2045 Delta 4 — the LAST place where `dm` still spells "Agent Space".
    //
    // The workspace rail's sort mode is keyed by rail scope, and the Agent Spaces
    // section's scope has been the string `dm` since v48 (see the fossil comment
    // there — it predates the Agent Space / DirectPair split, when the rail's only
    // agent-ish section WAS the DM list). After #task:2066 the renderer and after
    // this migration the wire both spell it `agent_space`, so the stored value has
    // to move with them, atomically in one tree.
    //
    // Why a data migration is REQUIRED rather than nice-to-have — the two sort
    // tables are keyed differently, so a rename without this UPDATE fails
    // asymmetrically:
    //   - `user_sort_entry` (manual rank) is keyed by (user, list_key, ENTITY_ID)
    //     and carries no scope ⇒ every hand-dragged rank survives untouched.
    //   - `user_list_sort_mode` (which sort mode that section is IN) is keyed by
    //     (user, list_key, SCOPE) ⇒ the row keyed `dm` is orphaned the moment the
    //     reader asks for `agent_space`, and the section silently falls back to
    //     the default mode.
    // The user-visible result of skipping this would be "my manual order is still
    // there but is no longer being used" — a strictly harder bug to report and to
    // diagnose than "my order was lost".
    //
    // No shape change: `scope` has no CHECK (v48 above), so this is a plain value
    // rewrite, not a table rebuild. It is idempotent (`WHERE scope = 'dm'` matches
    // nothing on a second pass) and it is fail-loud on collision: if a user somehow
    // already holds an `agent_space` row for the same list, the UPDATE hits the
    // PRIMARY KEY and SQLite's DEFAULT conflict action (ABORT) throws, which db.ts
    // rolls back together with the `_migrations` row — no half-renamed population.
    // Written as a bare UPDATE on purpose: spelling `OR ABORT` would only restate
    // the default while spending the db-boundary conflict-action budget, so the
    // hazard fence stays armed for a real `OR REPLACE`/`OR IGNORE`, which is the
    // change that WOULD matter. Silently merging the two rows is the wrong default
    // anyway: which of two conflicting modes wins is a user-data decision.
    //
    // Scope note: the rail's collapse state is NOT touched. That key lives in
    // localStorage as `SECTION_KEY_PREFIX + persistId` with `persistId="dm"`
    // (#task:2035, documented at the `Sidebar.tsx` call site); renaming it would
    // silently reset every user's collapse choice, and there is no collision to
    // avoid — real DMs use `persistId="direct-messages"`.
    up: `
      UPDATE user_list_sort_mode
         SET scope = 'agent_space'
       WHERE list_key = 'workspaces'
         AND scope = 'dm';
    `,
  },
  {
    version: 87,
    name: 'messages_drop_v60_task_residue',
    // #task:2083 — physically remove the "message IS task" carrier that v60
    // retired. v60 created the `tasks` aggregate and backfilled every
    // `messages.task_number IS NOT NULL` row into it (see the INSERT above),
    // reusing the anchor message's id as `tasks.id`; since then nothing writes
    // these 12 columns and nothing reads them. They have been dead weight on
    // every message row, and — worse — a second spelling of a fact the `tasks`
    // table owns, which is exactly the shape a future reader accidentally
    // resurrects.
    //
    // This is IRREVERSIBLE physical deletion: after this migration the old flat
    // task fields are gone from the file, and a downgrade cannot recover them.
    // The safety argument is therefore not "we believe nothing needs them" but
    // the precondition below, which proves per carrier that the canonical row
    // exists before a single byte is dropped.
    //
    // Order matters: SQLite refuses to DROP a column that any index still names,
    // including one named only in a partial index's WHERE clause, so the four
    // indexes go first. Written without `IF EXISTS` on purpose — if one of them
    // is already absent, some other change moved this ground and the assumption
    // this migration rests on is void, so it must fail loudly rather than
    // continue. No table rebuild is needed (verified against the in-tree SQLite):
    // plain DROP COLUMN handles a column carrying its own `REFERENCES` clause and
    // the column-level CHECK on `task_last_editor_kind`, and leaves
    // `foreign_key_check` clean, so this runs with `foreign_keys = ON` like every
    // other non-rebuild migration.
    //
    // Deliberately NOT touched:
    //   - `cc_source_message_id` — a different, LIVE column (mention-cc dedupe)
    //     whose name merely ends the same way; `idx_messages_cc_dedupe` stays.
    //   - `messages.metadata` task system-event payloads (`$.task_id` etc.) —
    //     that is the task event stream, not the retired carrier.
    //   - the v60 backfill SQL above, which reads these columns as HISTORY.
    //     Applied migrations are never rewritten; that statement only ever runs
    //     against a v59 file, where the columns still exist.
    precondition: (db) => {
      const handle = db as {
        prepare: (sql: string) => { all: () => { id: string }[] };
      };
      // Every historical carrier must already have its canonical task. Identity,
      // not field equality: a migrated task's content is free to have changed
      // since v60, but its id cannot, because v60 reused the anchor id.
      //
      // `task_number IS NOT NULL` is the whole carrier set, and the check is
      // deliberately NOT widened to "any task_* column is non-NULL". Those
      // columns were only ever populated on task rows (v54 above states it for
      // the later five: "NULL for non-task rows"), and v60's own backfill keyed
      // on exactly this predicate — so a stray value on a task_number IS NULL
      // row is something v60 already treated as absent and no reader has looked
      // at since. Aborting an upgrade over those bytes would block a real
      // migration to protect data no code path can consume.
      const orphans = handle
        .prepare(
          `SELECT m.id AS id
             FROM messages m
            WHERE m.task_number IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.id = m.id)
            ORDER BY m.id
            LIMIT 20`,
        )
        .all();
      if (orphans.length > 0) {
        throw new Error(
          `migration v87: ${orphans.length} message row(s) still carry retired task_* ` +
            'data with no canonical tasks row of the same id, so dropping the columns ' +
            'would destroy the only copy. Ids (up to 20): ' +
            `${orphans.map((row) => row.id).join(', ')}. ` +
            'Re-run the v60 backfill for these rows (tasks.id = the anchor message id) ' +
            'before upgrading.',
        );
      }
    },
    up: `
      DROP INDEX uniq_messages_task_workspace_number;
      DROP INDEX idx_messages_task_workspace;
      DROP INDEX idx_messages_task_workspace_closed_at;
      DROP INDEX idx_messages_source_message_id;

      ALTER TABLE messages DROP COLUMN task_number;
      ALTER TABLE messages DROP COLUMN task_status;
      ALTER TABLE messages DROP COLUMN task_assignee_agent_id;
      ALTER TABLE messages DROP COLUMN task_assignee_user_id;
      ALTER TABLE messages DROP COLUMN task_title;
      ALTER TABLE messages DROP COLUMN task_workspace_id;
      ALTER TABLE messages DROP COLUMN task_closed_at;
      ALTER TABLE messages DROP COLUMN source_message_id;
      ALTER TABLE messages DROP COLUMN task_revision;
      ALTER TABLE messages DROP COLUMN task_last_editor_kind;
      ALTER TABLE messages DROP COLUMN task_last_editor_id;
      ALTER TABLE messages DROP COLUMN task_last_edited_at;
    `,
  },
  {
    version: 88,
    name: 'workspaces_drop_retired_rail_sort_scopes',
    // #task:2190 — migrate the workspace rail's sort-mode rows to the current
    // two-group scope vocabulary by physically deleting the two retired kinds.
    //
    // History: the workspace rail once stored a single flat mode under the
    // list-wide sentinel scope `''` (and, at one point, a `'pinned'` group).
    // v48 folded EVERY `user_list_sort_mode` row to `scope=''` when it introduced
    // the scope column, and its comment even called the resulting
    // `workspaces`/`''` row a harmless orphan. v86 then split the rail into two
    // hard groups — `'regular'` and `'agent_space'` — and only remapped the
    // `'dm'` legacy value, deliberately leaving the older `''`/`'pinned'` rows in
    // place. Real databases created before v48 therefore still carry
    // `workspaces` rows at `scope IN ('', 'pinned')`.
    //
    // Those two kinds are no longer legal on this rail: the current model has
    // exactly two groups and no flat/pinned scope. The Rust cold-read reader
    // (and the product's own two-group semantics) treat any `workspaces` scope
    // outside `{regular, agent_space}` as corruption and fail closed — which
    // would safe-500 a legitimately-upgraded user whose only "fault" is a
    // pre-v48 provenance. A retired feature migrates to its terminal state; it
    // is not preserved 1:1. The terminal state for these rows is "gone": a mode
    // row is a pure viewer preference with no downstream fact, so dropping it
    // simply returns that group to its default, exactly as a user who never set
    // a mode.
    //
    // Boundary, deliberately narrow:
    //   - ONLY `list_key = 'workspaces'` rows at `scope IN ('', 'pinned')` go.
    //   - Other lists' legitimate `scope = ''` (channel rails `channels:*`,
    //     favorites) are the flat-list sentinel and MUST survive verbatim.
    //   - Any OTHER unknown `workspaces` scope is NOT swallowed here: it is left
    //     for the strict reader to fail closed on, because this migration only
    //     retires the two kinds we can prove are historical, not "anything the
    //     reader dislikes".
    //
    // Pure data DELETE: no DDL, so the schema snapshot is byte-identical and this
    // runs with `foreign_keys = ON` like every other non-rebuild migration.
    up: `
      DELETE FROM user_list_sort_mode
       WHERE list_key = 'workspaces'
         AND scope IN ('', 'pinned');
    `,
  },
  {
    version: 89,
    name: 'workspaces_drop_retired_starred_sort_scope',
    // The pre-#1888 rail accepted `starred` as a workspace hard group
    // (297b5b407, sort.ts legalGroups/modeScopeOf). Step 88 retired the flat
    // and pinned modes but missed this historical preference. Append a step:
    // databases already at 88 must receive the repair too. This removes only
    // the retired group mode, never entity ranks/stars, other lists, or content.
    up: `
      DELETE FROM user_list_sort_mode
       WHERE list_key = 'workspaces' AND scope = 'starred';
    `,
  },
  {
    version: 90,
    name: 'agent_activity_stream_coordinates',
    // Agent scope reads the existing single-copy rows by agent, never by focus.
    // No historical conversation coordinate or content is rewritten.
    up: `
      ALTER TABLE conversation_events ADD COLUMN runtime_generation INTEGER CHECK(runtime_generation IS NULL OR runtime_generation > 0);
      CREATE INDEX idx_conversation_events_stream ON conversation_events(agent_id, id);
    `,
  },
  {
    version: 91,
    name: 'activity_retention_identity_and_created_at',
    // Retention can empty the table or remove its largest id. Plain rowid would
    // then recycle an HTTP/WS upsert identity. SQLite owns the monotone sequence;
    // no application watermark or retention coordinator is needed.
    // No table references conversation_events, so this child-table rebuild runs
    // with foreign_keys ON; preserve every original row and all four indexes.
    up: `
      CREATE TABLE conversation_events_retained (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL,
        kind TEXT NOT NULL,
        payload TEXT,
        created_at INTEGER NOT NULL,
        runtime_generation INTEGER CHECK(runtime_generation IS NULL OR runtime_generation > 0)
      );
      INSERT INTO conversation_events_retained
        (id, conversation_id, agent_id, kind, payload, created_at, runtime_generation)
        SELECT id, conversation_id, agent_id, kind, payload, created_at, runtime_generation
        FROM conversation_events;
      DROP TABLE conversation_events;
      ALTER TABLE conversation_events_retained RENAME TO conversation_events;
      CREATE INDEX idx_conversation_events ON conversation_events(conversation_id, id);
      CREATE INDEX idx_conversation_events_agent ON conversation_events(conversation_id, agent_id, id);
      CREATE INDEX idx_conversation_events_stream ON conversation_events(agent_id, id);
      CREATE INDEX idx_conversation_events_created_at ON conversation_events(created_at);
    `,
  },
  {
    version: 92,
    name: 'human_message_retry_identity',
    up: `
      ALTER TABLE messages ADD COLUMN client_message_id TEXT;
      ALTER TABLE messages ADD COLUMN client_request_json TEXT;
      CREATE UNIQUE INDEX idx_messages_client_identity ON messages(from_user_id, client_message_id)
        WHERE client_message_id IS NOT NULL;
    `,
  },
  {
    version: 93,
    name: 'pi_runtime_transition_target',
    // Replace the closed column CHECK without rebuilding the referenced parent
    // agents table. Preserve in-flight locks and enforce both fields on updates.
    up: `
      DROP TRIGGER agents_runtime_transition_pair_insert;
      DROP TRIGGER agents_runtime_transition_pair_update;
      ALTER TABLE agents RENAME COLUMN runtime_transition_target TO runtime_transition_target_previous;
      ALTER TABLE agents ADD COLUMN runtime_transition_target TEXT;
      UPDATE agents SET runtime_transition_target = runtime_transition_target_previous;
      ALTER TABLE agents DROP COLUMN runtime_transition_target_previous;
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
    `,
  },
  {
    version: 94,
    name: 'profile_message_count_indexes',
    up: `
      CREATE INDEX idx_messages_profile_user ON messages(from_user_id, created_at)
        WHERE from_kind='user' AND cc_source_message_id IS NULL;
      CREATE INDEX idx_messages_profile_agent ON messages(from_agent_id, created_at)
        WHERE from_kind='agent' AND cc_source_message_id IS NULL;
    `,
  },
  {
    version: 95,
    name: 'operator_console_aggregate_indexes',
    up: `
      CREATE INDEX idx_auth_sessions_last_used_at
        ON auth_sessions(last_used_at, user_id);
      CREATE INDEX idx_messages_product_activity_created_at
        ON messages(created_at)
        WHERE from_kind IN ('user','agent') AND cc_source_message_id IS NULL;
    `,
  },
  {
    version: 96,
    name: 'canonical_task_titles',
    // v60 only trimmed ASCII edge spaces. Freeze the v96 title contract,
    // including ECMAScript whitespace and the UTF-16 length bound.
    precondition: (db) => {
      const handle = db as {
        exec: (sql: string) => void;
        prepare: (sql: string) => {
          all: () => { id: string; title: string }[];
          run: (...params: unknown[]) => void;
        };
      };
      handle.exec(`CREATE TEMP TABLE task_title_backfill (
        id TEXT PRIMARY KEY, title TEXT NOT NULL
      )`);
      const insert = handle.prepare('INSERT INTO task_title_backfill VALUES (?, ?)');
      for (const row of handle.prepare('SELECT id, title FROM tasks').all()) {
        const title = canonicalizeTaskTitleV96(row.title);
        if (title !== row.title) insert.run(row.id, title);
      }
    },
    up: `
      UPDATE tasks SET title = (SELECT title FROM task_title_backfill WHERE id = tasks.id)
        WHERE id IN (SELECT id FROM task_title_backfill);
      DROP TABLE task_title_backfill;
    `,
  },
  {
    version: 97,
    name: 'grok_runtime_transition_target',
    up: `
      DROP TRIGGER agents_runtime_transition_pair_insert;
      DROP TRIGGER agents_runtime_transition_pair_update;
      CREATE TRIGGER agents_runtime_transition_pair_insert BEFORE INSERT ON agents
      WHEN (NEW.runtime_transition_target IS NULL) != (NEW.runtime_transition_started_at IS NULL)
        OR (NEW.runtime_transition_target IS NOT NULL AND NEW.runtime_transition_target NOT IN
          ('claude','codex','cursor','gemini','qoder','opencode','pi','grok'))
      BEGIN SELECT RAISE(ABORT, 'invalid agents runtime transition lock'); END;
      CREATE TRIGGER agents_runtime_transition_pair_update
      BEFORE UPDATE OF runtime_transition_target, runtime_transition_started_at ON agents
      WHEN (NEW.runtime_transition_target IS NULL) != (NEW.runtime_transition_started_at IS NULL)
        OR (NEW.runtime_transition_target IS NOT NULL AND NEW.runtime_transition_target NOT IN
          ('claude','codex','cursor','gemini','qoder','opencode','pi','grok'))
      BEGIN SELECT RAISE(ABORT, 'invalid agents runtime transition lock'); END;
    `,
  },
  {
    version: 98,
    name: 'retire_workspace_sort_for_sidebar',
    // Only the superseded top-level ordering is discarded. Channel favorites,
    // child ordering and all other preferences retain their existing authority.
    up: `
      DELETE FROM user_list_sort_mode WHERE list_key = 'workspaces';
      DELETE FROM user_sort_entry WHERE list_key = 'workspaces';
    `,
  },
  {
    version: 99,
    name: 'embedded_profile_setup',
    // Existing identities are complete. Only a new embedded bootstrap identity
    // explicitly opts in, in the transaction that creates the user.
    up: `ALTER TABLE users ADD COLUMN embedded_profile_pending INTEGER NOT NULL DEFAULT 0
      CHECK (embedded_profile_pending IN (0, 1));`,
  },
  {
    version: 100,
    name: 'avatar_designs',
    // A design is the editable source of the avatar image it rendered; display
    // still reads only avatar_storage_key. Each design belongs to exactly one
    // principal row and is deleted when that row drops or changes it.
    up: `
      CREATE TABLE avatar_designs (
        id            TEXT PRIMARY KEY,
        parts_version INTEGER NOT NULL CHECK (parts_version > 0),
        config        TEXT NOT NULL CHECK (json_valid(config) AND json_type(config) = 'object'),
        created_at    INTEGER NOT NULL
      );
      ALTER TABLE users ADD COLUMN avatar_design_id TEXT REFERENCES avatar_designs(id)
        CHECK (avatar_design_id IS NULL OR avatar_storage_key IS NOT NULL);
      ALTER TABLE agents ADD COLUMN avatar_design_id TEXT REFERENCES avatar_designs(id)
        CHECK (avatar_design_id IS NULL OR avatar_storage_key IS NOT NULL);
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
    `,
  },
  {
    version: 101,
    name: 'password_recovery_state',
    // One row per account: the live recovery code (keyed hash only) plus the
    // hourly send/attempt window, which a resend never resets.
    up: `
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
    `,
  },
  {
    version: 102,
    name: 'conversation_inbox_settlement_states',
    // An inbox row is pending, leased, delivered or failed; the stamps encode exactly one.
    // Rows written before the guard are normalized first: a settled row drops its lease.
    up: `
      UPDATE conversation_inboxes SET failed_at = NULL, failed_reason = NULL
        WHERE delivered_at IS NOT NULL AND failed_at IS NOT NULL;
      UPDATE conversation_inboxes SET in_flight_owner = NULL, in_flight_at = NULL
        WHERE (in_flight_owner IS NOT NULL OR in_flight_at IS NOT NULL)
          AND (in_flight_owner IS NULL OR in_flight_at IS NULL
               OR delivered_at IS NOT NULL OR failed_at IS NOT NULL);
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
    `,
  },
];

/**
 * Highest migration STEP this build knows. This is an internal implementation
 * number, not the version a release announces: one release ships as many steps as
 * it needs and moves the outward `CURRENT_SCHEMA_EPOCH` (see `schema-epochs.ts`)
 * at most once (#task:2103).
 */
export const CURRENT_MIGRATION_STEP = migrations[migrations.length - 1]?.version ?? 0;

// Baseline compaction floor (#task:1213): historical migrations v1..v44 were
// folded into the single `baseline` snapshot at version 44, so 44 is now the
// lowest step that can upgrade in place. This is a SAFETY invariant, not just
// a support policy — the baseline is a full CREATE, so a non-empty DB below it
// (ledger missing 44) MUST be rejected by the floor guard in
// packages/backend/src/db.ts rather than have the baseline rebuild existing
// tables. A pre-baseline DB must first upgrade through an intermediate release.
// Brand-new empty databases are still initialized from scratch (isEmpty bypass).
// It is also the LOWER BOUND for reading a ledger (#task:2103): a real
// pre-baseline database still carries rows for v1..v43, and those rows are
// history, not unknown steps. That floor is `BASELINE_MIGRATION_STEP` in
// baseline-schema.ts.
//
// The floor the RUNNER enforces is a different question — how old a release we
// promise to upgrade from — and it lives on the outward axis as
// `MIN_SUPPORTED_SCHEMA_EPOCH` in schema-epochs.ts, which derives
// `MIN_SUPPORTED_MIGRATION_STEP` from it (#task:2103). Support policy is stated
// in releases, so it is declared in releases' units; only its derived step is
// compared against a ledger.
