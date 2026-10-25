use super::*;

impl<E: SendClock + SendEntropy + Send + Sync> MessagingStore<E> {
    #[tracing::instrument(skip(self, command), fields(
        target_conversation = command.target_conversation().as_str(),
        content_bytes = command.content().byte_length(),
    ))]
    async fn send_message_transaction(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        // The environment is asked before a transaction exists, so a host that
        // cannot answer costs nothing: there is no writer to reserve and no
        // statement to roll back.
        let created_at_ms = self.environment.now_unix_ms()?;
        let message_id = MessageId::mint(&self.environment)?;

        let mut scope = TxScope {
            tx: self.begin_root_write().await.map_err(driver)?,
            entropy: &self.environment,
            opened_direct_pairs: Vec::new(),
        };
        match scope.write(command, message_id, created_at_ms).await {
            Ok(outcome) => {
                // The outcome only escapes a COMMITTED unit: this is the boundary that matters,
                // not where the value happened to be built.
                scope.tx.commit().await.map_err(driver)?;
                Ok(outcome)
            }
            Err(refusal) => {
                // The refusal is the answer: a rollback statement that itself
                // fails must not replace it.
                drop(scope.tx.rollback().await);
                Err(refusal)
            }
        }
    }
}

impl<E: SendClock + SendEntropy + Send + Sync> MentionedViewerRecord for MessagingStore<E> {
    /// Writes the records this send owes the people it named — deciding, HERE, who
    /// those names belong to and who may still read the conversation.
    ///
    /// One transaction of its own, taken after the send has committed: the entitlement
    /// read and every insert share it, so the rows that exist are the rows the world
    /// looked like when they were written. A person removed before this runs is not a
    /// candidate; a person removed after it commits keeps a record of something they
    /// could read at the time.
    async fn record_mentioned_viewers(
        &self,
        intent: &MentionIntent,
    ) -> Result<Vec<CommittedMentionRow>, EffectFailure> {
        self.record_mentions_transaction(intent)
            .await
            .map_err(EffectFailure::from_source)
    }
}

impl<E: SendClock + SendEntropy> MessagingStore<E> {
    #[tracing::instrument(skip(self, intent), fields(
        source_conversation = intent.source_conversation().as_str(),
        named = intent.handles().len(),
    ))]
    pub(super) async fn record_mentions_transaction(
        &self,
        intent: &MentionIntent,
    ) -> Result<Vec<CommittedMentionRow>, MessagingStoreError> {
        let named = serde_json::to_string(
            &intent
                .handles()
                .iter()
                .map(ParticipantHandle::as_str)
                .collect::<Vec<_>>(),
        )?;
        let payload_of = |mentioned: &str| {
            serde_json::to_string(&serde_json::json!({
                "conversation_title": intent.source_conversation_title(),
                "trigger_name": intent.trigger_name(),
                "mentioned_name": mentioned,
                "mention_excerpt": intent.excerpt(),
            }))
        };

        let mut tx = self.pool.begin_with(ROOT_WRITE_BEGIN).await?;
        let entitled = sqlx::query(SELECT_ENTITLED_VIEWERS)
            .bind(named)
            .bind(intent.author().map(UserId::as_str))
            .bind(intent.source_conversation().as_str())
            .bind(intent.source_workspace().as_str())
            .fetch_all(&mut *tx)
            .await?;

        let mut rows = Vec::with_capacity(entitled.len());
        for row in entitled {
            let viewer: String = row.try_get("id")?;
            let mentioned: String = row.try_get("name")?;
            let id = NotificationId::mint(&self.environment)?;
            let created_at_ms = self.environment.now_unix_ms()?;

            sqlx::query(INSERT_MENTION_SELF)
                .bind(id.as_str())
                .bind(&viewer)
                .bind(intent.source_workspace().as_str())
                .bind(intent.source_conversation().as_str())
                .bind(intent.source_message().as_str())
                .bind(intent.source_workspace().as_str())
                .bind(intent.source_conversation().as_str())
                .bind(intent.source_message().as_str())
                .bind(payload_of(&mentioned)?)
                .bind(created_at_ms)
                .execute(&mut *tx)
                .await?;

            rows.push(CommittedMentionRow::committed(
                id,
                created_at_ms,
                UserId::parse(viewer)
                    .map_err(|_| MessagingStoreError::MalformedIdentity { field: "users.id" })?,
                ParticipantHandle::parse(mentioned).map_err(|_| {
                    MessagingStoreError::MalformedIdentity {
                        field: "users.name",
                    }
                })?,
                intent.clone(),
            ));
        }

        // Nothing is reported until every row of this batch is durable: a partial
        // report would tell some people about a record and leave others' behind.
        tx.commit().await?;
        Ok(rows)
    }
}

impl<E: SendClock + SendEntropy + Send + Sync> SendMessageUnitOfWork for MessagingStore<E> {
    async fn send_message(
        &self,
        command: SendMessage,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        self.send_message_transaction(command).await
    }
}

/// The transaction's own scope. Every fact the write depends on is read through
/// it, so a concurrent change cannot land between a check and the insert.
struct TxScope<'connection, 'entropy, E> {
    tx: Transaction<'connection, Sqlite>,
    entropy: &'entropy E,
    opened_direct_pairs:
        Vec<silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact>,
}

#[derive(Default)]
struct RecordedDirectPairOpens(
    Mutex<Vec<silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact>>,
);

impl RecordedDirectPairOpens {
    fn into_facts(
        self,
    ) -> Vec<silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact> {
        self.0.into_inner().expect("direct-pair recorder poisoned")
    }
}

impl DirectPairOpenedPort for RecordedDirectPairOpens {
    type Error = Infallible;

    async fn direct_pair_opened(
        &self,
        fact: &silverhand_backend_core::conversation_messaging::messaging::DirectPairOpenedFact,
    ) -> Result<(), Self::Error> {
        self.0
            .lock()
            .expect("direct-pair recorder poisoned")
            .push(fact.clone());
        Ok(())
    }
}

struct TransactionDirectPairMint<'scope, 'connection, 'entropy, E> {
    scope: &'scope mut TxScope<'connection, 'entropy, E>,
}

impl<E: SendEntropy + Send + Sync> DirectPairMint for TransactionDirectPairMint<'_, '_, '_, E> {
    type Error = SendMessageError;

    async fn mint_direct_pair(
        &mut self,
        participants: &DirectPairParticipants,
        now_unix_ms: i64,
    ) -> Result<DirectPairMinted, Self::Error> {
        let minted = mint_conversation_id(self.scope.entropy)?;
        mint_direct_pair_on(&mut self.scope.tx, minted, participants, now_unix_ms)
            .await
            .map_err(infrastructure)
    }
}

