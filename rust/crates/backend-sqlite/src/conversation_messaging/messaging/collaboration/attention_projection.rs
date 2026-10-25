//! SQLite authority for the tiered badge and per-workspace mode cold faces.

use crate::{
    conversation_messaging::messaging::MessagingStore,
    viewer_read_model::unread_read::{
        CHANNEL_DEFAULT_MODE, UnreadReadModelError, attention_badge_counts, directed_claim_count,
        effective_directed_mention,
    },
};
use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::{
        attention_preference::ChannelAttentionMode,
        attention_projection::{
            AttentionBadgeProjectionError, AttentionBadgeRollup, AttentionCount,
            AttentionModesRefusal, AttentionProjectionPort, ChannelAttentionTier,
            WorkspaceAttentionCount, WorkspaceAttentionModes,
        },
    },
    foundation::identity::{ConversationId, UserId, WorkspaceId},
    foundation::persistence::PersistenceFailure,
};
use silverhand_backend_observability::Observe;
use sqlx::Row;
use thiserror::Error;

// Composes the same crate-private directed-claim/count fragments and default tier
// as the badge authority. A predicate change therefore recompiles both faces.
const CHANNEL_TIERS: &str = concat!(
    "WITH p(viewer, workspace, channel_default_mode) AS (SELECT ?, ?, ?) \
     SELECT c.id AS conversation_id, COALESCE((SELECT cap.mode FROM \
     channel_attention_preferences cap WHERE cap.user_id=(SELECT viewer FROM p) AND \
     cap.conversation_id=c.id), (SELECT channel_default_mode FROM p)) AS mode, ",
    "(SELECT ",
    directed_claim_count!(),
    " FROM notifications n WHERE n.user_id=(SELECT viewer FROM p) AND \
     n.conversation_id=c.id AND ",
    effective_directed_mention!(),
    ") AS directed_unread_count FROM conversations c JOIN conversation_users cu ON \
     cu.conversation_id=c.id AND cu.user_id=(SELECT viewer FROM p) WHERE \
     c.workspace_id=(SELECT workspace FROM p) AND c.placement_kind='workspace' AND \
     c.roster_policy='open' AND c.id IS NOT NULL AND c.archived_at IS NULL ORDER BY \
     c.created_at ASC, c.id ASC"
);

const MUTED_VISIBLE_WORKSPACES: &str = r#"
SELECT p.workspace_id
  FROM workspace_attention_preferences p
  JOIN workspace_users wu
    ON wu.workspace_id=p.workspace_id AND wu.user_id=?
 WHERE p.user_id=? AND p.muted=1
 ORDER BY p.workspace_id
"#;

#[derive(Clone)]
pub struct SqliteAttentionProjection {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn attention_projection(&self) -> SqliteAttentionProjection {
        SqliteAttentionProjection {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum AttentionProjectionStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("tiered badge authority failed")]
    Badge(#[from] UnreadReadModelError),
    #[error("stored attention projection identity is malformed")]
    MalformedIdentity,
    #[error("stored attention mode is malformed")]
    MalformedMode,
    #[error("stored workspace mute flag is malformed")]
    MalformedMute,
    #[error("stored attention count is malformed")]
    MalformedCount,
    #[error("attention badge total overflowed")]
    TotalOverflow,
    #[error("attention badge authority returned a duplicate workspace")]
    DuplicateWorkspace,
}

impl From<AttentionProjectionStoreError> for PersistenceFailure {
    fn from(value: AttentionProjectionStoreError) -> Self {
        Self::from_source(value)
    }
}

impl AttentionProjectionPort for SqliteAttentionProjection {
    type Error = PersistenceFailure;

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.attention_badges",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn badges(&self, viewer: &UserId) -> Result<AttentionBadgeRollup, Self::Error> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
            let mut tx =
                crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                    .await
                    .map_err(AttentionProjectionStoreError::from)?;
            let badge_rows = attention_badge_counts(&mut tx, viewer)
                .await
                .map_err(AttentionProjectionStoreError::from)?;
            let muted_rows = sqlx::query(MUTED_VISIBLE_WORKSPACES)
                .bind(viewer.as_str())
                .bind(viewer.as_str())
                .fetch_all(&mut *tx)
                .observe(silverhand_backend_observability::operation_span!(
                    "db.attention.muted_workspaces"
                ))
                .await
                .map_err(AttentionProjectionStoreError::from)?;

