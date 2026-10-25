//! One-snapshot admission for the internal stale-message resolution route.
//!
//! The operation does not write the product database. It still consumes the
//! exact post authority used by messaging: selected conversation placement,
//! actual agent participation and the half-dead direct-pair narrowing. The
//! independent `send_messages` capability remains a separate port at the HTTP
//! boundary.
//!
//! This adapter answers the core admission port directly, so the admitted fact
//! it returns is the canonical one every consumer binds — there is no local
//! source type to translate.

use silverhand_backend_core::{
    conversation_messaging::messaging::collaboration::stale_message_resolution::{
        StaleMessageResolutionAdmissionFact, StaleMessageResolutionAdmissionPort,
        StaleMessageResolutionAdmissionRefusal,
    },
    foundation::identity::{AgentId, ConversationId},
    foundation::persistence::PersistenceFailure,
};

use crate::conversation_messaging::messaging::{
    MessagingStore, authorities::ConversationAuthorityReader, driver,
};

#[derive(Clone)]
pub struct SqliteStaleMessageResolutionAdmission {
    pool: crate::foundation::database::SqliteDatabase,
}

impl<E> MessagingStore<E> {
    pub fn stale_message_resolution_admission(&self) -> SqliteStaleMessageResolutionAdmission {
        SqliteStaleMessageResolutionAdmission {
            pool: self.pool.clone(),
        }
    }
}

impl StaleMessageResolutionAdmissionPort for SqliteStaleMessageResolutionAdmission {
    type Error = PersistenceFailure;

    async fn admit(
        &self,
        agent: &AgentId,
        conversation: &ConversationId,
    ) -> Result<
        Result<StaleMessageResolutionAdmissionFact, StaleMessageResolutionAdmissionRefusal>,
        PersistenceFailure,
    > {
        let mut tx = self
            .pool
            .begin_with("BEGIN DEFERRED")
            .await
            .map_err(driver)?;
        let outcome = {
            let mut reads = ConversationAuthorityReader::new(&mut tx);
            let Some(selected) = reads.conversation(conversation).await? else {
                return commit_refusal(tx).await;
            };
            let Some(source_agent) = reads.agent_participation_source(&selected, agent).await?
            else {
                return commit_refusal(tx).await;
            };
            if !reads.accepts_posts(&selected).await? {
                return commit_refusal(tx).await;
            }
            StaleMessageResolutionAdmissionFact::selected(source_agent, selected.id)
        };
        tx.commit().await.map_err(driver)?;
        Ok(Ok(outcome))
    }
}

async fn commit_refusal(
    tx: sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<
    Result<StaleMessageResolutionAdmissionFact, StaleMessageResolutionAdmissionRefusal>,
    PersistenceFailure,
> {
    tx.commit().await.map_err(driver)?;
    Ok(Err(StaleMessageResolutionAdmissionRefusal::Forbidden))
}
