//! SQLite adapter for message answer options: the `message_options` row, the answer
//! and options-update units of work, and the batch read every message reader joins.

use std::collections::HashMap;

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::message_options::{
        AnswerCommand, AnswerEventMessage, AnswerRefusal, AnswerWithdrawnFact, AnsweredFact,
        Answerer, MessageAnswer, MessageOptions, MessageOptionsPort, MessageOptionsState,
        NamedPrincipal, OptionMode, OptionsActor, OptionsUpdatedFact, QuestionCoordinates,
        UpdateOptionsCommand, WithdrawAnswerCommand, answer_event, decide_answer, decide_update,
        decide_withdraw, withdraw_event,
    },
    conversation_messaging::messaging::collaboration::reaction::ReactionActor,
    conversation_messaging::messaging::{MessageId, PrincipalIdentity},
    foundation::identity::{AgentId, UserId},
};
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection, Transaction};
use thiserror::Error;

use super::reaction::{ReactionAccess, conversation_facts, reaction_access};
use crate::conversation_messaging::messaging::{
    FROM_KIND_SYSTEM, INSERT_MESSAGE, MessagingStore, ReserveSeqFault, bump_stream_head_on,
    reserve_next_seq_on,
};

#[derive(Clone)]
pub struct SqliteMessageOptionsStore {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn message_options(&self) -> SqliteMessageOptionsStore {
        SqliteMessageOptionsStore {
            pool: self.pool.clone(),
        }
    }
}