impl<E: SendEntropy + Send + Sync> TxScope<'_, '_, E> {
    async fn write(
        &mut self,
        command: SendMessage,
        message_id: MessageId,
        created_at_ms: i64,
    ) -> Result<SendMessageOutcome, SendMessageError> {
        // The sender's own binding must still exist. Read for existence only: what
        // the send is authorized against is the target.
        if command.source_conversation() != command.target_conversation()
            && self
                .conversation_facts(command.source_conversation())
                .await?
                .is_none()
        {
            return Err(ConversationRefusal::NotFound.into());
        }

        let facts =
            messaging::assert_post_authority(self, command.target_conversation(), command.actor())
                .await?;

        if let Some(replayed) = self.replay_client_message(&command).await? {
            return Ok(replayed);
        }

        let thread_root = self
            .resolve_thread_root(&facts.id, command.destination())
            .await?;
        let snapshots = self
            .resolve_attachments(&facts.id, command.attachment_refs())
            .await?;
        let freshness_bypassed = self.assert_fresh(&command, &facts.id).await?;

        let seq = self.reserve_next_seq(&facts.id).await?;
        let author = self.author_snapshot(command.actor()).await?;
        let message = self
            .insert_message(
                &command,
                &facts.id,
                message_id,
                created_at_ms,
                thread_root,
                seq,
                author,
                snapshots,
            )
            .await?;
        if let Some(id) = command.client_message_id() {
            sqlx::query(
                "UPDATE messages SET client_message_id=?, client_request_json=? WHERE id=?",
            )
            .bind(id.as_str())
            .bind(client_request_json(&command))
            .bind(message.id.as_str())
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        }
        self.bump_stream_head(&facts.id, message.thread_root_id.as_ref(), seq)
            .await?;

        let thread = match &message.thread_root_id {
            None => None,
            Some(root) => {
                // A cluster's durable consequences belong to this transaction: a
                // follower recorded without its catch-up would leave an agent
                // following a conversation whose beginning it cannot see.
                self.apply_cluster_follow_effects(&facts.id, root, &command, seq, created_at_ms)
                    .await?;
                // The list entry is projected HERE, inside the same transaction that
                // wrote this reply, so the value carried on the fact (and borrowed by
                // publication and the ACK) is the committed one — not something a
                // post-commit reread might find changed or be unable to answer. This
                // reply is itself a non-system participant of the cluster, so the
                // projection always finds it; an absent entry is a broken invariant.
                let list_entry = project_list_entry(&mut self.tx, &facts.id, root)
                    .await?
                    .ok_or_else(|| infrastructure(MessagingStoreError::MissingClusterProjection))?;
                Some(ThreadReplyFact::committed(list_entry))
            }
        };

        let mention_copies = self
            .apply_mention_copies(&command, &facts.id, &message, created_at_ms)
            .await?;
        // Planned last, so the enumeration sees every row this unit wrote — a
        // follower this very reply created is part of who it is for.
        let post_commit = self
            .plan_post_commit(command.source_conversation(), &facts.id, &command, &message)
            .await?;

        // The current message's durable delivery witness rides THIS transaction. The
        // recipients are read back from the plan the aftermath will consume — never
        // recomputed — so the queued set and the dispatched set are the same fact by
        // construction. A recipient owed this message therefore holds a `turn` row
        // before the send is committed: a crash between commit and dispatch cannot
        // lose it, and the daemon's claim finds it rather than the empty `NoPending`
        // it would otherwise swallow. Historical catch-up (`observe`) is queued by the
        // cluster-follow effects above; this writes only the message this send made,
        // and the main-line and reply arms stay separate so each path's witness has
        // its own producer.
        match post_commit.delivery() {
            DeliveryPlan::MainLine(plan) => {
                for recipient in plan.recipients() {
                    self.ensure_delivery(
                        &facts.id,
                        recipient,
                        message.id.as_str(),
                        DELIVERY_KIND_TURN,
                        created_at_ms,
                    )
                    .await?;
                }
            }
            DeliveryPlan::ReplyCluster(audience) => {
                for recipient in audience.recipients() {
                    self.ensure_delivery(
                        &facts.id,
                        recipient,
                        message.id.as_str(),
                        DELIVERY_KIND_TURN,
                        created_at_ms,
                    )
                    .await?;
                }
            }
        }

        // Last, so it sees the copies this send actually made and the roster as of this unit.
        let unresolved = self
            .unresolved_references(&command, &facts.id, &mention_copies)
            .await?;

        // Assembled INSIDE the transaction, like the fact itself; whether it ever reaches a
        // caller is the wrapper's decision, and on a rollback it does not. Whether a local value
        // has been constructed is not an observable property of a transaction.
        Ok(SendMessageOutcome::committed_send(
            MessageCreatedFact::committed(
                message,
                thread,
                post_commit,
                freshness_bypassed,
                mention_copies,
            ),
            unresolved,
        ))
    }

