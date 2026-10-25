//! SQLite adapter for the canonical, capability-NEUTRAL `agents`-row write behind the
//! public agent-lifecycle routes (`PATCH /api/agents/{id}` and the avatar upload/removal
//! today; internal-profile next).
//!
//! Like the other write stores, this is a **pure consumer** of an existing product
//! database: it opens a pool against a schema that already exists and migrates or
//! seeds nothing. It owns no product *rules* — this port names no capability's gate.
//! Inside its own write lock it reads the LOCKED current `runtime`/`model`/
//! `reasoning_effort` into an identity-bound before-image and hands it to the
//! caller-supplied DECISION closure, which decides WHETHER to write: the public tuning
//! route injects
//! [`silverhand_backend_core::identity_access::agent_profile_write::admit_public_tuning_edit`] (the one
//! copy of the runtime tuning gate) against the locked before-context. The adapter does
//! NOT mint the product verdict. A verdict is the domain's to derive: this adapter
//! reports only DB-observable WITNESSES (the before-image it read, and the committed row
//! on a write), and the core use case replays the same decision against that witness.
//! Reading under the lock makes the fact the decision judged the same fact the row
//! commits, so two concurrent PATCHes can never compose an illegal final runtime state;
//! reporting a witness (not a verdict) keeps a buggy adapter from fabricating a refusal
//! the core cannot replay.
//!
//! The whole write is one `BEGIN IMMEDIATE` transaction. First a locked
//! `SELECT` of the row's `id`, tuning columns, and coupled avatar pair `WHERE id = ?`:
//! no row → [`AgentProfileRowWrite::NoSuchAgent`] (the surface's `404`, zero `UPDATE`);
//! a row whose `runtime` cannot be decoded, or whose id / avatar pair is malformed, is a
//! persistence-integrity fault. The before-image witness binds its `agent_id` AND its
//! avatar state to the SELECTED row's OWN columns — decoded WHOLE through the domain's
//! one shared avatar rule, never back-filled from the query parameter — so the core can
//! confirm the witness was read for the request subject before it replays the decision.
//! Then the injected `decide(&before)` chooses the branch: `None` returns
//! [`AgentProfileRowWrite::Unchanged`] carrying that before-image, with zero `UPDATE`;
//! `Some(edit)` drives the `UPDATE`. Only the columns the admitted edit names are placed
//! in the `SET` clause — a `Leave` is omitted, a `Clear` writes `NULL`, a `Set` binds the
//! value, and the avatar's two columns are always written or cleared TOGETHER — and
//! `updated_at` is always stamped to `now_ms`, so a committed write always bumps the
//! row's instant (matching `agent-service.ts`'s always-touch update). The committed
//! columns are projected from the write's OWN `RETURNING` row — never a post-commit
//! re-read — and returned alongside the before-image as
//! [`AgentProfileRowWrite::Committed`], so the core replay can re-derive the admitted
//! intent and seal the fact this write produced.
//!
//! The clock stays in the application: `now_ms` is passed in, exactly as the other
//! write stores take it. Every driver error and every committed row whose id cannot
//! be decoded reaches the caller as the source of a [`PersistenceFailure`]; nothing
//! is logged here.

use std::{path::Path, time::Duration};

use silverhand_backend_core::{
    foundation::identity::AgentId,
    foundation::persistence::PersistenceFailure,
    identity_access::agent_avatar::{
        AgentAvatarRow, AgentAvatarRowCorruption, AgentAvatarServeRef, decode_agent_avatar_row,
    },
    identity_access::agent_profile_write::{
        AdmittedAgentProfileEdit, AgentProfileRowWrite, AgentProfileWriteUnitOfWork,
        AgentRowBefore, AgentTuningContext, CommittedAgentRow, FieldEdit, RequiredEdit,
    },
    identity_access::agents::runtime::RuntimeKind,
};
use sqlx::{QueryBuilder, Row, Sqlite, sqlite::SqliteRow};
use thiserror::Error;

const ROOT_WRITE_BEGIN: &str = "BEGIN IMMEDIATE";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

