//! The `mention_self` notification row: the durable record that a person was named
//! in a conversation they can read. Written by message send (an `@` mention) and by
//! task assignment (a human assignee); both ride the directed-mention badge,
//! Activity and read clearing.

use sqlx::SqliteConnection;

/// Coordinates are the source's, because that is what the reader can open.
const INSERT_MENTION_SELF: &str = "INSERT INTO notifications (id, kind, user_id, workspace_id, \
     conversation_id, message_id, source_workspace_id, source_conversation_id, source_message_id, \
     payload, created_at) VALUES (?1, 'mention_self', ?2, ?3, ?4, ?5, ?3, ?4, ?5, ?6, ?7)";

pub(crate) struct MentionSelfRow<'a> {
    pub(crate) id: &'a str,
    pub(crate) viewer: &'a str,
    /// `None` for a direct pair.
    pub(crate) workspace_id: Option<&'a str>,
    pub(crate) conversation_id: &'a str,
    pub(crate) message_id: &'a str,
    pub(crate) payload: String,
    pub(crate) created_at: i64,
}

pub(crate) async fn insert_mention_self(
    conn: &mut SqliteConnection,
    row: MentionSelfRow<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(INSERT_MENTION_SELF)
        .bind(row.id)
        .bind(row.viewer)
        .bind(row.workspace_id)
        .bind(row.conversation_id)
        .bind(row.message_id)
        .bind(row.payload)
        .bind(row.created_at)
        .execute(conn)
        .await
        .map(|_| ())
}