    // Called after the same mutable post-authority gate as a new send, under
    // BEGIN IMMEDIATE. A replay neither reserves a sequence nor constructs effects.
    async fn replay_client_message(
        &mut self,
        command: &SendMessage,
    ) -> Result<Option<SendMessageOutcome>, SendMessageError> {
        let Some(token) = command.client_message_id() else {
            return Ok(None);
        };
        let SendActor::User(user) = command.actor() else {
            return Err(SendMessageError::ClientMessageConflict);
        };
        let row =
            sqlx::query("SELECT * FROM messages WHERE from_user_id=? AND client_message_id=?")
                .bind(user.as_str())
                .bind(token.as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;
        let Some(row) = row else { return Ok(None) };
        let request: Option<String> = row.try_get("client_request_json").map_err(driver)?;
        if request.as_deref() != Some(client_request_json(command).as_str()) {
            return Err(SendMessageError::ClientMessageConflict);
        }
        let metadata: Option<String> = row.try_get("metadata").map_err(driver)?;
        let attachments =
            collaboration::conversation_message_window::decode_attachments(metadata.as_deref())
                .map_err(|e| SendMessageError::Persistence(PersistenceFailure::from_source(e)))?;
        let root: Option<String> = row.try_get("thread_root_id").map_err(driver)?;
        let message = CreatedMessage {
            client_message_id: Some(token.as_str().to_owned()),
            id: MessageId::parse(row.try_get::<String, _>("id").map_err(driver)?)
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
            conversation_id: command.target_conversation().clone(),
            from_kind: FromKind::User,
            from_agent_id: None,
            from_user_id: Some(user.clone()),
            author: self.author_snapshot(command.actor()).await?,
            content: MessageContent::parse(row.try_get::<String, _>("content").map_err(driver)?)
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
            attachments,
            seq: MessageSeq::try_from(row.try_get::<i64, _>("seq").map_err(driver)?)
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
            created_at_ms: row.try_get("created_at").map_err(driver)?,
            thread_root_id: root
                .map(ThreadRootId::parse)
                .transpose()
                .map_err(|_| SendMessageError::ClientMessageConflict)?,
        };
        Ok(Some(
            SendMessageOutcome::committed_send(
                MessageCreatedFact::committed(
                    message,
                    None,
                    PostCommitPlan::committed(
                        None,
                        DeliveryPlan::MainLine(MainDeliveryPlan::committed(Vec::new(), Vec::new())),
                        Vec::new(),
                    ),
                    false,
                    Vec::new(),
                ),
                Vec::new(),
            )
            .as_replay(),
        ))
    }

    /// Which of this send's references reached nobody, decided INSIDE the unit that committed it.
    ///
    /// Asked here rather than after the commit because the answer is only true of the roster this
    /// send saw: a membership change a moment later would make the diagnostic disagree with a
    /// message that is already a fact.
    ///
    /// A reference is REACHED if the principal still participates in the TARGET conversation, or if
    /// this very send copied the mention to that agent. Both halves matter: an agent outside the
    /// target can still be reached through the human author ↔ agent DirectPair when eligible
    /// for CC, and a participant needs no copy.
    ///
    /// The copy outcomes are read for `Copied` ONLY — never for a skip reason. `AlreadyMember` asks
    /// about the SOURCE conversation, so on a cross-post it answers about the wrong roster.
    ///
    /// Unknown and resolvable-but-not-a-participant are ONE answer. The sender learns that the name
    /// reached nobody, never whether it exists — so this cannot become a way to probe the roster.
    async fn unresolved_references(
        &mut self,
        command: &SendMessage,
        target: &ConversationId,
        copies: &[mention_cc::MentionCopyOutcome],
    ) -> Result<Vec<MessageReference>, SendMessageError> {
        // AGENT handles, and the kind is half of the key. One body may carry both
        // `<silverhand://agent/Alice>` and `<silverhand://human/Alice>`; a copy made for the agent
        // says nothing about the human token, and matching on the handle alone would silently
        // report the human one as reached.
        let copied_agents: HashSet<&str> = copies
            .iter()
            .filter_map(|outcome| match outcome {
                mention_cc::MentionCopyOutcome::Copied { handle, .. } => Some(handle.as_str()),
                mention_cc::MentionCopyOutcome::Skipped { .. } => None,
            })
            .collect();

        let mut unresolved = Vec::new();
        for reference in command.addressed().iter() {
            if reference.kind() == PrincipalKind::Agent
                && copied_agents.contains(reference.handle().as_str())
            {
                continue;
            }
            // The reference's own kind decides which namespace answers for the handle: a human
            // reference must never resolve to an agent of the same name.
            let statement = match reference.kind() {
                PrincipalKind::Agent => SELECT_AGENT_ID_BY_NAME,
                PrincipalKind::Human => SELECT_USER_ID_BY_NAME,
            };
            let resolved: Option<String> = sqlx::query_scalar(statement)
                .bind(reference.handle().as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;

            let reached = match resolved {
                None => false,
                Some(id) => match actor_of(reference.kind(), id)? {
                    SendActor::Agent(agent) => self.is_agent_participant(target, &agent).await?,
                    SendActor::User(user) => self.is_user_participant(target, &user).await?,
                },
            };
            if !reached {
                unresolved.push(reference.clone());
            }
        }
        Ok(unresolved)
    }

    /// Resolves the cluster this send belongs to, checking the caller's name for it
    /// here rather than trusting that some earlier layer did.
    async fn resolve_thread_root(
        &mut self,
        conversation: &ConversationId,
        destination: &SendDestination,
    ) -> Result<Option<ThreadRootId>, SendMessageError> {
        match destination {
            SendDestination::MainLine => Ok(None),
            // A named root must be a message of this conversation that starts a
            // cluster. Unknown, foreign, and "is itself a reply" are one answer.
            SendDestination::Thread(root) => {
                let row = self
                    .message_in_conversation(conversation, root.as_str())
                    .await?
                    .ok_or(ThreadRootRefusal)?;
                if row.thread_root_id.is_some() {
                    return Err(ThreadRootRefusal.into());
                }
                Ok(Some(root.clone()))
            }
            // A reply names a message; the cluster is that message's own root, or
            // the message itself when it starts one.
            SendDestination::Reply(target) => {
                let row = self
                    .message_in_conversation(conversation, target.as_str())
                    .await?
                    .ok_or(ReplyTargetRefusal)?;
                let root = row.thread_root_id.unwrap_or(row.id);
                Ok(Some(ThreadRootId::parse(root).map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity {
                        field: "messages.id",
                    })
                })?))
            }
        }
    }

    async fn message_in_conversation(
        &mut self,
        conversation: &ConversationId,
        message_id: &str,
    ) -> Result<Option<MessageRow>, SendMessageError> {
        let row = sqlx::query(SELECT_MESSAGE_IN_CONVERSATION)
            .bind(conversation.as_str())
            .bind(message_id)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        let Some(row) = row else { return Ok(None) };

        Ok(Some(MessageRow {
            id: row.try_get("id").map_err(driver)?,
            thread_root_id: row.try_get("thread_root_id").map_err(driver)?,
            from_kind: row.try_get("from_kind").map_err(driver)?,
            from_agent_id: row.try_get("from_agent_id").map_err(driver)?,
            from_user_id: row.try_get("from_user_id").map_err(driver)?,
        }))
    }

    async fn resolve_attachments(
        &mut self,
        conversation: &ConversationId,
        refs: &[messaging::attachments::AttachmentRef],
    ) -> Result<Vec<AttachmentSnapshot>, SendMessageError> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        // Looked up one id at a time: an `IN` list would have to be assembled as
        // SQL text, and the decision below stops at the first bad reference
        // anyway, so a batched read would buy nothing it can use.
        let mut sources = Vec::with_capacity(refs.len());
        for reference in refs {
            let row = sqlx::query(SELECT_ATTACHMENT_SOURCE)
                .bind(reference.id.as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;
            let Some(row) = row else { continue };

            sources.push(AttachmentSource {
                id: reference.id.clone(),
                channel_id: ConversationId::parse(
                    row.try_get::<String, _>("channel_id").map_err(driver)?,
                )
                .map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity {
                        field: "attachments.channel_id",
                    })
                })?,
                filename: row.try_get("filename").map_err(driver)?,
                size: row.try_get("size").map_err(driver)?,
                content_type: row.try_get("content_type").map_err(driver)?,
            });
        }

        Ok(messaging::attachments::resolve_attachment_snapshots(
            conversation,
            refs,
            &sources,
        )?)
    }

    /// Returns whether the guard was skipped at the sender's request.
    async fn assert_fresh(
        &mut self,
        command: &SendMessage,
        target: &ConversationId,
    ) -> Result<bool, SendMessageError> {
        let agent =
            match messaging::freshness::freshness_gate(command.actor(), command.freshness_bypass())
            {
                FreshnessGate::NotApplicable => return Ok(false),
                FreshnessGate::Bypassed => return Ok(true),
                FreshnessGate::Check(agent) => agent,
            };

        let declared: Option<String> = sqlx::query_scalar(SELECT_AGENT_SESSION_SCOPE)
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        let declared = declared.ok_or_else(|| FreshnessGuardInvariant::AgentAbsent {
            agent: agent.clone(),
        })?;
        let scope = AgentSessionScope::parse(&declared).ok_or_else(|| {
            FreshnessGuardInvariant::UnknownSessionScope {
                agent: agent.clone(),
            }
        })?;

        // The conversation arm keys on the SOURCE conversation: freshness is about
        // what this runtime has not yet consumed, not about where it is writing.
        let undelivered: Option<i64> = match scope {
            AgentSessionScope::Conversation => {
                sqlx::query_scalar(SELECT_UNDELIVERED_IN_CONVERSATION)
                    .bind(command.source_conversation().as_str())
                    .bind(agent.as_str())
                    .fetch_optional(&mut *self.tx)
                    .await
                    .map_err(driver)?
            }
            AgentSessionScope::Agent => sqlx::query_scalar(SELECT_UNDELIVERED_FOR_AGENT)
                .bind(agent.as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?,
        };

        if undelivered.is_some() {
            return Err(messaging::StaleRefusal {
                stale_event_id: StaleEventId::mint(self.entropy)?,
                target_conversation_id: target.clone(),
            }
            .into());
        }
        Ok(false)
    }

    /// Advances the conversation's counter and reads back what it reserved. The
    /// counter is conversation-global: a reply and a main-line message draw from
    /// it alike, which is what makes `(conversation, ordinal)` a total order.
    async fn reserve_next_seq(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<MessageSeq, SendMessageError> {
        reserve_next_seq_on(&mut self.tx, conversation)
            .await
            .map_err(|fault| match fault {
                ReserveSeqFault::Driver(error) => driver(error).into(),
                ReserveSeqFault::Ordinal(error) => {
                    infrastructure(MessagingStoreError::Ordinal(error))
                }
            })
    }

    /// The author the message reports, resolved through the adapter's one author
    /// authority ([`resolve_authors`]) as a ONE-ELEMENT batch — the same authority a
    /// list read uses for a whole page, so a point read invents no second read or
    /// discrimination path of its own. At send the actor is current, so this resolves
    /// to an active author; its snapshot columns are then copied onto the row so the
    /// message keeps naming its author after a rename or a delete.
    async fn author_snapshot(
        &mut self,
        actor: &SendActor,
    ) -> Result<Option<MessageAuthor>, SendMessageError> {
        let evidence = match actor {
            SendActor::Agent(agent) => AuthorEvidence {
                from_kind: FromKind::Agent,
                from_agent_id: Some(agent.as_str().to_owned()),
                from_user_id: None,
                // Not yet deleted: the snapshot columns are what this send is about
                // to write, so resolution reads the live row, not them.
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
            SendActor::User(user) => AuthorEvidence {
                from_kind: FromKind::User,
                from_agent_id: None,
                from_user_id: Some(user.as_str().to_owned()),
                author_id: None,
                author_name: None,
                author_display_name: None,
            },
        };
        let mut resolved = resolve_authors(&mut self.tx, std::slice::from_ref(&evidence))
            .await
            .map_err(SendMessageError::from)?;
        Ok(resolved.pop().flatten())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_message(
        &mut self,
        command: &SendMessage,
        conversation: &ConversationId,
        id: MessageId,
        created_at_ms: i64,
        thread_root: Option<ThreadRootId>,
        seq: MessageSeq,
        author: Option<MessageAuthor>,
        attachments: Vec<AttachmentSnapshot>,
    ) -> Result<CreatedMessage, SendMessageError> {
        let metadata = metadata_json(&attachments)?;
        let (from_kind, agent_id, user_id) = match command.actor() {
            SendActor::Agent(agent) => (FROM_KIND_AGENT, Some(agent.as_str()), None),
            SendActor::User(user) => (FROM_KIND_USER, None, Some(user.as_str())),
        };
        let (author_id, author_name, author_display_name) =
            author_snapshot_columns(author.as_ref());

        sqlx::query(INSERT_MESSAGE)
            .bind(id.as_str())
            .bind(conversation.as_str())
            .bind(from_kind)
            .bind(agent_id)
            .bind(user_id)
            .bind(command.content().as_str())
            .bind(metadata)
            .bind(seq.get())
            .bind(created_at_ms)
            .bind(thread_root.as_ref().map(ThreadRootId::as_str))
            .bind(author_id)
            .bind(author_name)
            .bind(author_display_name)
            .bind(None::<&str>)
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;

        Ok(CreatedMessage {
            client_message_id: command.client_message_id().map(|id| id.as_str().to_owned()),
            id,
            conversation_id: conversation.clone(),
            from_kind: command.actor().from_kind(),
            from_agent_id: match command.actor() {
                SendActor::Agent(agent) => Some(agent.clone()),
                SendActor::User(_) => None,
            },
            from_user_id: match command.actor() {
                SendActor::User(user) => Some(user.clone()),
                SendActor::Agent(_) => None,
            },
            author,
            content: command.content().clone(),
            attachments,
            seq,
            created_at_ms,
            thread_root_id: thread_root,
        })
    }

    /// The head is advanced unconditionally: it records which ordinal a stream last
    /// reached, and is not a token anything competes for.
    async fn bump_stream_head(
        &mut self,
        conversation: &ConversationId,
        thread_root: Option<&ThreadRootId>,
        seq: MessageSeq,
    ) -> Result<(), SendMessageError> {
        bump_stream_head_on(&mut self.tx, conversation, thread_root, seq)
            .await
            .map_err(|error| driver(error).into())
    }

    /// Copies this message to each agent it addressed that cannot read the
    /// conversation it was written in, onto the direct line between them and the
    /// author — minting that line if it does not exist yet.
    ///
    /// The copy, its delivery row and the author's notification commit with the
    /// message that caused them: a copy that could be lost while the mention stands
    /// would leave the author believing they had reached someone they had not.
    ///
    /// Only a person's mention carries this promise. An agent's does not, so its
    /// send addresses no copies at all.
    async fn apply_mention_copies(
        &mut self,
        command: &SendMessage,
        source: &ConversationId,
        message: &CreatedMessage,
        created_at_ms: i64,
    ) -> Result<Vec<MentionCopyOutcome>, SendMessageError> {
        let SendActor::User(author) = command.actor() else {
            return Ok(Vec::new());
        };
        let addressed: Vec<ParticipantHandle> = command
            .addressed()
            .iter()
            .filter(|reference| reference.kind() == PrincipalKind::Agent)
            .map(|reference| reference.handle().clone())
            .collect();
        if addressed.is_empty() {
            return Ok(Vec::new());
        }

        // A copy is named after where it came from, and a direct line has no
        // workspace to be named after.
        let (workspace_id, source_title) = self.conversation_place(source).await?;
        let Some(workspace_id) = workspace_id else {
            return Ok(addressed
                .into_iter()
                .map(|handle| MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::SourceNotWorkspacePlaced,
                    agent: None,
                })
                .collect());
        };
        let workspace_name = self.workspace_name(&workspace_id).await?;

        // No "already copied" arm: the addressed set is deduplicated where it is
        // derived, and one handle names one agent, so a second copy to the same
        // agent has no way to be asked for.
        let mut outcomes = Vec::with_capacity(addressed.len());
        for handle in addressed {
            let Some(agent) = self.agent_by_name(&handle).await? else {
                outcomes.push(MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::UnknownHandle,
                    agent: None,
                });
                continue;
            };
            // Already a participant: it received the original, and a copy would be
            // the same message twice.
            if self.is_agent_participant(source, &agent).await? {
                outcomes.push(MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::AlreadyMember,
                    agent: Some(agent),
                });
                continue;
            }
            // The handle was resolved to an id HERE, inside the unit, and the
            // verdict was computed before it: a name that now points at a different
            // agent than it did then is not granted, and the copy does not happen.
            if !command.direct_lines().contains(&agent) {
                outcomes.push(MentionCopyOutcome::Skipped {
                    handle,
                    reason: MentionCopySkip::NoDirectLine,
                    agent: Some(agent),
                });
                continue;
            }

            let place = MentionCopyPlace {
                workspace_id: &workspace_id,
                workspace_name: &workspace_name,
                source: source.clone(),
                source_title: &source_title,
            };
            let line = self.open_direct_pair(author, &agent, created_at_ms).await?;
            let copy = self
                .write_mention_copy(&line, &handle, &place, message, author, created_at_ms)
                .await?;

            self.ensure_delivery(
                &line,
                &agent,
                copy.id.as_str(),
                DELIVERY_KIND_TURN,
                created_at_ms,
            )
            .await?;
            self.notify_author_of_copy(author, &agent, &place, message, created_at_ms)
                .await?;

            outcomes.push(MentionCopyOutcome::Copied {
                handle,
                agent,
                copy: Box::new(copy),
            });
        }
        Ok(outcomes)
    }

    /// Decides what this send owes the world, from the same snapshot it was written
    /// in.
    ///
    /// Two layers: who was named is a fact of the body and does not depend on the
    /// path, while who receives the message does — a reply reaches its cluster's
    /// followers, a main-line message reaches the conversation's participants.
    async fn plan_post_commit(
        &mut self,
        source: &ConversationId,
        target: &ConversationId,
        command: &SendMessage,
        message: &CreatedMessage,
    ) -> Result<PostCommitPlan, SendMessageError> {
        let mentions = self.mention_intent(target, command, message).await?;
        let opened_direct_pairs = std::mem::take(&mut self.opened_direct_pairs);

        if let Some(root) = &message.thread_root_id {
            let audience = self.cluster_audience(root, command.actor()).await?;
            return Ok(PostCommitPlan::committed(
                mentions,
                DeliveryPlan::ReplyCluster(ReplyAudience::committed(audience)),
                opened_direct_pairs,
            ));
        }

        // Three cases, and they differ in both answers. An agent writing into its own
        // conversation is not delivered its own message, and starts nobody: the
        // runtime that would be started is the one already running to write this. An
        // agent writing into ANOTHER conversation is not the same actor there — that
        // conversation's own session is a recipient, and idle recipients are started.
        // A person's send excludes nobody and starts whoever is idle.
        let writes_elsewhere = source != target;
        let (excluded, starts) = match command.actor() {
            SendActor::Agent(agent) if !writes_elsewhere => {
                (Some(agent.as_str().to_owned()), false)
            }
            SendActor::Agent(_) => (None, true),
            SendActor::User(_) => (None, true),
        };

        let participants: Vec<String> = sqlx::query_scalar(SELECT_PARTICIPANT_AGENTS)
            .bind(target.as_str())
            .fetch_all(&mut *self.tx)
            .await
            .map_err(driver)?;
        let recipients: Vec<AgentId> = participants
            .into_iter()
            .filter(|candidate| excluded.as_deref() != Some(candidate.as_str()))
            .map(|candidate| {
                AgentId::parse(candidate).map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity { field: "agents.id" })
                })
            })
            .collect::<Result<_, _>>()?;
        let start_eligible = if starts {
            recipients.clone()
        } else {
            Vec::new()
        };

        Ok(PostCommitPlan::committed(
            mentions,
            DeliveryPlan::MainLine(MainDeliveryPlan::committed(recipients, start_eligible)),
            opened_direct_pairs,
        ))
    }

    /// What this send says about naming people, if it named anyone inside a workspace.
    ///
    /// Only the facts of the send: the handles the body named, where it happened, who
    /// wrote it, and what it said. Who is entitled to be told is NOT decided here —
    /// that is a fact of the moment the record is written, and deciding it now would
    /// both freeze an entitlement that can be revoked in between and drag its reads
    /// into this transaction, where a failed lookup for a notification would roll back
    /// a message that was otherwise complete.
    ///
    /// Nothing is recorded outside a workspace: a mention record is navigated to by
    /// workspace and conversation, and a direct line has no workspace to reach it from.
    async fn mention_intent(
        &mut self,
        conversation: &ConversationId,
        command: &SendMessage,
        message: &CreatedMessage,
    ) -> Result<Option<MentionIntent>, SendMessageError> {
        let handles: Vec<ParticipantHandle> = command
            .addressed()
            .iter()
            .filter(|reference| reference.kind() == PrincipalKind::Human)
            .map(|reference| reference.handle().clone())
            .collect();
        if handles.is_empty() {
            return Ok(None);
        }

        let (workspace_id, title) = self.conversation_place(conversation).await?;
        let Some(workspace_id) = workspace_id else {
            return Ok(None);
        };
        Ok(Some(MentionIntent::committed(
            handles,
            match command.actor() {
                SendActor::User(author) => Some(author.clone()),
                SendActor::Agent(_) => None,
            },
            trigger_name_of(message),
            workspace_id,
            conversation.clone(),
            title,
            message.id.clone(),
            mention_cc::mention_excerpt(message.content.as_str()),
        )))
    }

    async fn conversation_place(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<(Option<WorkspaceId>, String), SendMessageError> {
        let row = sqlx::query(SELECT_CONVERSATION_PLACE)
            .bind(conversation.as_str())
            .fetch_one(&mut *self.tx)
            .await
            .map_err(driver)?;
        let workspace_id: Option<String> = row.try_get("workspace_id").map_err(driver)?;
        let workspace_id = workspace_id
            .map(WorkspaceId::parse)
            .transpose()
            .map_err(|_| {
                infrastructure(MessagingStoreError::MalformedIdentity {
                    field: "conversations.workspace_id",
                })
            })?;
        Ok((workspace_id, row.try_get("title").map_err(driver)?))
    }

    async fn workspace_name(
        &mut self,
        workspace: &WorkspaceId,
    ) -> Result<String, SendMessageError> {
        let name: Option<String> = sqlx::query_scalar(SELECT_WORKSPACE_NAME)
            .bind(workspace.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        name.ok_or_else(|| {
            infrastructure(MessagingStoreError::MalformedIdentity {
                field: "conversations.workspace_id",
            })
        })
    }

    async fn agent_by_name(
        &mut self,
        handle: &ParticipantHandle,
    ) -> Result<Option<AgentId>, SendMessageError> {
        let found: Option<String> = sqlx::query_scalar(SELECT_AGENT_ID_BY_NAME)
            .bind(handle.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        match found {
            None => Ok(None),
            Some(id) => Ok(Some(AgentId::parse(id).map_err(|_| {
                infrastructure(MessagingStoreError::MalformedIdentity { field: "agents.id" })
            })?)),
        }
    }

    /// Gets or creates the line between two principals. The endpoints are stored in
    /// one canonical order so the pair has a single identity whichever side is
    /// asking; the order is the database's own, which in Rust is just how strings
    /// compare.
    async fn open_direct_pair(
        &mut self,
        author: &UserId,
        agent: &AgentId,
        created_at_ms: i64,
    ) -> Result<ConversationId, SendMessageError> {
        let participants = DirectPairParticipants::between(
            PrincipalIdentity::Human(author.clone()),
            PrincipalIdentity::Agent(agent.clone()),
        )
        .expect("a human and an agent are distinct principals");
        let opened = RecordedDirectPairOpens::default();
        let result = {
            let mut mint = TransactionDirectPairMint { scope: self };
            open_direct_pair(&mut mint, &opened, participants, created_at_ms).await
        };
        match result {
            Ok(conversation) => {
                self.opened_direct_pairs.extend(opened.into_facts());
                Ok(conversation)
            }
            Err(OpenDirectPairError::Mint(error)) => Err(error),
            Err(OpenDirectPairError::Announcement(never)) => match never {},
            Err(OpenDirectPairError::Identity(error)) => {
                Err(PersistenceFailure::from_source(error).into())
            }
        }
    }

    /// Writes the copy into the line: the same three facts a send writes anywhere —
    /// the conversation's ordinal, the message, the stream head.
    async fn write_mention_copy(
        &mut self,
        line: &ConversationId,
        recipient: &ParticipantHandle,
        place: &MentionCopyPlace<'_>,
        message: &CreatedMessage,
        author: &UserId,
        created_at_ms: i64,
    ) -> Result<mention_cc::CommittedMentionCopy, SendMessageError> {
        let seq = self.reserve_next_seq(line).await?;
        let copy = mention_cc::CommittedMentionCopy {
            id: MessageId::mint(self.entropy)?,
            conversation_id: line.clone(),
            from_kind: messaging::FromKind::System,
            content: mention_cc::render_copy_body(&mention_cc::MentionCopySource {
                recipient_handle: recipient,
                source_workspace_name: place.workspace_name,
                source_conversation_title: place.source_title,
                source_content: message.content.as_str(),
            }),
            metadata: mention_cc::MentionCopyMetadata {
                subtype: mention_cc::MENTION_COPY_SUBTYPE,
                source_workspace_id: place.workspace_id.clone(),
                source_workspace_name: place.workspace_name.to_owned(),
                source_conversation_id: place.source.clone(),
                source_conversation_title: place.source_title.to_owned(),
                source_message_id: message.id.clone(),
                source_from_kind: messaging::FromKind::User,
                source_from_user_id: author.clone(),
                source_message_content: message.content.as_str().to_owned(),
            },
            seq,
            created_at_ms,
            cc_source_message_id: message.id.clone(),
        };

        // The row is written FROM the fact, so what was committed and what is
        // reported are the same value rather than two renderings of one intent.
        sqlx::query(INSERT_MENTION_COPY)
            .bind(copy.id.as_str())
            .bind(copy.conversation_id.as_str())
            .bind(from_kind_column(copy.from_kind))
            .bind(&copy.content)
            .bind(mention_copy_metadata(&copy.metadata)?)
            .bind(copy.seq.get())
            .bind(copy.created_at_ms)
            .bind(copy.cc_source_message_id.as_str())
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        self.bump_stream_head(&copy.conversation_id, None, copy.seq)
            .await?;
        Ok(copy)
    }

    async fn notify_author_of_copy(
        &mut self,
        author: &UserId,
        agent: &AgentId,
        place: &MentionCopyPlace<'_>,
        message: &CreatedMessage,
        created_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        let payload = mention_cc::MentionNotificationPayload::of(
            place.workspace_name,
            place.source_title,
            message.content.as_str(),
        );
        let id = MessageId::mint(self.entropy)?;

        sqlx::query(INSERT_MENTION_NOTIFICATION)
            .bind(id.as_str())
            .bind(author.as_str())
            .bind(agent.as_str())
            .bind(place.workspace_id.as_str())
            .bind(place.source.as_str())
            .bind(message.id.as_str())
            .bind(place.workspace_id.as_str())
            .bind(place.source.as_str())
            .bind(message.id.as_str())
            .bind(mention_notification_payload(&payload)?)
            .bind(created_at_ms)
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(())
    }

    /// Records who follows this cluster now, and catches up whoever is new to it.
    ///
    /// Three sources, in this order: the sender, the root's author, and every
    /// principal the reply addressed. A reference widens the cluster's audience but
    /// grants nothing — following is not a way into a conversation — so each
    /// candidate still has to pass participation and liveness.
    async fn apply_cluster_follow_effects(
        &mut self,
        conversation: &ConversationId,
        root: &ThreadRootId,
        command: &SendMessage,
        current_seq: MessageSeq,
        followed_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        // The cluster's anchor is read once: every catch-up needs it, and one
        // transaction is one snapshot.
        let root_row = self
            .message_in_conversation(conversation, root.as_str())
            .await?;

        self.follow_actor(
            conversation,
            root,
            root_row.as_ref(),
            command.actor(),
            ThreadFollowerSource::Reply,
            current_seq,
            followed_at_ms,
        )
        .await?;

        if let Some(row) = &root_row
            && let Some(author) = actor_of_row(row)?
        {
            self.follow_actor(
                conversation,
                root,
                root_row.as_ref(),
                &author,
                ThreadFollowerSource::TargetAuthor,
                current_seq,
                followed_at_ms,
            )
            .await?;
        }

        for reference in command.addressed().iter() {
            // The reference's own kind decides which namespace answers for the
            // handle: a human reference must never resolve to an agent of the same
            // name.
            let statement = match reference.kind() {
                PrincipalKind::Agent => SELECT_AGENT_ID_BY_NAME,
                PrincipalKind::Human => SELECT_USER_ID_BY_NAME,
            };
            let resolved: Option<String> = sqlx::query_scalar(statement)
                .bind(reference.handle().as_str())
                .fetch_optional(&mut *self.tx)
                .await
                .map_err(driver)?;
            let Some(resolved) = resolved else { continue };

            let addressed = actor_of(reference.kind(), resolved)?;
            self.follow_actor(
                conversation,
                root,
                root_row.as_ref(),
                &addressed,
                ThreadFollowerSource::Mention,
                current_seq,
                followed_at_ms,
            )
            .await?;
        }
        Ok(())
    }

    /// Participation and liveness are both required, and they are different
    /// questions: participation survives a departed principal on purpose so a
    /// survivor keeps reading, while a delivery row for a principal that no longer
    /// exists cannot be written at all.
    #[allow(clippy::too_many_arguments)]
    async fn follow_actor(
        &mut self,
        conversation: &ConversationId,
        root: &ThreadRootId,
        root_row: Option<&MessageRow>,
        actor: &SendActor,
        source: ThreadFollowerSource,
        current_seq: MessageSeq,
        followed_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        let (kind, id) = match actor {
            SendActor::Agent(agent) => (PRINCIPAL_KIND_AGENT, agent.as_str()),
            SendActor::User(user) => (PRINCIPAL_KIND_HUMAN, user.as_str()),
        };

        let participates = match actor {
            SendActor::Agent(agent) => self.is_agent_participant(conversation, agent).await?,
            SendActor::User(user) => self.is_user_participant(conversation, user).await?,
        };
        if !participates {
            return Ok(());
        }
        let alive = match actor {
            SendActor::Agent(agent) => self.agent_exists(agent).await?,
            SendActor::User(user) => {
                self.endpoint_principal_exists(&ConversationEndpoint {
                    principal: messaging::PrincipalIdentity::Human(user.clone()),
                    deleted_at_ms: None,
                })
                .await?
            }
        };
        if !alive {
            return Ok(());
        }

        let followed = sqlx::query(FOLLOW_CLUSTER)
            .bind(root.as_str())
            .bind(conversation.as_str())
            .bind(kind)
            .bind(id)
            .bind(followed_at_ms)
            .bind(match source {
                ThreadFollowerSource::Reply => FOLLOWER_SOURCE_REPLY,
                ThreadFollowerSource::TargetAuthor => FOLLOWER_SOURCE_TARGET_AUTHOR,
                ThreadFollowerSource::Mention => FOLLOWER_SOURCE_MENTION,
            })
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?
            .rows_affected();

        // Only a NEW follower is caught up, and only an agent has anywhere to
        // catch up into: a person reads a conversation, an agent is delivered one.
        if followed == 1
            && let SendActor::Agent(agent) = actor
        {
            self.backfill_cluster_history(
                conversation,
                root_row,
                root,
                agent,
                current_seq,
                followed_at_ms,
            )
            .await?;
        }
        Ok(())
    }

    /// Queues the cluster as it stood before this reply: the root, which anchors it,
    /// and its most recent earlier replies. The agent's own messages are skipped
    /// because it has already seen what it said. The current reply is not queued
    /// here; delivering it is a separate act.
    #[allow(clippy::too_many_arguments)]
    async fn backfill_cluster_history(
        &mut self,
        conversation: &ConversationId,
        root_row: Option<&MessageRow>,
        root: &ThreadRootId,
        agent: &AgentId,
        current_seq: MessageSeq,
        queued_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        if let Some(root_row) = root_row
            && !root_row.authored_by(agent)
        {
            self.ensure_delivery(
                conversation,
                agent,
                &root_row.id,
                DELIVERY_KIND_OBSERVE,
                queued_at_ms,
            )
            .await?;
        }

        let history: Vec<String> = sqlx::query_scalar(SELECT_CLUSTER_HISTORY)
            .bind(conversation.as_str())
            .bind(root.as_str())
            .bind(current_seq.get())
            .bind(agent.as_str())
            .bind(THREAD_BACKFILL_RECENT_REPLIES)
            .fetch_all(&mut *self.tx)
            .await
            .map_err(driver)?;
        for id in history {
            self.ensure_delivery(
                conversation,
                agent,
                &id,
                DELIVERY_KIND_OBSERVE,
                queued_at_ms,
            )
            .await?;
        }
        Ok(())
    }

    async fn ensure_delivery(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
        message_id: &str,
        delivery_kind: &'static str,
        queued_at_ms: i64,
    ) -> Result<(), SendMessageError> {
        if !conversation_allows_delivery(&mut self.tx, conversation)
            .await
            .map_err(driver)?
        {
            return Ok(());
        }
        sqlx::query(ENSURE_INBOX_DELIVERY)
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .bind(message_id)
            .bind(delivery_kind)
            .bind(queued_at_ms)
            .execute(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(())
    }

    /// Who this reply is for: the cluster's follower agents that still participate
    /// and still exist, minus the sender — nobody is delivered their own message.
    /// Read here because the answer must include the followers this transaction
    /// just created.
    async fn cluster_audience(
        &mut self,
        root: &ThreadRootId,
        actor: &SendActor,
    ) -> Result<Vec<AgentId>, SendMessageError> {
        let rows: Vec<String> = sqlx::query_scalar(SELECT_CLUSTER_AUDIENCE)
            .bind(root.as_str())
            .fetch_all(&mut *self.tx)
            .await
            .map_err(driver)?;

        let sender = match actor {
            SendActor::Agent(agent) => Some(agent.as_str()),
            SendActor::User(_) => None,
        };
        rows.into_iter()
            .filter(|candidate| sender != Some(candidate.as_str()))
            .map(|candidate| {
                AgentId::parse(candidate).map_err(|_| {
                    infrastructure(MessagingStoreError::MalformedIdentity {
                        field: "thread_followers.principal_id",
                    })
                })
            })
            .collect()
    }
}

/// The principal a stored row names as its author, or nothing when the row was
/// written by the system or its author is no longer named on it.
fn actor_of_row(row: &MessageRow) -> Result<Option<SendActor>, SendMessageError> {
    match row.from_kind.as_str() {
        FROM_KIND_AGENT => match &row.from_agent_id {
            Some(agent) => Ok(Some(actor_of(PrincipalKind::Agent, agent.clone())?)),
            None => Ok(None),
        },
        FROM_KIND_USER => match &row.from_user_id {
            Some(user) => Ok(Some(actor_of(PrincipalKind::Human, user.clone())?)),
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

fn actor_of(kind: PrincipalKind, id: String) -> Result<SendActor, SendMessageError> {
    let malformed = || {
        infrastructure(MessagingStoreError::MalformedIdentity {
            field: "messages author",
        })
    };
    Ok(match kind {
        PrincipalKind::Agent => SendActor::Agent(AgentId::parse(id).map_err(|_| malformed())?),
        PrincipalKind::Human => SendActor::User(UserId::parse(id).map_err(|_| malformed())?),
    })
}

/// Where a copy came from, carried together because every write of a copy needs all
/// of it.
struct MentionCopyPlace<'facts> {
    workspace_id: &'facts WorkspaceId,
    workspace_name: &'facts str,
    source: ConversationId,
    source_title: &'facts str,
}

fn from_kind_column(kind: messaging::FromKind) -> &'static str {
    match kind {
        messaging::FromKind::User => FROM_KIND_USER,
        messaging::FromKind::Agent => FROM_KIND_AGENT,
        messaging::FromKind::System => FROM_KIND_SYSTEM,
    }
}

/// The inverse of [`from_kind_column`], reading a stored provenance back. The column
/// is this store's own closed set, so a spelling it never writes is a broken
/// database rather than a caller's mistake.
pub(super) fn from_kind_of(stored: &str) -> Result<FromKind, PersistenceFailure> {
    match stored {
        FROM_KIND_USER => Ok(FromKind::User),
        FROM_KIND_AGENT => Ok(FromKind::Agent),
        FROM_KIND_SYSTEM => Ok(FromKind::System),
        _ => Err(malformed("messages.from_kind")),
    }
}

/// A stored column that should name something usable names nothing — a broken
/// database the store owns, reported with the column that broke it.
pub(super) fn malformed(field: &'static str) -> PersistenceFailure {
    PersistenceFailure::from_source(MessagingStoreError::MalformedIdentity { field })
}

fn mention_copy_metadata(
    metadata: &mention_cc::MentionCopyMetadata,
) -> Result<String, SendMessageError> {
    serde_json::to_string(&serde_json::json!({
        "subtype": metadata.subtype,
        "source_workspace_id": metadata.source_workspace_id.as_str(),
        "source_workspace_name": metadata.source_workspace_name,
        "source_conversation_id": metadata.source_conversation_id.as_str(),
        "source_conversation_title": metadata.source_conversation_title,
        "source_message_id": metadata.source_message_id.as_str(),
        "source_from_kind": from_kind_column(metadata.source_from_kind),
        "source_from_user_id": metadata.source_from_user_id.as_str(),
        "source_message_content": metadata.source_message_content,
    }))
    .map_err(|error| infrastructure(MessagingStoreError::Serialization(error)))
}

fn mention_notification_payload(
    payload: &mention_cc::MentionNotificationPayload<'_>,
) -> Result<String, SendMessageError> {
    serde_json::to_string(&serde_json::json!({
        "source_workspace_name": payload.source_workspace_name,
        "source_conversation_title": payload.source_conversation_title,
        "mention_excerpt": payload.mention_excerpt,
    }))
    .map_err(|error| infrastructure(MessagingStoreError::Serialization(error)))
}

/// Who named someone, as a reader will see it. The sender's own snapshot, taken
/// inside the transaction: display name if it says anything, then name, then the id —
/// an id is not friendly, but it is never absent. An unresolved author names no one,
/// so it contributes nothing, exactly as an absent author does.
fn trigger_name_of(message: &CreatedMessage) -> String {
    let Some(author) = &message.author else {
        return String::new();
    };
    let (display, name, id): (&str, &str, Option<&str>) = match author {
        MessageAuthor::Active(ActiveAuthor::Agent(agent)) => (
            agent.display_name(),
            agent.name(),
            Some(agent.id().as_str()),
        ),
        MessageAuthor::Active(ActiveAuthor::Human(human)) => (
            human.display_name(),
            human.name(),
            Some(human.id().as_str()),
        ),
        MessageAuthor::Deleted(deleted) => (
            deleted.display_name(),
            deleted.name(),
            Some(deleted.identity().as_str()),
        ),
        MessageAuthor::Unresolved(_) => ("", "", None),
    };
    let display = display.trim();
    if !display.is_empty() {
        return display.to_owned();
    }
    let name = name.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    id.unwrap_or("").to_owned()
}

struct MessageRow {
    id: String,
    thread_root_id: Option<String>,
    from_kind: String,
    from_agent_id: Option<String>,
    from_user_id: Option<String>,
}

impl MessageRow {
    fn authored_by(&self, agent: &AgentId) -> bool {
        self.from_kind == FROM_KIND_AGENT && self.from_agent_id.as_deref() == Some(agent.as_str())
    }
}

impl<E: Send + Sync> EndpointPrincipalReads for TxScope<'_, '_, E> {
    async fn endpoint_allows_direct_messages(
        &mut self,
        endpoint: &ConversationEndpoint,
    ) -> Result<bool, PersistenceFailure> {
        match &endpoint.principal {
            messaging::PrincipalIdentity::Human(_) => Ok(true),
            messaging::PrincipalIdentity::Agent(agent) => {
                let scope: Option<String> = sqlx::query_scalar(SELECT_AGENT_SESSION_SCOPE)
                    .bind(agent.as_str())
                    .fetch_optional(&mut *self.tx)
                    .await
                    .map_err(driver)?;
                Ok(scope.as_deref() == Some("agent"))
            }
        }
    }

    async fn endpoint_principal_exists(
        &mut self,
        endpoint: &ConversationEndpoint,
    ) -> Result<bool, PersistenceFailure> {
        let statement = match &endpoint.principal {
            messaging::PrincipalIdentity::Agent(_) => SELECT_AGENT_EXISTS,
            messaging::PrincipalIdentity::Human(_) => SELECT_USER_EXISTS,
        };
        let found: Option<i64> = sqlx::query_scalar(statement)
            .bind(endpoint.principal.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }
}

impl<E: Send + Sync> PostAuthorityReads for TxScope<'_, '_, E> {
    async fn conversation_facts(
        &mut self,
        conversation: &ConversationId,
    ) -> Result<Option<ConversationFacts>, PersistenceFailure> {
        let row = sqlx::query(SELECT_CONVERSATION_FACTS)
            .bind(conversation.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        let Some(row) = row else { return Ok(None) };

        let closed_at: Option<i64> = row.try_get("closed_at").map_err(driver)?;
        let archived_at: Option<i64> = row.try_get("archived_at").map_err(driver)?;
        let placement: String = row.try_get("placement_kind").map_err(driver)?;

        let direct_pair = if placement == PLACEMENT_DIRECT_PAIR {
            Some(DirectPairEndpoints {
                a: endpoint_from_row(&row, EndpointSlot::A)?,
                b: endpoint_from_row(&row, EndpointSlot::B)?,
            })
        } else {
            None
        };

        Ok(Some(ConversationFacts {
            id: conversation.clone(),
            is_closed: closed_at.is_some(),
            is_archived: archived_at.is_some(),
            direct_pair,
        }))
    }

    async fn agent_exists(&mut self, agent: &AgentId) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = sqlx::query_scalar(SELECT_AGENT_EXISTS)
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }

    async fn is_agent_participant(
        &mut self,
        conversation: &ConversationId,
        agent: &AgentId,
    ) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = sqlx::query_scalar(SELECT_AGENT_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .bind(agent.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }

    async fn is_user_participant(
        &mut self,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<bool, PersistenceFailure> {
        let found: Option<i64> = sqlx::query_scalar(SELECT_USER_PARTICIPATES)
            .bind(conversation.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .bind(user.as_str())
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(driver)?;
        Ok(found.is_some())
    }
}

enum EndpointSlot {
    A,
    B,
}

fn endpoint_from_row(
    row: &sqlx::sqlite::SqliteRow,
    slot: EndpointSlot,
) -> Result<ConversationEndpoint, PersistenceFailure> {
    let malformed = || PersistenceFailure::from_source(MessagingStoreError::MalformedPlacement);
    let (kind_column, id_column, deleted_column) = match slot {
        EndpointSlot::A => ("endpoint_a_kind", "endpoint_a_id", "endpoint_a_deleted_at"),
        EndpointSlot::B => ("endpoint_b_kind", "endpoint_b_id", "endpoint_b_deleted_at"),
    };

    let kind: Option<String> = row.try_get(kind_column).map_err(driver)?;
    let id: Option<String> = row.try_get(id_column).map_err(driver)?;
    let deleted_at_ms: Option<i64> = row.try_get(deleted_column).map_err(driver)?;
    let (kind, id) = kind.zip(id).ok_or_else(malformed)?;

    let principal = match kind.as_str() {
        ENDPOINT_KIND_HUMAN => {
            messaging::PrincipalIdentity::Human(UserId::parse(id).map_err(|_| malformed())?)
        }
        ENDPOINT_KIND_AGENT => {
            messaging::PrincipalIdentity::Agent(AgentId::parse(id).map_err(|_| malformed())?)
        }
        _ => return Err(malformed()),
    };
    Ok(ConversationEndpoint {
        principal,
        deleted_at_ms,
    })
}

/// The durable snapshot shape, owned by the schema that holds it rather than by
/// the application type it is projected from.
#[derive(serde::Serialize)]
struct StoredAttachmentSnapshot<'a> {
    id: &'a str,
    filename: &'a str,
    size: i64,
    content_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

/// Attachments travel in the message's metadata column, so a message with none
/// stores no metadata at all rather than an empty container.
/// The send-time snapshot columns (`author_id`, `author_name`,
/// `author_display_name`) for an author, as the total projection of the value onto
/// what this schema stores. Active and deleted authors carry a named identity; a
/// system send (`None`) and an unresolved author name no one, so all three columns
/// are NULL. The write path only ever feeds an active author, but the projection is
/// defined for every arm so it cannot silently mishandle one.
fn author_snapshot_columns(
    author: Option<&MessageAuthor>,
) -> (Option<&str>, Option<&str>, Option<&str>) {
    match author {
        Some(MessageAuthor::Active(ActiveAuthor::Agent(agent))) => (
            Some(agent.id().as_str()),
            Some(agent.name()),
            Some(agent.display_name()),
        ),
        Some(MessageAuthor::Active(ActiveAuthor::Human(human))) => (
            Some(human.id().as_str()),
            Some(human.name()),
            Some(human.display_name()),
        ),
        Some(MessageAuthor::Deleted(deleted)) => (
            Some(deleted.identity().as_str()),
            Some(deleted.name()),
            Some(deleted.display_name()),
        ),
        Some(MessageAuthor::Unresolved(_)) | None => (None, None, None),
    }
}

fn metadata_json(attachments: &[AttachmentSnapshot]) -> Result<Option<String>, SendMessageError> {
    if attachments.is_empty() {
        return Ok(None);
    }
    let stored: Vec<StoredAttachmentSnapshot<'_>> = attachments
        .iter()
        .map(|snapshot| StoredAttachmentSnapshot {
            id: snapshot.id.as_str(),
            filename: &snapshot.filename,
            size: snapshot.size,
            content_type: &snapshot.content_type,
            label: snapshot.label.as_ref().map(AttachmentLabel::to_string),
        })
        .collect();

    serde_json::to_string(&serde_json::json!({ "attachments": stored }))
        .map(Some)
        .map_err(|error| infrastructure(MessagingStoreError::Serialization(error)))
}

// Canonical admitted request, not mutable resolved mentions or attachment snapshots.
// Preserve attachment order and normalized labels; destinations stay explicit.
fn client_request_json(command: &SendMessage) -> String {
    let destination = match command.destination() {
        SendDestination::MainLine => serde_json::json!(["main"]),
        SendDestination::Reply(id) => serde_json::json!(["reply", id.as_str()]),
        SendDestination::Thread(id) => serde_json::json!(["thread", id.as_str()]),
    };
    serde_json::json!({"conversation": command.target_conversation().as_str(),
        "destination": destination, "content": command.content().as_str(),
        "attachments": command.attachment_refs().iter().map(|a| serde_json::json!([a.id.as_str(), a.label])).collect::<Vec<_>>()}).to_string()
}