// The committed columns the core seal binds against the request: the id, the touched
// tuning columns, and the write instant. A FIXED `RETURNING` suffix — it does not
// vary with which columns the `SET` clause touched, so the write always projects the
// same shape the seal reads.
const RETURNING_COMMITTED_ROW: &str = " RETURNING id, display_name, description, model, \
     reasoning_effort, fast_mode, avatar_storage_key, avatar_content_type, updated_at";

/// A pooled connection to an existing product database, offering the canonical
/// agent-tuning write. Holds no clock; `now_ms` is passed in by the application.
#[derive(Clone)]
pub struct SqliteAgentProfileWrite {
    pool: crate::foundation::database::SqliteDatabase,
}

/// Startup and driver failures surfaced only to the composition root that opens the
/// store. The unit-of-work method hands the driver error over as the source of a
/// [`PersistenceFailure`] instead.
#[derive(Debug, Error)]
pub enum SqliteAgentProfileWriteError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
}

/// A stored `agents.runtime` that the domain refuses to decode. The product schema
/// constrains this column to the known runtime kinds, so an undecodable value is a
/// persistence-integrity fault surfaced as the source of a [`PersistenceFailure`] —
/// never coerced into a baseline runtime that would silently widen the tuning gate.
#[derive(Debug, Error)]
#[error("agents.runtime holds an unknown runtime kind {value:?}")]
struct CorruptStoredRuntime {
    value: String,
}

