use crate::*;

pub(super) struct HostRuntimeResources {
    pub(super) host_store: SqliteHostRuntimeStore,
    pub(super) host_agent_store: SqliteHostAgentStore,
    pub(super) computer_agents_read: SqliteComputerAgentsRead,
    pub(super) custody_store: SqliteRuntimeCustodyStore,
    pub(super) delivery_store: SqliteRuntimeDeliveryStore,
    pub(super) host_resources_cold_read: SqliteHostResourcesColdRead,
    pub(super) agent_runtime_state_cold_read: SqliteAgentRuntimeStateColdRead,
}

pub(super) struct MessagingResources {
    pub(super) store: MessagingStore,
    pub(super) task_store: SqliteTaskStore,
    pub(super) task_status_write: SqliteTaskStatusWrite,
    pub(super) task_claim_write: SqliteTaskClaimWrite,
    pub(super) task_edit_write: SqliteTaskEditWrite,
    pub(super) task_create_write: SqliteTaskCreateWrite,
    pub(super) scope_authority: SqliteEventScopeAuthority,
    pub(super) unread_read_model: SqliteUnreadReadModel,
    pub(super) direct_pair_read_model: SqliteDirectPairReadModel,
}

pub(super) struct WorkspaceIdentityResources {
    pub(super) access_store: SqliteAccessStore,
    pub(super) workspace_cold_read: SqliteWorkspaceColdRead,
    pub(super) detail_read: SqliteWorkspaceDetailRead,
    pub(super) metadata_writes: SqliteWorkspaceMetadataWrite,
    pub(super) namespace: SqliteRegularWorkspaceNamespace,
    pub(super) audience_read: SqliteWorkspaceAudienceRead,
}

pub(super) struct AgentIdentityResources {
    pub(super) availability_cold_read: SqliteAgentAvailabilityColdRead,
    pub(super) detail_cold_read: SqliteAgentDetailColdRead,
    pub(super) list_cold_read: SqliteAgentListColdRead,
    pub(super) presence_cold_read: SqliteAgentPresenceColdRead,
    pub(super) profile_write: SqliteAgentProfileWrite,
    pub(super) avatar_read: SqliteAgentAvatarRead,
    pub(super) avatar_design_read: SqliteAvatarDesignRead,
    pub(super) profile_read: SqliteAgentProfileRead,
    pub(super) audit_read: SqliteAgentAuditRead,
}

pub(super) struct HumanIdentityResources {
    pub(super) session_store: SqliteSessionStore,
    pub(super) handle_namespace: SqliteHandleNamespace,
    pub(super) user_store: SqliteUserStore,
    pub(super) registration: SqliteHumanRegistration,
    pub(super) credential_store: SqliteCredentialStore,
}

pub(super) struct PresenceResources {
    pub(super) cold_read: SqliteUserPresenceColdRead,
    pub(super) last_seen_sink: SqliteLastSeenSink,
}

pub(super) struct SchedulingResources {
    pub(super) create: SqliteScheduleCreate,
    pub(super) driver: SqliteScheduleStore,
    pub(super) list_read: SqliteScheduleListRead,
    pub(super) cancel: SqliteScheduleCancel,
}

pub(super) struct WorkspaceDataPlaneResources {
    pub(super) scope_store: SqliteWorkspaceScopeStore,
}

pub(super) struct EmbeddedBootstrapResources {
    pub(super) store: SqliteEmbeddedBootstrap,
}

/// Every resource acquired before the two audit writer tasks start. The outer bundle is
/// an ownership boundary only; its fields remain grouped by their real context owners.
pub(super) struct PassiveResources {
    pub(super) boot: BootProgress,
    pub(super) host_runtime: HostRuntimeResources,
    pub(super) messaging: MessagingResources,
    pub(super) workspace_identity: WorkspaceIdentityResources,
    pub(super) agent_identity: AgentIdentityResources,
    pub(super) human_identity: HumanIdentityResources,
    pub(super) presence: PresenceResources,
    pub(super) scheduling: SchedulingResources,
    pub(super) workspace_data_plane: WorkspaceDataPlaneResources,
    pub(super) embedded_bootstrap: EmbeddedBootstrapResources,
    pub(super) blob_store: LocalBlobStore,
}