            tx.commit()
                .await
                .map_err(AttentionProjectionStoreError::from)?;
            let mut by_workspace = Vec::with_capacity(badge_rows.len());
            for (workspace, raw_count) in badge_rows {
                let count = AttentionCount::new(raw_count)
                    .map_err(|_| AttentionProjectionStoreError::MalformedCount)?;
                if count.get() > 0 {
                    by_workspace.push(WorkspaceAttentionCount::projected(workspace, count));
                }
            }
            let mut muted_workspaces = Vec::with_capacity(muted_rows.len());
            for row in muted_rows {
                let raw: String = row
                    .try_get("workspace_id")
                    .map_err(AttentionProjectionStoreError::from)?;
                let workspace = WorkspaceId::parse(raw)
                    .map_err(|_| AttentionProjectionStoreError::MalformedIdentity)?;
                muted_workspaces.push(workspace);
            }
            let projected = AttentionBadgeRollup::projected(by_workspace, muted_workspaces)
                .map_err(|error| match error {
                    AttentionBadgeProjectionError::DuplicateWorkspace => {
                        AttentionProjectionStoreError::DuplicateWorkspace
                    }
                    AttentionBadgeProjectionError::TotalOverflow => {
                        AttentionProjectionStoreError::TotalOverflow
                    }
                })?;
            Ok(projected)
        }
        .await;
        observation.finish(&result);
        result
    }

    #[tracing::instrument(
        target = "silverhand::diagnostics",
        name = "db.attention_modes",
        skip_all,
        fields(
            elapsed_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            operation_finished = tracing::field::Empty,
        )
    )]
    async fn modes(
        &self,
        viewer: &UserId,
        workspace: &WorkspaceId,
    ) -> Result<Result<WorkspaceAttentionModes, AttentionModesRefusal>, Self::Error> {
        let observation = silverhand_backend_observability::Operation::current();
        let result = async {
        let mut tx =
            crate::foundation::statement_metrics::begin(&self.pool, Some("BEGIN DEFERRED"))
                .await
                .map_err(AttentionProjectionStoreError::from)?;
        let visible = sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM workspace_users WHERE workspace_id=? AND user_id=?",
        )
        .bind(workspace.as_str())
        .bind(viewer.as_str())
        .fetch_optional(&mut *tx)
        .observe(silverhand_backend_observability::operation_span!("db.attention.workspace_membership"))
        .await
        .map_err(AttentionProjectionStoreError::from)?
        .is_some();
        if !visible {
            tx.rollback()
                .await
                .map_err(AttentionProjectionStoreError::from)?;
            return Ok(Err(AttentionModesRefusal::Forbidden));
        }

        let raw_muted = sqlx::query_scalar::<_, i64>(
            "SELECT muted FROM workspace_attention_preferences WHERE user_id=? AND workspace_id=?",
        )
        .bind(viewer.as_str())
        .bind(workspace.as_str())
        .fetch_optional(&mut *tx)
        .observe(silverhand_backend_observability::operation_span!("db.attention.workspace_preference"))
        .await
        .map_err(AttentionProjectionStoreError::from)?;
        let muted = match raw_muted {
            None | Some(0) => false,
            Some(1) => true,
            Some(_) => return Err(AttentionProjectionStoreError::MalformedMute.into()),
        };

        let rows = sqlx::query(CHANNEL_TIERS)
            .bind(viewer.as_str())
            .bind(workspace.as_str())
            .bind(CHANNEL_DEFAULT_MODE)
            .fetch_all(&mut *tx)
            .observe(silverhand_backend_observability::operation_span!("db.attention.channel_tiers"))
            .await
            .map_err(AttentionProjectionStoreError::from)?;
        tx.commit()
            .await
            .map_err(AttentionProjectionStoreError::from)?;
        let mut channels = Vec::with_capacity(rows.len());
        for row in rows {
            let raw_conversation: String = row
                .try_get("conversation_id")
                .map_err(AttentionProjectionStoreError::from)?;
            let raw_mode: String = row
                .try_get("mode")
                .map_err(AttentionProjectionStoreError::from)?;
            let raw_count: i64 = row
                .try_get("directed_unread_count")
                .map_err(AttentionProjectionStoreError::from)?;
            channels.push(ChannelAttentionTier::projected(
                ConversationId::parse(raw_conversation)
                    .map_err(|_| AttentionProjectionStoreError::MalformedIdentity)?,
                ChannelAttentionMode::parse(&raw_mode)
                    .ok_or(AttentionProjectionStoreError::MalformedMode)?,
                AttentionCount::new(raw_count)
                    .map_err(|_| AttentionProjectionStoreError::MalformedCount)?,
            ));
        }
        Ok(Ok(WorkspaceAttentionModes::projected(muted, channels)))

        }.await;
        observation.finish(&result);
        result
    }
}