/// A write that failed in the driver or on a row that cannot be decoded into the
/// domain. All three reach the caller as the source of one `PersistenceFailure`; the
/// split lets an integrity fault name its cause rather than masquerade as a driver
/// error. The agent id AND the avatar pair are decoded through the shared
/// [`decode_agent_avatar_row`] rule, so a malformed id or a half-populated avatar pair
/// is [`AvatarCorruption`], caught before the decision or the seal ever runs.
#[derive(Debug, Error)]
enum ProfileWriteFailure {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    AvatarCorruption(#[from] AgentAvatarRowCorruption),
    #[error(transparent)]
    CorruptRuntime(#[from] CorruptStoredRuntime),
    #[error(transparent)]
    Design(#[from] super::avatar_design::DesignInsertFailure),
}

impl SqliteAgentProfileWrite {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SqliteAgentProfileWriteError> {
        let pool = crate::foundation::database::store_pool(path.as_ref(), BUSY_TIMEOUT).await?;
        Ok(Self { pool })
    }

    /// Construct over an already-open pool, so a composition root that shares one
    /// pool across adapters need not reopen the file.
    pub fn from_pool(pool: impl Into<crate::foundation::database::SqliteDatabase>) -> Self {
        let pool = pool.into();
        Self { pool }
    }

    async fn write_row<D>(
        &self,
        agent: &AgentId,
        decide: D,
        now_ms: i64,
    ) -> Result<AgentProfileRowWrite, ProfileWriteFailure>
    where
        D: FnOnce(&AgentRowBefore) -> Option<AdmittedAgentProfileEdit>,
    {
        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;

        // Under the write lock, read the row's CURRENT tuning state. This is both the
        // existence check (no row → the surface's 404) and the context the tuning
        // gate is replayed against — so the fact the gate judges is the fact this
        // transaction will commit, with no window for a concurrent PATCH to slip a
        // stale legality verdict past it.
        let current = sqlx::query(
            "SELECT id, runtime, model, reasoning_effort, display_name, description, fast_mode, \
                    avatar_storage_key, avatar_content_type \
               FROM agents WHERE id = ?",
        )
        .bind(agent.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current) = current else {
            tx.commit().await?;
            return Ok(AgentProfileRowWrite::NoSuchAgent);
        };

        // Bind the before-image to the SELECTED row's OWN id and avatar columns —
        // never back-filled from the query parameter — so the witness the core replays
        // against carries the identity AND avatar state it was actually read for. The id
        // and the coupled avatar pair are decoded WHOLE through the one shared rule; a
        // malformed id or a half-populated avatar pair is corruption caught here, before
        // the decision or the seal ever runs.
        let (before_id, before_avatar) = decode_id_and_avatar(&current)?;
        let runtime_raw: String = current.try_get("runtime")?;
        let runtime =
            RuntimeKind::parse(&runtime_raw).ok_or(CorruptStoredRuntime { value: runtime_raw })?;
        let fast_mode: Option<i64> = current.try_get("fast_mode")?;
        let before = AgentRowBefore {
            agent_id: before_id,
            display_name: current.try_get("display_name")?,
            description: current.try_get("description")?,
            fast_mode: fast_mode.map(|value| value != 0),
            avatar: before_avatar,
            context: AgentTuningContext {
                runtime,
                model: current.try_get("model")?,
                reasoning_effort: current.try_get("reasoning_effort")?,
            },
        };

        // Hand the LOCKED before-image to the caller-supplied decision. The adapter
        // names no capability's rule: `None` means the capability refuses this write, so
        // no UPDATE is issued and `updated_at` does not move; `Some(edit)` is the sealed
        // intent that drives the SET. This adapter never returns the product verdict —
        // that stays the domain's to re-derive from the before-image witness.
        let admitted = match decide(&before) {
            Some(admitted) => admitted,
            None => {
                tx.commit().await?;
                return Ok(AgentProfileRowWrite::Unchanged { before });
            }
        };

        // Build the `SET` clause from ONLY the columns the admitted edit names,
        // through sqlx's `QueryBuilder` so every value goes through an audited
        // `push_bind` (never string interpolation). A `Leave` contributes nothing; a
        // `Clear` writes a literal `NULL`; a `Set` binds the value. `updated_at` is
        // always appended, so the `SET` clause is never empty — a committed tuning
        // write always bumps the instant.
        let design_id = match admitted.avatar() {
            FieldEdit::Set(assignment) => {
                super::avatar_design::insert_design(&mut tx, assignment.design.as_ref(), now_ms)
                    .await?
            }
            _ => None,
        };
        let mut builder = QueryBuilder::<Sqlite>::new("UPDATE agents SET ");
        let mut needs_comma = false;

        if let RequiredEdit::Set(value) = admitted.display_name() {
            separator(&mut builder, &mut needs_comma);
            builder.push("display_name = ");
            builder.push_bind(value.as_str());
        }
        push_nullable_str(
            &mut builder,
            &mut needs_comma,
            "description",
            admitted.description(),
        );
        push_nullable_str(&mut builder, &mut needs_comma, "model", admitted.model());
        push_nullable_str(
            &mut builder,
            &mut needs_comma,
            "reasoning_effort",
            admitted.reasoning_effort(),
        );
        match admitted.fast_mode() {
            FieldEdit::Leave => {}
            FieldEdit::Clear => {
                separator(&mut builder, &mut needs_comma);
                builder.push("fast_mode = NULL");
            }
            FieldEdit::Set(value) => {
                separator(&mut builder, &mut needs_comma);
                builder.push("fast_mode = ");
                builder.push_bind(*value);
            }
        }
        // The avatar is a COUPLED pair: the storage key and the content type are always
        // written or cleared together, never one without the other, so the row can never
        // hold a half-populated pair the decode rule would reject on the next read. A
        // `Leave` omits both; a `Clear` NULLs both; a `Set` binds both.
        match admitted.avatar() {
            FieldEdit::Leave => {}
            FieldEdit::Clear => {
                separator(&mut builder, &mut needs_comma);
                builder.push(
                    "avatar_storage_key = NULL, avatar_content_type = NULL, avatar_design_id = NULL",
                );
            }
            FieldEdit::Set(assignment) => {
                separator(&mut builder, &mut needs_comma);
                builder.push("avatar_storage_key = ");
                builder.push_bind(assignment.pointer.as_str());
                builder.push(", avatar_content_type = ");
                builder.push_bind(assignment.content_type.as_str());
                builder.push(", avatar_design_id = ");
                builder.push_bind(design_id);
            }
        }

        separator(&mut builder, &mut needs_comma);
        builder.push("updated_at = ");
        builder.push_bind(now_ms);

        builder.push(" WHERE id = ");
        builder.push_bind(agent.as_str());
        builder.push(RETURNING_COMMITTED_ROW);

        // Existence was confirmed under this same lock, so the `RETURNING` row is
        // present; a missing row here would be an in-transaction integrity anomaly.
        let row = builder.build().fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(AgentProfileRowWrite::NoSuchAgent);
        };
        let committed = decode_committed_row(&row)?;

        tx.commit().await?;
        Ok(AgentProfileRowWrite::Committed {
            before,
            row: committed,
        })
    }
}

/// Emit a `, ` before every assignment after the first, so the `SET` list stays
/// well-formed however many columns the sealed edit touched.
fn separator(builder: &mut QueryBuilder<Sqlite>, needs_comma: &mut bool) {
    if *needs_comma {
        builder.push(", ");
    }
    *needs_comma = true;
}

/// Append a nullable STRING column's assignment if the edit names it: `Clear` writes
/// a literal `NULL`; `Set` binds through an audited `push_bind`; `Leave` contributes
/// nothing. `column` is always a hardcoded literal from the call site, never caller
/// input.
fn push_nullable_str(
    builder: &mut QueryBuilder<Sqlite>,
    needs_comma: &mut bool,
    column: &'static str,
    edit: &FieldEdit<String>,
) {
    match edit {
        FieldEdit::Leave => {}
        FieldEdit::Clear => {
            separator(builder, needs_comma);
            builder.push(column);
            builder.push(" = NULL");
        }
        FieldEdit::Set(value) => {
            separator(builder, needs_comma);
            builder.push(column);
            builder.push(" = ");
            builder.push_bind(value.as_str());
        }
    }
}

/// Decode a row's read-back `id` + coupled avatar pair through the ONE shared core
/// rule ([`decode_agent_avatar_row`]). Both the locked before-image and the committed
/// `RETURNING` row go through this, so the id is bound to the row's OWN `id` column
/// (never the query parameter) and a malformed id or a half-populated avatar pair is an
/// integrity fault BEFORE the decision or the seal, never a silently-dropped column.
fn decode_id_and_avatar(row: &SqliteRow) -> Result<(AgentId, AgentAvatarRow), ProfileWriteFailure> {
    let row_id: String = row.try_get("id")?;
    let storage_key: Option<String> = row.try_get("avatar_storage_key")?;
    let content_type: Option<String> = row.try_get("avatar_content_type")?;
    // The shared decode yields `Row { source, avatar }` for any present row (the no-row
    // case is the caller's SELECT/RETURNING existence check, never this decode), so both
    // halves of the identity-bound fact come out of one rule, whole.
    let AgentAvatarServeRef::Row { source, avatar } =
        decode_agent_avatar_row(&row_id, storage_key.as_deref(), content_type.as_deref())?
    else {
        unreachable!("decode_agent_avatar_row yields Row for a present agents row");
    };
    Ok((source, avatar))
}

/// Decode the committed columns from the write's own `RETURNING` row. The id and the
/// coupled avatar pair are decoded WHOLE through [`decode_id_and_avatar`], so a
/// malformed committed id or a half-populated avatar pair is corruption, never a
/// product answer; `fast_mode` is read as an integer and folded `!= 0`, mirroring the
/// detail read's affinity-safe decode.
fn decode_committed_row(row: &SqliteRow) -> Result<CommittedAgentRow, ProfileWriteFailure> {
    let (id, avatar) = decode_id_and_avatar(row)?;
    let fast_mode: Option<i64> = row.try_get("fast_mode")?;
    let fast_mode = fast_mode.map(|value| value != 0);
    Ok(CommittedAgentRow {
        id,
        display_name: row.try_get("display_name")?,
        description: row.try_get("description")?,
        model: row.try_get("model")?,
        reasoning_effort: row.try_get("reasoning_effort")?,
        fast_mode,
        avatar,
        updated_at: row.try_get("updated_at")?,
    })
}

impl AgentProfileWriteUnitOfWork for SqliteAgentProfileWrite {
    async fn write_agent_row<D>(
        &self,
        agent: &AgentId,
        decide: D,
        now_ms: i64,
    ) -> Result<AgentProfileRowWrite, PersistenceFailure>
    where
        D: FnOnce(&AgentRowBefore) -> Option<AdmittedAgentProfileEdit> + Send,
    {
        self.write_row(agent, decide, now_ms)
            .await
            .map_err(PersistenceFailure::from_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::identity_access::agent_avatar::{
        AgentAvatarAssignment, AgentAvatarBlobPointer,
    };
    use silverhand_backend_core::identity_access::agent_profile_write::{
        AgentTuningEdit, admit_avatar_assignment, admit_avatar_removal, admit_public_tuning_edit,
    };
    use silverhand_backend_core::identity_access::avatar::AvatarContentType;
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{Connection, Executor, SqliteConnection};

    /// Drive the canonical writer exactly as the production PATCH route does: inject the
    /// public-tuning decision (`admit_public_tuning_edit` against the LOCKED
    /// before-context) as the `decide` closure. A refusal collapses to `None` (write
    /// nothing → `Unchanged`); an admit is the sealed edit that drives the `UPDATE`.
    async fn admit_and_apply(
        adapter: &SqliteAgentProfileWrite,
        agent: &AgentId,
        request: &AgentTuningEdit,
        now_ms: i64,
    ) -> Result<AgentProfileRowWrite, PersistenceFailure> {
        let request = request.clone();
        adapter
            .write_agent_row(
                agent,
                move |before| admit_public_tuning_edit(&before.context, request).ok(),
                now_ms,
            )
            .await
    }

    const CURRENT_SCHEMA: &str =
        include_str!("../../../../../rust/test-fixtures/migration-baseline/schema-current.sql");

    // A codex agent `agent-1` seeded with a full tuning state (model + reasoning +
    // fast_mode) so a Clear has something to erase; a claude agent `agent-2` whose
    // tuning columns start NULL; a codex agent `agent-3` at `gpt-5.6-sol` + `high` used
    // only by the under-lock re-admission falsifier; a claude agent `agent-4` seeded WITH
    // a valid coupled avatar pair so a Clear has one to erase and a Leave one to
    // preserve; and a claude agent `agent-5` seeded with an EMPTY storage key + a content
    // type — a pair that passes the schema's NULL-coupling CHECK yet the writer never
    // mints, so the decode rule rejects it as corruption BEFORE any decision.
    const SEED_SQL: &str = "\
INSERT INTO users (id, name, display_name, created_at, updated_at) \
  VALUES ('user-1', 'owner', 'Owner', 1, 1); \
INSERT INTO hosts (id, name, kind, created_at) VALUES ('host-1', 'Host', 'local', 1); \
INSERT INTO agents (id, name, display_name, runtime, model, reasoning_effort, fast_mode, description, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-1', 'alice', 'Alice', 'codex', 'gpt-5.6-sol', 'ultra', 1, 'first', 'host-1', 'user-1', 10, 100); \
INSERT INTO agents (id, name, display_name, runtime, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-2', 'bob', 'Bob', 'claude', 'host-1', 'user-1', 10, 100); \
INSERT INTO agents (id, name, display_name, runtime, model, reasoning_effort, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-3', 'carol', 'Carol', 'codex', 'gpt-5.6-sol', 'high', 'host-1', 'user-1', 10, 100); \
INSERT INTO agents (id, name, display_name, runtime, avatar_storage_key, avatar_content_type, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-4', 'dave', 'Dave', 'claude', 'agent-avatars/agent-4/blob-seed', 'image/png', 'host-1', 'user-1', 10, 100); \
INSERT INTO agents (id, name, display_name, runtime, avatar_storage_key, avatar_content_type, host_id, created_by_user_id, created_at, updated_at) \
  VALUES ('agent-5', 'erin', 'Erin', 'claude', '', 'image/png', 'host-1', 'user-1', 10, 100);";

    async fn open_seeded() -> (tempfile::TempDir, SqliteAgentProfileWrite) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent-profile-write.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .foreign_keys(true);
        let mut db = SqliteConnection::connect_with(&options).await.unwrap();
        db.execute(sqlx::raw_sql(CURRENT_SCHEMA)).await.unwrap();
        db.execute(sqlx::raw_sql(SEED_SQL)).await.unwrap();
        db.close().await.unwrap();
        let adapter = SqliteAgentProfileWrite::open(&path).await.unwrap();
        (directory, adapter)
    }

    fn agent(id: &str) -> AgentId {
        AgentId::parse(id).unwrap()
    }

    fn leave_all() -> AgentTuningEdit {
        AgentTuningEdit {
            display_name: RequiredEdit::Leave,
            description: FieldEdit::Leave,
            model: FieldEdit::Leave,
            reasoning_effort: FieldEdit::Leave,
            fast_mode: FieldEdit::Leave,
        }
    }

    async fn stored(
        adapter: &SqliteAgentProfileWrite,
        id: &str,
    ) -> (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        i64,
    ) {
        sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<String>,
                i64,
            ),
        >(
            "SELECT display_name, description, model, fast_mode, reasoning_effort, updated_at \
               FROM agents WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&adapter.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_set_edit_writes_only_named_columns_bumps_updated_at_and_returns_the_committed_row() {
        let (_dir, adapter) = open_seeded().await;
        // A raw request; the adapter admits it under the lock against agent-1's
        // seeded codex/`gpt-5.6-sol` context, where `high` is legal.
        let request = AgentTuningEdit {
            display_name: RequiredEdit::Set("Alice II".to_owned()),
            reasoning_effort: FieldEdit::Set("high".to_owned()),
            ..leave_all()
        };

        let outcome = admit_and_apply(&adapter, &agent("agent-1"), &request, 777)
            .await
            .unwrap();
        let AgentProfileRowWrite::Committed { before, row } = outcome else {
            panic!("expected Committed, got {outcome:?}");
        };
        // The before witness carries the id decoded from the SELECTED row, bound to the
        // subject the core will replay against.
        assert_eq!(before.agent_id, agent("agent-1"));
        assert_eq!(row.id, agent("agent-1"));
        assert_eq!(row.display_name, "Alice II");
        assert_eq!(row.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(row.updated_at, 777);
        // The left-alone columns kept their seeded values.
        assert_eq!(row.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(row.fast_mode, Some(true));
        assert_eq!(row.description.as_deref(), Some("first"));

        // And the row on disk matches, with updated_at bumped off its seed.
        let (display, description, model, fast, reasoning, updated) =
            stored(&adapter, "agent-1").await;
        assert_eq!(display, "Alice II");
        assert_eq!(description.as_deref(), Some("first"));
        assert_eq!(model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(fast, Some(1));
        assert_eq!(reasoning.as_deref(), Some("high"));
        assert_eq!(updated, 777);
    }

    #[tokio::test]
    async fn a_clear_writes_null_and_leaves_untouched_columns_alone() {
        let (_dir, adapter) = open_seeded().await;
        // Clearing model would strand the stored `ultra` effort, so also clear it —
        // the under-lock admission accepts this (baseline codex + no effort is legal).
        let request = AgentTuningEdit {
            model: FieldEdit::Clear,
            reasoning_effort: FieldEdit::Clear,
            description: FieldEdit::Clear,
            ..leave_all()
        };

        let outcome = admit_and_apply(&adapter, &agent("agent-1"), &request, 900)
            .await
            .unwrap();
        let AgentProfileRowWrite::Committed { row, .. } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(row.model, None);
        assert_eq!(row.reasoning_effort, None);
        assert_eq!(row.description, None);
        // display_name and fast_mode were left alone.
        assert_eq!(row.display_name, "Alice");
        assert_eq!(row.fast_mode, Some(true));
        assert_eq!(row.updated_at, 900);
    }

    #[tokio::test]
    async fn a_leave_only_edit_still_bumps_updated_at_and_touches_no_value() {
        let (_dir, adapter) = open_seeded().await;
        // An all-Leave request (the route forbids an empty body, but the adapter is
        // defensive): the SET clause is `updated_at = ?` alone.
        let outcome = admit_and_apply(&adapter, &agent("agent-1"), &leave_all(), 1234)
            .await
            .unwrap();
        let AgentProfileRowWrite::Committed { row, .. } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(row.display_name, "Alice");
        assert_eq!(row.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(row.updated_at, 1234);
    }

    #[tokio::test]
    async fn a_fast_mode_set_writes_the_integer_zero() {
        let (_dir, adapter) = open_seeded().await;
        // agent-1 (codex) starts with fast_mode 1; codex supports fast mode, so set
        // it false → 0.
        let request = AgentTuningEdit {
            fast_mode: FieldEdit::Set(false),
            ..leave_all()
        };
        let outcome = admit_and_apply(&adapter, &agent("agent-1"), &request, 1500)
            .await
            .unwrap();
        let AgentProfileRowWrite::Committed { row, .. } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(row.fast_mode, Some(false));
        assert_eq!(stored(&adapter, "agent-1").await.3, Some(0));
    }

    #[tokio::test]
    async fn a_missing_row_is_no_such_agent_and_writes_nothing() {
        let (_dir, adapter) = open_seeded().await;
        let request = AgentTuningEdit {
            display_name: RequiredEdit::Set("Ghost".to_owned()),
            ..leave_all()
        };
        let outcome = admit_and_apply(&adapter, &agent("agent-missing"), &request, 2000)
            .await
            .unwrap();
        assert!(matches!(outcome, AgentProfileRowWrite::NoSuchAgent));
        // The real agents are untouched.
        assert_eq!(stored(&adapter, "agent-1").await.5, 100);
    }

    #[tokio::test]
    async fn an_effort_is_rejected_after_an_earlier_model_change_and_writes_nothing() {
        let (_dir, adapter) = open_seeded().await;

        let a = admit_and_apply(
            &adapter,
            &agent("agent-3"),
            &AgentTuningEdit {
                model: FieldEdit::Set("gpt-5.6-terra".to_owned()),
                ..leave_all()
            },
            500,
        )
        .await
        .unwrap();
        assert!(
            matches!(a, AgentProfileRowWrite::Committed { .. }),
            "A: {a:?}"
        );
        let (_, _, model_a, _, effort_a, updated_a) = stored(&adapter, "agent-3").await;
        assert_eq!(model_a.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(effort_a.as_deref(), Some("high"));
        assert_eq!(updated_a, 500);

        // `ultra` was legal against the original `sol` state. These sequential
        // calls prove re-reading current values, not concurrent lock scheduling.
        let b = admit_and_apply(
            &adapter,
            &agent("agent-3"),
            &AgentTuningEdit {
                reasoning_effort: FieldEdit::Set("ultra".to_owned()),
                ..leave_all()
            },
            900,
        )
        .await
        .unwrap();
        // Refused under the lock → the adapter writes nothing and reports the LOCKED
        // before-image it read (codex + `terra` + `high`), NOT a verdict, bound to the
        // agent id decoded from the SELECTED row. The domain replays the rule against
        // this witness to mint the 400; a `terra + ultra` row is never composed.
        let AgentProfileRowWrite::Unchanged { before } = b else {
            panic!("B must report an Unchanged witness under the lock, got {b:?}");
        };
        assert_eq!(before.agent_id, agent("agent-3"));
        assert_eq!(before.context.runtime, RuntimeKind::Codex);
        assert_eq!(before.context.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(before.context.reasoning_effort.as_deref(), Some("high"));
        // Zero UPDATE: model, effort, and the instant are exactly as A left them —
        // the illegal `terra + ultra` combination never reached the row.
        let (_, _, model_b, _, effort_b, updated_b) = stored(&adapter, "agent-3").await;
        assert_eq!(model_b.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(effort_b.as_deref(), Some("high"));
        assert_eq!(updated_b, 500);
    }

    /// The raw coupled avatar columns as they sit on disk, for asserting a `Set` binds
    /// both and a `Clear` NULLs both.
    async fn stored_avatar(
        adapter: &SqliteAgentProfileWrite,
        id: &str,
    ) -> (Option<String>, Option<String>) {
        sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT avatar_storage_key, avatar_content_type FROM agents WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&adapter.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_avatar_assignment_writes_both_paired_columns_and_returns_present() {
        let (_dir, adapter) = open_seeded().await;
        // agent-2 starts with no avatar; assign one — both coupled columns must land
        // together and the committed row must report the paired Present state.
        let assignment = AgentAvatarAssignment {
            pointer: AgentAvatarBlobPointer::parse("agent-avatars/agent-2/blob-0").unwrap(),
            content_type: AvatarContentType::Png,
            design: None,
        };
        let expected = assignment.clone();
        let outcome = adapter
            .write_agent_row(
                &agent("agent-2"),
                move |_before| Some(admit_avatar_assignment(&assignment)),
                4000,
            )
            .await
            .unwrap();
        let AgentProfileRowWrite::Committed { before, row } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(before.avatar, AgentAvatarRow::Absent);
        assert_eq!(
            row.avatar,
            AgentAvatarRow::Present {
                pointer: expected.pointer.clone(),
                content_type: AvatarContentType::Png,
            }
        );
        assert_eq!(row.updated_at, 4000);
        let (key, mime) = stored_avatar(&adapter, "agent-2").await;
        assert_eq!(key.as_deref(), Some("agent-avatars/agent-2/blob-0"));
        assert_eq!(mime.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn an_avatar_removal_nulls_both_paired_columns_and_returns_absent() {
        let (_dir, adapter) = open_seeded().await;
        // agent-4 is seeded WITH a coupled avatar pair; a removal must NULL both columns
        // together and the committed row must report Absent, with the before-image
        // carrying the seeded Present state it read under the lock.
        let outcome = adapter
            .write_agent_row(
                &agent("agent-4"),
                |_before| Some(admit_avatar_removal()),
                4100,
            )
            .await
            .unwrap();
        let AgentProfileRowWrite::Committed { before, row } = outcome else {
            panic!("expected Committed");
        };
        assert!(
            matches!(before.avatar, AgentAvatarRow::Present { .. }),
            "the before witness must carry the seeded avatar it read"
        );
        assert_eq!(row.avatar, AgentAvatarRow::Absent);
        assert_eq!(row.updated_at, 4100);
        let (key, mime) = stored_avatar(&adapter, "agent-4").await;
        assert_eq!(key, None);
        assert_eq!(mime, None);
    }

    #[tokio::test]
    async fn a_leave_avatar_edit_preserves_the_seeded_pair_in_the_committed_row() {
        let (_dir, adapter) = open_seeded().await;
        // A pure tuning edit (avatar Leave) on agent-4 must not touch the avatar columns:
        // the RETURNING row still carries the seeded Present pair, whole.
        let request = AgentTuningEdit {
            display_name: RequiredEdit::Set("Dave II".to_owned()),
            ..leave_all()
        };
        let outcome = admit_and_apply(&adapter, &agent("agent-4"), &request, 4200)
            .await
            .unwrap();
        let AgentProfileRowWrite::Committed { row, .. } = outcome else {
            panic!("expected Committed");
        };
        assert_eq!(row.display_name, "Dave II");
        assert_eq!(
            row.avatar,
            AgentAvatarRow::Present {
                pointer: AgentAvatarBlobPointer::parse("agent-avatars/agent-4/blob-seed").unwrap(),
                content_type: AvatarContentType::Png,
            }
        );
        let (key, mime) = stored_avatar(&adapter, "agent-4").await;
        assert_eq!(key.as_deref(), Some("agent-avatars/agent-4/blob-seed"));
        assert_eq!(mime.as_deref(), Some("image/png"));
    }

    #[tokio::test]
    async fn a_corrupt_stored_avatar_pair_is_an_integrity_fault_before_the_decision() {
        let (_dir, adapter) = open_seeded().await;
        // agent-5's stored key is empty — a pair the writer never mints. The decode of
        // the LOCKED before-image must reject it as corruption BEFORE `decide` runs, so a
        // decision closure that panics is never consulted and the call returns a
        // persistence error rather than composing a write off a corrupt witness.
        let result = adapter
            .write_agent_row(
                &agent("agent-5"),
                |_before| panic!("the decision must never run on a corrupt before-image"),
                4300,
            )
            .await;
        assert!(
            result.is_err(),
            "an empty stored avatar key is corruption, not a decision input"
        );
    }
}