#[derive(Debug, Error)]
pub enum MessageOptionsStoreError {
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    #[error("stored message options are malformed")]
    Malformed,
    #[error("message sequence could not be reserved")]
    Ordinal,
    #[error("entropy unavailable")]
    Entropy,
    #[error(transparent)]
    Reaction(#[from] super::reaction::ReactionStoreError),
}

fn items_json(options: &MessageOptions) -> String {
    serde_json::Value::Array(
        options
            .items()
            .iter()
            .map(|item| serde_json::json!({ "id": item.id(), "label": item.label() }))
            .collect(),
    )
    .to_string()
}

/// Write a fresh message's options inside its send transaction.
pub(crate) async fn insert_options(
    conn: &mut SqliteConnection,
    message: &MessageId,
    options: &MessageOptions,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO message_options (message_id, mode, items) VALUES (?, ?, ?)")
        .bind(message.as_str())
        .bind(options.mode().as_str())
        .bind(items_json(options))
        .execute(conn)
        .await?;
    Ok(())
}

/// The options state of each listed message that has options.
pub(crate) async fn load_states(
    conn: &mut SqliteConnection,
    messages: &[MessageId],
) -> Result<HashMap<MessageId, MessageOptionsState>, MessageOptionsStoreError> {
    let mut states = HashMap::new();
    for chunk in messages.chunks(500) {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT * FROM message_options WHERE message_id IN (");
        let mut separated = builder.separated(",");
        for id in chunk {
            separated.push_bind(id.as_str());
        }
        builder.push(")");
        for row in builder.build().fetch_all(&mut *conn).await? {
            let id = MessageId::parse(row.try_get::<String, _>("message_id")?)
                .map_err(|_| MessageOptionsStoreError::Malformed)?;
            states.insert(id, decode_state(&row)?);
        }
    }
    Ok(states)
}

pub(crate) fn decode_state(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<MessageOptionsState, MessageOptionsStoreError> {
    let malformed = || MessageOptionsStoreError::Malformed;
    let mode = OptionMode::parse(&row.try_get::<String, _>("mode")?).ok_or_else(malformed)?;
    let items: Vec<serde_json::Value> =
        serde_json::from_str(&row.try_get::<String, _>("items")?).map_err(|_| malformed())?;
    let items = items
        .into_iter()
        .map(|item| {
            Some((
                item.get("id")?.as_str()?.to_owned(),
                item.get("label")?.as_str()?.to_owned(),
            ))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(malformed)?;
    let options = MessageOptions::parse(mode, items).map_err(|_| malformed())?;
    let answer = match row.try_get::<Option<i64>, _>("answered_at")? {
        None => None,
        Some(answered_at) => {
            let id: String = row.try_get("answer_by_id")?;
            let principal = match row.try_get::<String, _>("answer_by_kind")?.as_str() {
                "user" => PrincipalIdentity::Human(UserId::parse(id).map_err(|_| malformed())?),
                "agent" => PrincipalIdentity::Agent(AgentId::parse(id).map_err(|_| malformed())?),
                _ => return Err(malformed()),
            };
            let option_ids: Vec<String> =
                serde_json::from_str(&row.try_get::<String, _>("answer_option_ids")?)
                    .map_err(|_| malformed())?;
            Some(MessageAnswer::stored(
                Answerer::new(
                    principal,
                    row.try_get("answer_by_name")?,
                    row.try_get("answer_by_display")?,
                ),
                option_ids,
                row.try_get("answer_other")?,
                answered_at,
            ))
        }
    };
    Ok(MessageOptionsState::new(options, answer))
}

struct Question {
    coordinates: QuestionCoordinates,
    author_name: String,
    state: MessageOptionsState,
}

enum Located {
    Found(Box<Question>),
    Refused(AnswerRefusal),
}

async fn locate(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &OptionsActor,
    message: &MessageId,
) -> Result<Located, MessageOptionsStoreError> {
    let hidden = |human: AnswerRefusal| {
        if actor.is_agent() {
            AnswerRefusal::TargetHidden
        } else {
            human
        }
    };
    let Some(row) = sqlx::query(
        "SELECT m.conversation_id, m.thread_root_id, m.from_kind, m.from_agent_id, m.from_user_id, \
         m.author_name, COALESCE(a.name, u.name) AS live_name, o.message_id AS options_id, o.* \
         FROM messages m LEFT JOIN agents a ON a.id = m.from_agent_id \
         LEFT JOIN users u ON u.id = m.from_user_id \
         LEFT JOIN message_options o ON o.message_id = m.id WHERE m.id = ?",
    )
    .bind(message.as_str())
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(Located::Refused(hidden(AnswerRefusal::MessageNotFound)));
    };
    let malformed = || MessageOptionsStoreError::Malformed;
    let conversation = silverhand_backend_core::foundation::identity::ConversationId::parse(
        row.try_get::<String, _>("conversation_id")?,
    )
    .map_err(|_| malformed())?;
    if let OptionsActor::Agent {
        source: Some(source),
        ..
    } = actor
        && source != &conversation
    {
        return Ok(Located::Refused(AnswerRefusal::TargetHidden));
    }
    let Some(facts) = conversation_facts(tx, &conversation).await? else {
        return Ok(Located::Refused(hidden(AnswerRefusal::MessageNotFound)));
    };
    let reaction_actor = match actor {
        OptionsActor::Human(id) => ReactionActor::Human(id.clone()),
        OptionsActor::Agent { id, source } => ReactionActor::Agent {
            id: id.clone(),
            source: source.clone(),
        },
    };
    match reaction_access(tx, &reaction_actor, &conversation, &facts).await? {
        ReactionAccess::Allowed => {}
        ReactionAccess::Forbidden => {
            return Ok(Located::Refused(hidden(AnswerRefusal::Forbidden)));
        }
        ReactionAccess::CapabilityDenied => {
            return Ok(Located::Refused(AnswerRefusal::CapabilityDenied));
        }
    }
    if facts.archived {
        return Ok(Located::Refused(AnswerRefusal::ConversationArchived));
    }
    if row.try_get::<Option<String>, _>("options_id")?.is_none() {
        return Ok(Located::Refused(AnswerRefusal::NoOptions));
    }
    let author = match row.try_get::<String, _>("from_kind")?.as_str() {
        "agent" => PrincipalIdentity::Agent(
            AgentId::parse(
                row.try_get::<Option<String>, _>("from_agent_id")?
                    .unwrap_or_default(),
            )
            .map_err(|_| malformed())?,
        ),
        "user" => PrincipalIdentity::Human(
            UserId::parse(
                row.try_get::<Option<String>, _>("from_user_id")?
                    .unwrap_or_default(),
            )
            .map_err(|_| malformed())?,
        ),
        _ => return Ok(Located::Refused(AnswerRefusal::NoOptions)),
    };
    let author_name = row
        .try_get::<Option<String>, _>("live_name")?
        .or(row.try_get("author_name")?)
        .unwrap_or_default();
    Ok(Located::Found(Box::new(Question {
        coordinates: QuestionCoordinates {
            message: message.clone(),
            conversation,
            thread_root: row
                .try_get::<Option<String>, _>("thread_root_id")?
                .map(MessageId::parse)
                .transpose()
                .map_err(|_| malformed())?,
            author,
        },
        author_name,
        state: decode_state(&row)?,
    })))
}

async fn answerer(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &OptionsActor,
) -> Result<Answerer, MessageOptionsStoreError> {
    let (query, id) = match actor {
        OptionsActor::Human(id) => (
            "SELECT name, display_name FROM users WHERE id = ?",
            id.as_str(),
        ),
        OptionsActor::Agent { id, .. } => (
            "SELECT name, display_name FROM agents WHERE id = ?",
            id.as_str(),
        ),
    };
    let row = sqlx::query(query).bind(id).fetch_one(&mut **tx).await?;
    Ok(Answerer::new(
        actor.principal(),
        row.try_get("name")?,
        row.try_get("display_name")?,
    ))
}

fn new_message_id() -> Result<String, MessageOptionsStoreError> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes).map_err(|_| MessageOptionsStoreError::Entropy)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn principal_columns(principal: &PrincipalIdentity) -> (&'static str, &str) {
    match principal {
        PrincipalIdentity::Human(id) => ("user", id.as_str()),
        PrincipalIdentity::Agent(id) => ("agent", id.as_str()),
    }
}

/// Append a system event in the question's stream: reserve seq, insert, bump head.
async fn append_event(
    tx: &mut Transaction<'_, Sqlite>,
    question: &QuestionCoordinates,
    content: String,
    metadata: String,
    created_at_ms: i64,
) -> Result<AnswerEventMessage, MessageOptionsStoreError> {
    let event_id = new_message_id()?;
    let conversation = &question.conversation;
    let thread_root = question
        .thread_root
        .as_ref()
        .map(|root| {
            silverhand_backend_core::conversation_messaging::messaging::ThreadRootId::parse(
                root.as_str(),
            )
        })
        .transpose()
        .map_err(|_| MessageOptionsStoreError::Malformed)?;
    let seq = reserve_next_seq_on(tx, conversation)
        .await
        .map_err(|fault| match fault {
            ReserveSeqFault::Driver(error) => MessageOptionsStoreError::Sql(error),
            ReserveSeqFault::Ordinal(_) => MessageOptionsStoreError::Ordinal,
        })?;
    sqlx::query(INSERT_MESSAGE)
        .bind(&event_id)
        .bind(conversation.as_str())
        .bind(FROM_KIND_SYSTEM)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind(&content)
        .bind(&metadata)
        .bind(seq.get())
        .bind(created_at_ms)
        .bind(thread_root.as_ref().map(|root| root.as_str()))
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .bind(None::<&str>)
        .execute(&mut **tx)
        .await?;
    bump_stream_head_on(tx, conversation, thread_root.as_ref(), seq).await?;
    Ok(AnswerEventMessage {
        id: MessageId::parse(event_id).map_err(|_| MessageOptionsStoreError::Malformed)?,
        content,
        metadata,
        seq: seq.get(),
        created_at_ms,
    })
}

async fn rollback<T>(
    tx: Transaction<'_, Sqlite>,
    refusal: AnswerRefusal,
) -> Result<Result<T, AnswerRefusal>, MessageOptionsStoreError> {
    tx.rollback().await?;
    Ok(Err(refusal))
}

impl MessageOptionsPort for SqliteMessageOptionsStore {
    type Error = MessageOptionsStoreError;

    async fn answer(
        &self,
        command: AnswerCommand,
    ) -> Result<Result<AnsweredFact, AnswerRefusal>, Self::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let question = match locate(&mut tx, &command.actor, &command.message).await? {
            Located::Found(question) => question,
            Located::Refused(refusal) => return rollback(tx, refusal).await,
        };
        let principal = command.actor.principal();
        let (picked, other) = match decide_answer(
            &question.state,
            Some(&question.coordinates.author),
            &principal,
            command.option_ids,
            command.other,
        ) {
            Ok(decided) => decided,
            Err(refusal) => return rollback(tx, refusal).await,
        };
        let picked_ids: Vec<String> = picked.iter().map(|item| item.id().to_owned()).collect();
        let changed = question.state.answer().is_some();
        let by = answerer(&mut tx, &command.actor).await?;
        let (kind, id) = principal_columns(&principal);
        sqlx::query(
            "UPDATE message_options SET answer_by_kind = ?, answer_by_id = ?, answer_by_name = ?, \
             answer_by_display = ?, answer_option_ids = ?, answer_other = ?, answered_at = ? \
             WHERE message_id = ?",
        )
        .bind(kind)
        .bind(id)
        .bind(by.name())
        .bind(by.display_name())
        .bind(serde_json::Value::from(picked_ids.clone()).to_string())
        .bind(other.as_deref())
        .bind(command.occurred_at_ms)
        .bind(command.message.as_str())
        .execute(&mut *tx)
        .await?;

        let (content, metadata) = answer_event(
            &command.message,
            &NamedPrincipal {
                principal: question.coordinates.author.clone(),
                name: question.author_name.clone(),
            },
            &NamedPrincipal {
                principal: principal.clone(),
                name: by.name().to_owned(),
            },
            &picked,
            other.as_deref(),
            changed,
        );
        let event = append_event(
            &mut tx,
            &question.coordinates,
            content,
            metadata,
            command.occurred_at_ms,
        )
        .await?;
        tx.commit().await?;

        let answer = MessageAnswer::stored(by, picked_ids, other, command.occurred_at_ms);
        Ok(Ok(AnsweredFact {
            state: MessageOptionsState::new(question.state.options().clone(), Some(answer)),
            question: question.coordinates,
            event,
        }))
    }

    async fn withdraw_answer(
        &self,
        command: WithdrawAnswerCommand,
    ) -> Result<Result<AnswerWithdrawnFact, AnswerRefusal>, Self::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let question = match locate(&mut tx, &command.actor, &command.message).await? {
            Located::Found(question) => question,
            Located::Refused(refusal) => return rollback(tx, refusal).await,
        };
        let principal = command.actor.principal();
        if let Err(refusal) = decide_withdraw(&question.state, &principal) {
            return rollback(tx, refusal).await;
        }
        let by = answerer(&mut tx, &command.actor).await?;
        sqlx::query(
            "UPDATE message_options SET answer_by_kind = NULL, answer_by_id = NULL, \
             answer_by_name = NULL, answer_by_display = NULL, answer_option_ids = NULL, \
             answer_other = NULL, answered_at = NULL WHERE message_id = ?",
        )
        .bind(command.message.as_str())
        .execute(&mut *tx)
        .await?;
        let (content, metadata) = withdraw_event(
            &command.message,
            &NamedPrincipal {
                principal: question.coordinates.author.clone(),
                name: question.author_name.clone(),
            },
            &NamedPrincipal {
                principal,
                name: by.name().to_owned(),
            },
        );
        let event = append_event(
            &mut tx,
            &question.coordinates,
            content,
            metadata,
            command.occurred_at_ms,
        )
        .await?;
        tx.commit().await?;
        Ok(Ok(AnswerWithdrawnFact {
            state: MessageOptionsState::new(question.state.options().clone(), None),
            question: question.coordinates,
            event,
        }))
    }

    async fn update_options(
        &self,
        command: UpdateOptionsCommand,
    ) -> Result<Result<OptionsUpdatedFact, AnswerRefusal>, Self::Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let question = match locate(&mut tx, &command.actor, &command.message).await? {
            Located::Found(question) => question,
            Located::Refused(refusal) => return rollback(tx, refusal).await,
        };
        if let Err(refusal) = decide_update(
            &question.state,
            Some(&question.coordinates.author),
            &command.actor.principal(),
        ) {
            return rollback(tx, refusal).await;
        }
        let Ok(options) = command.options else {
            return rollback(tx, AnswerRefusal::InvalidOptions).await;
        };
        sqlx::query("UPDATE message_options SET mode = ?, items = ? WHERE message_id = ?")
            .bind(options.mode().as_str())
            .bind(items_json(&options))
            .bind(command.message.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Ok(OptionsUpdatedFact {
            question: question.coordinates,
            state: MessageOptionsState::new(options, None),
        }))
    }
}