impl PassiveResources {
    pub(super) async fn open(config: &AppConfig) -> Result<Self, StartupError> {
        super::startup_protocol::emit_preparation_phase(config.startup_generation(), false)?;
        let preparation =
            silverhand_backend_sqlite::foundation::database_startup::prepare_database(
                &config.database_path,
                config.allow_schema_upgrade,
            )
            .await
            .map_err(StartupError::DatabasePreparation)?;
        super::startup_protocol::emit_preparation_phase(config.startup_generation(), true)?;
        tracing::info!(
            before_step = preparation.before_step,
            after_step = preparation.after_step,
            applied_steps = ?preparation.applied_steps,
            backup_path = ?preparation.backup_path,
            "database startup preparation completed"
        );
        let boot = BootProgress::new();
        boot.mark_migrations_applied();

        let pool = open(
            "shared database pool",
            silverhand_backend_sqlite::foundation::database::open_shared(&config.database_path),
        )
        .await?;

        let host_store = SqliteHostRuntimeStore::from_pool(pool.clone());
        let host_agent_store = SqliteHostAgentStore::from_pool(pool.clone());
        let computer_agents_read = SqliteComputerAgentsRead::from_pool(pool.clone());
        let session_store = SqliteSessionStore::from_pool(pool.clone());
        let messaging_store = MessagingStore::from_pool(pool.clone());
        let task_store = SqliteTaskStore::from_pool(pool.clone());
        let task_status_write = SqliteTaskStatusWrite::from_pool(pool.clone());
        let task_claim_write = SqliteTaskClaimWrite::from_pool(pool.clone());
        let task_edit_write = SqliteTaskEditWrite::from_pool(pool.clone());
        let task_create_write = SqliteTaskCreateWrite::from_pool(pool.clone());
        let custody_store = SqliteRuntimeCustodyStore::from_pool(pool.clone());
        let delivery_store = SqliteRuntimeDeliveryStore::from_pool(pool.clone());
        let scope_authority = SqliteEventScopeAuthority::from_pool(pool.clone());
        let unread_read_model = SqliteUnreadReadModel::from_pool(pool.clone());
        let direct_pair_read_model = SqliteDirectPairReadModel::from_pool(pool.clone());
        let access_store = SqliteAccessStore::from_pool(pool.clone());
        let workspace_cold_read = SqliteWorkspaceColdRead::from_pool(pool.clone());
        let availability_cold_read = SqliteAgentAvailabilityColdRead::from_pool(pool.clone());
        let detail_cold_read = SqliteAgentDetailColdRead::from_pool(pool.clone());
        let list_cold_read = SqliteAgentListColdRead::from_pool(pool.clone());
        let agent_presence_cold_read = SqliteAgentPresenceColdRead::from_pool(pool.clone());
        let profile_write = SqliteAgentProfileWrite::from_pool(pool.clone());
        let avatar_read = SqliteAgentAvatarRead::from_pool(pool.clone());
        let avatar_design_read = SqliteAvatarDesignRead::from_pool(pool.clone());
        let profile_read = SqliteAgentProfileRead::from_pool(pool.clone());
        let presence_cold_read = SqliteUserPresenceColdRead::from_pool(pool.clone());
        let last_seen_sink = SqliteLastSeenSink::from_pool(pool.clone());
        let host_resources_cold_read = SqliteHostResourcesColdRead::from_pool(pool.clone());
        let detail_read = SqliteWorkspaceDetailRead::from_pool(pool.clone());
        let metadata_writes = SqliteWorkspaceMetadataWrite::from_pool(pool.clone());
        let namespace = SqliteRegularWorkspaceNamespace::from_pool(pool.clone());
        let audience_read = SqliteWorkspaceAudienceRead::from_pool(pool.clone());
        let handle_namespace = SqliteHandleNamespace::from_pool(pool.clone());
        let user_store = SqliteUserStore::from_pool(pool.clone());
        let registration = SqliteHumanRegistration::from_pool_with_entropy(
            pool.clone(),
            silverhand_backend_core::host_runtime::environment::OsEntropy,
        );
        let credential_store = SqliteCredentialStore::from_pool(pool.clone());
        let blob_store = open("blob store", LocalBlobStore::open(&config.blobs_root)).await?;
        let agent_runtime_state_cold_read =
            SqliteAgentRuntimeStateColdRead::from_pool(pool.clone());
        let agent_audit_read = SqliteAgentAuditRead::from_pool(pool.clone());
        let schedule_create = SqliteScheduleCreate::from_pool(pool.clone());
        let schedule_driver = SqliteScheduleStore::from_pool(pool.clone());
        let schedule_list_read = SqliteScheduleListRead::from_pool(pool.clone());
        let schedule_cancel = SqliteScheduleCancel::from_pool(
            pool.clone(),
            silverhand_backend_sqlite::conversation_messaging::messaging::schedule::SystemClock,
        );
        let workspace_scope_store = SqliteWorkspaceScopeStore::from_pool(pool.clone());
        let embedded_bootstrap_store = SqliteEmbeddedBootstrap::from_pool_with_entropy(
            pool.clone(),
            silverhand_backend_core::host_runtime::environment::OsEntropy,
        );

        Ok(Self {
            boot,
            host_runtime: HostRuntimeResources {
                host_store,
                host_agent_store,
                computer_agents_read,
                custody_store,
                delivery_store,
                host_resources_cold_read,
                agent_runtime_state_cold_read,
            },
            messaging: MessagingResources {
                store: messaging_store,
                task_store,
                task_status_write,
                task_claim_write,
                task_edit_write,
                task_create_write,
                scope_authority,
                unread_read_model,
                direct_pair_read_model,
            },
            workspace_identity: WorkspaceIdentityResources {
                access_store,
                workspace_cold_read,
                detail_read,
                metadata_writes,
                namespace,
                audience_read,
            },
            agent_identity: AgentIdentityResources {
                availability_cold_read,
                detail_cold_read,
                list_cold_read,
                presence_cold_read: agent_presence_cold_read,
                profile_write,
                avatar_read,
                avatar_design_read,
                profile_read,
                audit_read: agent_audit_read,
            },
            human_identity: HumanIdentityResources {
                session_store,
                handle_namespace,
                user_store,
                registration,
                credential_store,
            },
            presence: PresenceResources {
                cold_read: presence_cold_read,
                last_seen_sink,
            },
            scheduling: SchedulingResources {
                create: schedule_create,
                driver: schedule_driver,
                list_read: schedule_list_read,
                cancel: schedule_cancel,
            },
            workspace_data_plane: WorkspaceDataPlaneResources {
                scope_store: workspace_scope_store,
            },
            embedded_bootstrap: EmbeddedBootstrapResources {
                store: embedded_bootstrap_store,
            },
            blob_store,
        })
    }
}
