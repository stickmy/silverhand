#![forbid(unsafe_code)]
#![deny(rustdoc::broken_intra_doc_links)]
//! The sole executable composition root for the Rust backend.

use std::{
    env,
    error::Error,
    future::{Future, IntoFuture},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::SystemTime,
};

use axum::Router;
use silverhand_backend_core::{
    application_orchestration::teardown::computer_delete::DeleteComputer,
    foundation::blob_store::StorageKind,
    host_runtime::{
        claim::ClaimLocalComputer,
        environment::{OsEntropy, SystemClock},
        invite::CreateHostInvite,
        revoke::RevokeComputer,
        rotation::{RotateConnectToken, RotateHostToken, RotateHostTokenThroughDaemon},
    },
    identity_access::access::{EnrollmentClock, unix_millis},
    identity_access::auth::{
        EntropySessionTokenMint, HumanSessionAuthenticator, SessionTtl, SystemSessionClock,
    },
    identity_access::credential::{Argon2idPasswordHasher, RecoveryCodeKey},
    viewer_read_model::presence_grace::GraceWindow,
};
use silverhand_backend_filesystem::foundation::{
    audit_sink::{AgentAuditFileSink, CliAuditFile},
    blob_store::LocalBlobStore,
};
use silverhand_backend_filesystem::host_runtime::runtime_detection::MachineRuntimeDetector;
#[cfg(test)]
use silverhand_backend_server::foundation::cli_invoke_observer::CLI_INVOKE_PRODUCTION_PERIMETER;
use silverhand_backend_server::{
    application_orchestration::daemon_delivery::{
        ConversationMessageDeliveryAdapter, DaemonDeliveryController, DaemonDeliveryRuntime,
        DaemonHostConnections, OnboardingDeliveryAdapter,
    },
    application_orchestration::{
        agent_create, agent_delete, workspace_rename_fanout::WorkspaceRenameFanoutAdapter,
    },
    conversation_messaging::direct_pairs,
    conversation_messaging::messaging::{
        TargetAddressRefusalLog,
        collaboration::{
            activity,
            agent_channel_list::{AgentListChannelsState, agent_list_channels_router},
            agent_create_channel::{AgentCreateChannelState, agent_create_channel_router},
            agent_read_history::{AgentReadHistoryState, agent_read_history_router},
            agent_search::{AgentSearchState, agent_search_router},
            agent_thread_follow::{AgentThreadFollowState, agent_thread_follow_router},
            attachment::{
                self, FirstSendAttachmentStore, HostAttachmentEnvironment,
                agent_attachment_download_router, agent_attachment_locate_router,
                agent_attachment_update_router, agent_attachment_upload_router,
            },
            attention_preference, attention_projection, channel_directory, channel_lifecycle,
            conversation_agent_activity, conversation_agent_directory, conversation_agent_runtime,
            conversation_latest, conversation_log_debug, conversation_message_window,
            conversation_ref, conversation_threads, conversation_unread, human_search,
            membership as conversation_membership, message_unread, notification, reaction,
            runtime_control as conversation_runtime_control, saved,
            stale_message_resolution::{
                TracingStaleMessageResolutionObserver, router as stale_message_resolution_router,
            },
            thread_follow,
        },
        task::{
            HumanClaimState, HumanCreateState, HumanEditState, HumanStatusState,
            HumanTaskBoardState, HumanTaskPointState, PublishingTaskAftermath, TaskClaimState,
            TaskCreateState, TaskDetailState, TaskEditState, TaskListState, TaskStatusUpdateState,
            TaskUnclaimState, human_claim_router, human_create_router, human_edit_router,
            human_status_router, human_task_board_router, human_task_point_router,
            task_claim_router, task_create_router, task_detail_router, task_edit_router,
            task_list_router, task_unclaim_router, task_update_status_router,
        },
    },
    embedded_bootstrap,
    foundation::{
        bind_address_scope::classify_bind_address_scope,
        cli_invoke_observer::audit_cli_invocations,
        cli_preflight_trace::cli_trace_router,
        clock::SystemProcessClock,
        deployment_profile::DeploymentProfile,
        process_health,
        process_readiness::{self, BootProgress},
        scope_event_transport::{
            AllowedOrigin, EventWebSocketConfig, EventWebSocketController, ScopeEventHub,
        },
        server_info::{self, ServerAdvertisement},
    },
    host_runtime::{
        self, agent_clear_session, agent_presence, agent_runtime_state, agent_runtime_switch,
        agent_stop, availability,
        host_agent_background::{
            agent_deletion_resumer::AgentDeletionResumer, presence_grace_timer::PresenceGraceTimers,
        },
        host_control::management::{self},
        host_control::runtime_availability::DataPlaneRuntimeAvailabilityProbe,
        host_resources, local_runtime_catalog,
        runtime_credential::{RuntimeCredentialAuthority, require_runtime_credential},
    },
    identity_access::{
        access, agent_audit, agent_avatar, agent_avatar_write, agent_detail, agent_list,
        agent_list_workspaces::{AgentListWorkspacesState, agent_list_workspaces_router},
        agent_name_availability, agent_patch,
        auth::{PublicJsonUnauthorized, require_human_session},
        avatar_blob::SingleKindAvatarBlobs,
        avatar_design,
        human_auth::{self, HumanAuthDeps},
        password_hasher::SpawnBlockingHasher,
        peer_profile::{PeerProfilePorts, peer_profile_router},
        public_profile_mcp,
        self_profile::self_profile_router,
        self_profile_mcp::profile_mcp_router,
        self_profile_write::{
            InternalAvatarWriteState, InternalProfileWriteState, internal_avatar_write_router,
            internal_profile_write_router,
        },
        users::{self, UsersDeps},
        workspace_creation::{WorkspaceCreateState, workspace_create_router},
        workspace_deletion::{WorkspaceDeleteState, workspace_delete_router},
        workspace_detail, workspace_metadata_write, workspace_name_availability, workspaces,
    },
    scheduling::{
        schedule_cancel::route::{
            ScheduleCancelState, schedule_cancel_router as internal_schedule_cancel_router,
        },
        schedule_create::route::{ScheduleCreateState, schedule_create_router},
        schedule_list::route::{
            ScheduleListState, schedule_list_router as internal_schedule_list_router,
        },
        schedule_management::{schedule_cancel_router, schedule_list_router},
    },
    viewer_read_model::presence::{
        HubPresencePublisher, SystemDisconnectClock, compose_presence_vertical,
    },
    viewer_read_model::viewer_sort::{
        ChannelStarState, FavoritesState, SortMutationState, favorites_router,
        sort_mutation_router, star_router,
    },
    workspace_data_plane,
};
use silverhand_backend_sqlite::{
    conversation_messaging::direct_pair_read::SqliteDirectPairReadModel,
    conversation_messaging::messaging::{
        HostEnvironment, MessagingStore,
        schedule::{
            ProductionCronEvaluator, SqliteScheduleCancel, SqliteScheduleCreate,
            SqliteScheduleListRead, SqliteScheduleStore,
        },
        task::{
            SqliteTaskClaimWrite, SqliteTaskCreateWrite, SqliteTaskEditWrite,
            SqliteTaskStatusWrite, SqliteTaskStore,
        },
    },
    conversation_messaging::runtime_delivery::SqliteRuntimeDeliveryStore,
    embedded_bootstrap::SqliteEmbeddedBootstrap,
    foundation::{host_agent_store::SqliteHostAgentStore, scope_event::SqliteEventScopeAuthority},
    host_runtime::SqliteHostRuntimeStore,
    host_runtime::agent_availability_read::SqliteAgentAvailabilityColdRead,
    host_runtime::agent_presence_read::SqliteAgentPresenceColdRead,
    host_runtime::agent_runtime_state_read::SqliteAgentRuntimeStateColdRead,
    host_runtime::computer_agents_read::SqliteComputerAgentsRead,
    host_runtime::host_resources_read::SqliteHostResourcesColdRead,
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
    identity_access::access::SqliteAccessStore,
    identity_access::agent_audit_read::SqliteAgentAuditRead,
    identity_access::agent_avatar_read::SqliteAgentAvatarRead,
    identity_access::agent_detail_read::SqliteAgentDetailColdRead,
    identity_access::agent_list_read::SqliteAgentListColdRead,
    identity_access::agent_profile_read::SqliteAgentProfileRead,
    identity_access::agent_profile_write::SqliteAgentProfileWrite,
    identity_access::auth::SqliteSessionStore,
    identity_access::avatar_design::SqliteAvatarDesignRead,
    identity_access::credential::SqliteCredentialStore,
    identity_access::handle_namespace::SqliteHandleNamespace,
    identity_access::human_registration::SqliteHumanRegistration,
    identity_access::user_identity::SqliteUserStore,
    identity_access::workspace_audience_read::SqliteWorkspaceAudienceRead,
    identity_access::workspace_cold_read::SqliteWorkspaceColdRead,
    identity_access::workspace_detail_read::SqliteWorkspaceDetailRead,
    identity_access::workspace_metadata_write::SqliteWorkspaceMetadataWrite,
    identity_access::workspace_name_availability::SqliteRegularWorkspaceNamespace,
    viewer_read_model::unread_read::SqliteUnreadReadModel,
    viewer_read_model::user_presence_cold_read::SqliteUserPresenceColdRead,
    viewer_read_model::user_presence_last_seen::SqliteLastSeenSink,
    workspace_data_plane::SqliteWorkspaceScopeStore,
};
#[cfg(test)]
use sqlx::sqlite::SqlitePoolOptions;
use thiserror::Error;
use tokio::{net::TcpListener, sync::oneshot};

mod foundation;
use foundation::observability;

mod application_orchestration;
pub use application_orchestration::conversation_debug_layer::production_json_layer;
use application_orchestration::create_aftermath::CreateAftermath;

const DATABASE_PATH_ENV: &str = "SILVERHAND_DATABASE_PATH";
/// The local blob-storage root. Required and launcher-provided, exactly like the
/// database path: the shared app-data resolver hands the backend an EXPLICIT blobs
/// directory (`packages/shared/src/app-data-layout.ts`, #task:1598 — the overloaded
/// `silverhandDir` root is retired), separate from the database file, for BOTH the
/// embedded and standalone profiles. The filesystem blob store ensures it exists on
/// open (its own `create_dir_all`, mirroring the product's `ensureReady`).
const BLOBS_ROOT_ENV: &str = "SILVERHAND_BLOBS_ROOT";
const LISTEN_ADDR_ENV: &str = "SILVERHAND_LISTEN_ADDR";
const ALLOWED_ORIGINS_ENV: &str = "SILVERHAND_ALLOWED_ORIGINS";
const DAEMON_NPM_SPEC_ENV: &str = "SILVERHAND_DAEMON_NPM_SPEC";
/// The launcher's packaging choice. Required with NO default: a backend that is not
/// told which profile it is must fail to start rather than guess (see
/// [`DeploymentProfile`]). Binding the Desktop and standalone-server entrypoints to
/// their profiles is a named packaging residual — there is no Rust launcher yet.
const DEPLOYMENT_PROFILE_ENV: &str = "SILVERHAND_DEPLOYMENT_PROFILE";
/// Optional. When set to a truthy value, public signup is closed; otherwise it is
/// open, matching the product default.
const DISABLE_SIGNUP_ENV: &str = "SILVERHAND_DISABLE_SIGNUP";
/// Required at startup. The directory categorized log files (the CLI preflight-failure
/// and agent owner-audit streams among them) are written to. GENERAL, not CLI-specific:
/// every audit surface writes its own `<prefix>-<utc>.log` under the SAME directory. The
/// Rust backend NEVER self-derives this root — no hard-coded `~/.silverhand/logs`; the
/// launcher hands it an explicit path exactly like the database and blobs roots. Unset or
/// blank becomes the typed [`StartupError::AuditLogDirectoryRequired`] mount refusal: once
/// the two producers are mounted there is no legitimate production no-sink state.
const LOG_DIR_ENV: &str = "SILVERHAND_LOG_DIR";
/// Optional. The path the embedded-bootstrap one-shot token file is written to when
/// the process is on the embedded profile bound to a loopback address. This is the
/// convergence-owned launcher contract: the backend NEVER self-derives it (no
/// hard-coded `~/.silverhand/…`) — the launcher hands it an explicit path exactly
/// like the database, blobs, and log roots. Unset (or empty) means no token file is
/// armed; combined with a non-embedded or non-loopback listener the authority stays
/// disabled and configured stale artifacts are cleaned best-effort. Environment blank is
/// normalized to unset by [`AppConfig::from_env`]; direct construction with
/// `AppConfig::new(..., Some(PathBuf::new()))` is invalid.
const BOOTSTRAP_TOKEN_PATH_ENV: &str = "SILVERHAND_BOOTSTRAP_TOKEN_PATH";
/// Optional launcher-minted spawn correlation token (#task:2343). When set, the
/// process echoes it VERBATIM in the single stdout `bound` startup frame so the
/// launcher can confirm the frame came from THIS spawn (not a stale/foreign process).
/// It is opaque to the backend — never parsed, never a secret, never logged as
/// anything but its echo. Unset (or blank) means no startup frame is emitted (a bare
/// `cargo run` / standalone launch has no launcher awaiting a frame).
const STARTUP_GENERATION_ENV: &str = "SILVERHAND_STARTUP_GENERATION";
#[cfg(test)]
const MIGRATION_STEPS: &str = env!("SILVERHAND_MIGRATION_STEPS");

#[derive(Clone, Debug)]
pub struct AppConfig {
    metrics_listen: Option<SocketAddr>,
    console_token_hash: Option<[u8; 32]>,
    database_path: PathBuf,
    allow_schema_upgrade: bool,
    blobs_root: PathBuf,
    listen_addr: SocketAddr,
    allowed_origins: Vec<AllowedOrigin>,
    daemon_npm_spec: Option<String>,
    /// The launcher-chosen packaging profile. Both mode axes and the runtime-detection
    /// capability project from this ONE value — never picked per factory, never
    /// inferred from the listen address or the daemon spec.
    deployment_profile: DeploymentProfile,
    /// Whether public signup is open. Advertised by `/api/server/info`.
    signup_enabled: bool,
    /// The categorized-log root. `None` is representable only so startup can return the
    /// typed [`StartupError::AuditLogDirectoryRequired`] refusal; it is not a legitimate
    /// production state now that both audit producers are mounted. A configured but
    /// unopenable directory also fails startup.
    log_dir: Option<PathBuf>,
    /// The embedded-bootstrap one-shot token file path, or `None` when the launcher
    /// did not supply one. Whether a token is actually armed is a further AND with the
    /// embedded profile and the actual bound listener being loopback, decided only after
    /// `TcpListener::bind` succeeds; this field is only the launcher-provided coordinate.
    bootstrap_token_path: Option<PathBuf>,
    /// The launcher-minted spawn correlation token to echo in the stdout `bound`
    /// startup frame (#task:2343), or `None` when no launcher is awaiting a frame. Opaque
    /// — echoed verbatim, never a secret.
    startup_generation: Option<String>,
    /// SMTP delivery for password recovery; `None` disables recovery.
    mail: Option<foundation::mail::MailConfig>,
}

impl AppConfig {
    // The arity mirrors the launcher-provided config field set (each field is an
    // independent, separately-validated input), not a factorable parameter cluster; a
    // params struct would only restate `AppConfig` itself.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database_path: PathBuf,
        blobs_root: PathBuf,
        listen_addr: SocketAddr,
        allowed_origins: Vec<AllowedOrigin>,
        daemon_npm_spec: Option<String>,
        deployment_profile: DeploymentProfile,
        signup_enabled: bool,
        log_dir: Option<PathBuf>,
        bootstrap_token_path: Option<PathBuf>,
        startup_generation: Option<String>,
    ) -> Result<Self, ConfigError> {
        if database_path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyDatabasePath);
        }
        if blobs_root.as_os_str().is_empty() {
            return Err(ConfigError::EmptyBlobsRoot);
        }
        if allowed_origins.is_empty() {
            return Err(ConfigError::EmptyAllowedOrigins);
        }
        // An explicitly-supplied but empty log directory is a config mistake. `None`
        // remains representable so bound-listener preparation can surface the more
        // specific mounted-producer startup refusal.
        if log_dir
            .as_ref()
            .is_some_and(|dir| dir.as_os_str().is_empty())
        {
            return Err(ConfigError::EmptyLogDir);
        }
        // Same rule as the log dir: `None` (unset) is the legitimate no-token state; an
        // explicitly-supplied empty path is a launcher mistake.
        if bootstrap_token_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(ConfigError::EmptyBootstrapTokenPath);
        }
        let daemon_npm_spec = daemon_npm_spec
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        // Blank is normalized to `None` (no launcher awaiting a frame). The value is
        // otherwise opaque and echoed verbatim; the backend applies no shape rule.
        let startup_generation = startup_generation
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        Ok(Self {
            metrics_listen: None,
            console_token_hash: None,
            database_path,
            allow_schema_upgrade: deployment_profile == DeploymentProfile::Embedded,
            blobs_root,
            listen_addr,
            allowed_origins,
            daemon_npm_spec,
            deployment_profile,
            signup_enabled,
            log_dir,
            bootstrap_token_path,
            startup_generation,
            mail: None,
        })
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        let database_path = required_env(DATABASE_PATH_ENV)?.into();
        let blobs_root = required_env(BLOBS_ROOT_ENV)?.into();
        let listen_addr = required_env(LISTEN_ADDR_ENV)?
            .parse()
            .map_err(ConfigError::InvalidListenAddress)?;
        let allowed_origins = required_env(ALLOWED_ORIGINS_ENV)?
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| AllowedOrigin::parse(value.to_owned()).map_err(ConfigError::InvalidOrigin))
            .collect::<Result<Vec<_>, _>>()?;
        let daemon_npm_spec = env::var(DAEMON_NPM_SPEC_ENV).ok();
        // Required, no default: an unset profile is a launcher that has not said what
        // this process is, and the only safe answer is to refuse to start.
        let deployment_profile = match required_env(DEPLOYMENT_PROFILE_ENV)?.as_str() {
            "embedded" => DeploymentProfile::Embedded,
            "remote-only" => DeploymentProfile::RemoteOnly,
            other => return Err(ConfigError::InvalidDeploymentProfile(other.to_owned())),
        };
        let signup_enabled = parse_signup_enabled(
            deployment_profile,
            env::var(DISABLE_SIGNUP_ENV).ok().as_deref(),
        )?;
        // Unset or blank is retained as `None` so startup can emit the typed
        // `AuditLogDirectoryRequired` mount refusal; a set path is taken verbatim. The
        // backend never fabricates a default root.
        let log_dir = env::var(LOG_DIR_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        // Unset or blank means no bootstrap token file is provisioned; a set path is
        // taken verbatim (the embedded+actual-bound-loopback AND is applied only after
        // binding succeeds).
        let bootstrap_token_path = env::var(BOOTSTRAP_TOKEN_PATH_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        // Opaque launcher spawn-correlation token; blank/unset means no frame is owed.
        let startup_generation = env::var(STARTUP_GENERATION_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty());
        let allow_schema_upgrade = parse_schema_upgrade(
            deployment_profile,
            env::var("SILVERHAND_MIGRATE").ok().as_deref(),
        )?;
        let metrics_listen =
            observability::parse_listen(env::var("SILVERHAND_METRICS_LISTEN").ok().as_deref())?;
        let mail = foundation::mail::MailConfig::from_env().map_err(ConfigError::Mail)?;
        let console_token_hash = foundation::operator_console::parse_token(
            env::var("SILVERHAND_CONSOLE_TOKEN").ok().as_deref(),
            deployment_profile == DeploymentProfile::RemoteOnly,
        )?;
        Self::new(
            database_path,
            blobs_root,
            listen_addr,
            allowed_origins,
            daemon_npm_spec,
            deployment_profile,
            signup_enabled,
            log_dir,
            bootstrap_token_path,
            startup_generation,
        )
        .map(|mut config| {
            config.metrics_listen = metrics_listen;
            config.console_token_hash = console_token_hash;
            config.mail = mail;
            config.with_schema_upgrade(allow_schema_upgrade)
        })
    }

    /// Recovery is offered only by a remote-only server with mail configured.
    pub(crate) fn recovery_mailer(&self) -> Option<foundation::mail::MailConfig> {
        match self.deployment_profile {
            DeploymentProfile::RemoteOnly => self.mail.clone(),
            DeploymentProfile::Embedded => None,
        }
    }

    pub fn with_schema_upgrade(mut self, allow: bool) -> Self {
        self.allow_schema_upgrade = allow;
        self
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// The launcher-minted spawn-correlation token to echo in the stdout `bound` frame,
    /// or `None` when no launcher is awaiting one (#task:2343).
    pub fn startup_generation(&self) -> Option<&str> {
        self.startup_generation.as_deref()
    }

    /// The launcher-provided event-WS allowed origins (composition may augment with the
    /// bound loopback origin for the embedded profile — #task:2343).
    pub fn allowed_origins(&self) -> &[AllowedOrigin] {
        &self.allowed_origins
    }

    /// The launcher-chosen packaging profile.
    pub fn deployment_profile(&self) -> DeploymentProfile {
        self.deployment_profile
    }
}

fn required_env(name: &'static str) -> Result<String, ConfigError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(ConfigError::MissingEnvironment(name))
}

fn parse_schema_upgrade(
    profile: DeploymentProfile,
    raw: Option<&str>,
) -> Result<bool, ConfigError> {
    match raw {
        None | Some("") => Ok(profile == DeploymentProfile::Embedded),
        Some(value) => match value.trim() {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(ConfigError::InvalidMigrationPermission(value.to_owned())),
        },
    }
}

/// Signup admission is parsed PER PROFILE, because the two launchers ship two
/// different contracts for `SILVERHAND_DISABLE_SIGNUP` and a generic truthy helper
/// would invent a silent third one that neither honours.
///
/// - `Embedded`:
///   no trim, case-sensitive, ONLY the exact string `"1"` disables signup. Every other
///   value — unset, `"0"`, `"true"`, `" 1 "`, garbage, empty — enables it, and nothing
///   fails to start.
/// - `RemoteOnly` (`packages/server/src/config.ts`): unset or the exactly-empty string
///   default to enabled; any other value is trimmed and only `"0"` (enable) and `"1"`
///   (disable) are legal — everything else (including whitespace-only, `"true"`,
///   garbage) is a hard startup failure, matching `parseBoolean01`.
fn parse_signup_enabled(
    profile: DeploymentProfile,
    raw: Option<&str>,
) -> Result<bool, ConfigError> {
    match profile {
        DeploymentProfile::Embedded => Ok(raw != Some("1")),
        DeploymentProfile::RemoteOnly => match raw {
            // `resolveValue` short-circuits `undefined`/`''` to the default BEFORE the
            // parser runs; a whitespace-only value is NOT empty and does reach it.
            None | Some("") => Ok(true),
            Some(value) => match value.trim() {
                "0" => Ok(true),
                "1" => Ok(false),
                _ => Err(ConfigError::InvalidDisableSignup(value.to_owned())),
            },
        },
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("SILVERHAND_METRICS_LISTEN must be a loopback IP socket address")]
    InvalidMetricsListen,
    #[error("SILVERHAND_CONSOLE_TOKEN requires the remote-only deployment profile")]
    ConsoleRequiresRemoteOnly,
    #[error("SILVERHAND_CONSOLE_TOKEN must contain 32 to 256 visible ASCII characters")]
    InvalidConsoleToken,
    #[error("required environment variable {0} is missing or empty")]
    MissingEnvironment(&'static str),
    #[error("database path must not be empty")]
    EmptyDatabasePath,
    #[error("blob storage root must not be empty")]
    EmptyBlobsRoot,
    #[error("categorized log directory must not be empty when set")]
    EmptyLogDir,
    #[error("embedded-bootstrap token path must not be empty when set")]
    EmptyBootstrapTokenPath,
    #[error("event websocket origin allowlist must not be empty")]
    EmptyAllowedOrigins,
    #[error("listen address is invalid")]
    InvalidListenAddress(#[source] std::net::AddrParseError),
    #[error("event websocket origin is invalid")]
    InvalidOrigin(
        #[source]
        silverhand_backend_server::foundation::scope_event_transport::InvalidAllowedOrigin,
    ),
    #[error("deployment profile {0:?} is not one of: embedded, remote-only")]
    InvalidDeploymentProfile(String),
    #[error("SILVERHAND_DISABLE_SIGNUP {0:?} must be 0 or 1")]
    InvalidDisableSignup(String),
    #[error("SILVERHAND_MIGRATE {0:?} must be 0 or 1")]
    InvalidMigrationPermission(String),
    #[error(transparent)]
    Mail(foundation::mail::MailConfigError),
}

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("database startup preparation failed")]
    DatabasePreparation(
        #[source] silverhand_backend_sqlite::foundation::database_startup::DatabaseStartupError,
    ),
    #[error(
        "a categorized audit log directory (SILVERHAND_LOG_DIR) is required: the mounted \
         CLI-preflight and agent owner-audit routes are durable audit producers and may not \
         run without a sink"
    )]
    AuditLogDirectoryRequired,
    #[error("failed to open production adapter {name}")]
    Adapter {
        name: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    #[error("failed to construct {name}")]
    Composition {
        name: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    #[error("failed to bind the configured listener")]
    Bind(#[source] std::io::Error),
    #[error("failed to read the bound listener address")]
    BoundListenerAddress(#[source] std::io::Error),
    #[error("failed to emit the startup handoff frame to stdout")]
    StartupFrame(#[source] Box<dyn Error + Send + Sync>),
    #[error("failed to provision the embedded-bootstrap token file")]
    BootstrapToken(#[source] std::io::Error),
    #[error("served backend failed")]
    Serve(#[source] std::io::Error),
    #[error("shutdown signal authority failed")]
    ShutdownSignal(#[source] Box<dyn Error + Send + Sync>),
}

/// The production [`EnrollmentClock`]. Reading the wall clock is a
/// composition-root concern, so the environment read lives here while the
/// signed/saturating `Date.now()`-parity rule stays in the domain
/// ([`unix_millis`]); this adapter is that rule's trivial one-line wrapper.
#[derive(Clone, Copy, Debug, Default)]
struct SystemEnrollmentClock;

impl EnrollmentClock for SystemEnrollmentClock {
    fn now_unix_millis(&self) -> i64 {
        unix_millis(SystemTime::now())
    }
}

/// The one production agent-deletion resumer type, spelled once so `PreparedApp` and
/// `serve` can name the shared worker without repeating its three concrete authorities:
/// the SQLite `deletion_started_at` lifecycle store, the daemon fence binder minted from
/// the one registry, and the production cascade residue. A clone of this shares its
/// `Arc<Mutex<..>>` state; there is never a second worker.
type ProductionAgentDeletionResumer = AgentDeletionResumer<
    SqliteHostAgentStore,
    DaemonHostConnections,
    host_runtime::cascade_effects::ProductionComputerCascadeEffects<
        host_runtime::cascade_effects::HubAgentDeletedAnnouncer,
    >,
>;

pub struct PreparedApp {
    activity_retention: application_orchestration::activity_retention::ActivityRetention,
    pub router: Router,
    /// The live boot facts the `/readyz` router reads. Carried out of bound-listener
    /// finalization so [`serve`] can mark `listen_ready` as a witnessed consequence of
    /// serving, never merely binding/finalizing and never a handler.
    boot: BootProgress,
    event_controller: EventWebSocketController,
    daemon_controller: DaemonDeliveryController,
    presence_timers: PresenceGraceTimers<SqliteLastSeenSink, HubPresencePublisher>,
    /// The single agent-deletion resumer, already `recover`ed at boot. Carried so
    /// shutdown can `close` it FIRST — before any transport closes — so the shutdown
    /// window never keeps scheduling against a daemon that is being torn down.
    deletion_resumer: ProductionAgentDeletionResumer,
    /// #2234 CP6: the single create-aftermath boundary the `POST /api/agents` route hands
    /// its two remote post-commit legs to. Carried so shutdown can `close` it FIRST — while
    /// the daemon transport its legs drive is still alive — cancelling any in-flight leg and
    /// rejecting any dispatched afterward, exactly like the deletion resumer above.
    create_aftermath: CreateAftermath,
    /// #2256/#2261: the ONE composition-owned CLI audit-file lifecycle owner. Its two
    /// narrow facades record preflight failures and authenticated invocations into the
    /// same daily `cli.*.log`; neither facade can open or close the writer. Required
    /// (not optional) because mounted audit producers must
    /// have a durable sink, so bound-listener finalization fails closed without a log
    /// directory rather than degrading to a no-op. Carried so BOTH shutdown paths
    /// flush+close the same instance
    /// idempotently; it holds a background writer task whose queued lines must drain to
    /// disk before the process exits.
    cli_audit_sink: CliAuditFile<SystemProcessClock>,
    /// #2254/#2257: the ONE composition-owned owner-only agent-chat audit sink. Required on
    /// the same footing as [`Self::cli_audit_sink`] — its owner-audit read routes are
    /// mounted, so a durable sink is a startup precondition, never a no-op. Carried so BOTH
    /// shutdown paths flush+close the same instance idempotently. It writes its OWN
    /// `agent-audit.*.log` files through #2256's writer primitive and shares no record with the
    /// CLI sink.
    agent_audit_sink: AgentAuditFileSink<SystemProcessClock>,
    /// #2255/#2257: the process-lifetime embedded-bootstrap one-shot token authority. When
    /// armed (embedded profile + loopback listener + a launcher-provided path), it holds the
    /// SHA-256 of a token whose plaintext was written to a `0600` file before serving; when
    /// disabled it holds nothing. Carried so EVERY path after it can be armed — later
    /// startup rollback, a server-first error, or graceful shutdown — runs the SAME
    /// idempotent `cleanup`, deleting an UNCONSUMED token and disarming so it can never be
    /// spent after. A bind failure occurs before minting and therefore creates no authority.
    embedded_bootstrap_token: embedded_bootstrap::EmbeddedBootstrapTokenAuthority,
    /// #task:2343 — the launcher-minted spawn-correlation token to echo in the single
    /// stdout `bound` startup frame, or `None` when no launcher is awaiting one (a bare
    /// `cargo run` / standalone launch). Carried here (not read from a global) so
    /// [`serve`] emits the frame from the SAME bound listener it serves, once, after
    /// `mark_listen_ready` — never fabricated by a handler, never before a real bind.
    startup_generation: Option<String>,
    /// #2234 CP7a (test-only): a clone of the ONE runtime-credential authority the
    /// `agent_products` perimeter is wrapped with. Carried so a composition test can mint a
    /// real bearer through `dispatch_start` against the SAME in-memory token store the mounted
    /// perimeter authenticates against — the authority is `Arc`-backed, so the clone shares its
    /// process-local token map. This is the only way to drive the internal self-scope routes'
    /// authenticated effect through the real `prepare()` router; never read in production.
    #[cfg(test)]
    test_runtime_credentials: RuntimeCredentialAuthority<SqliteRuntimeCustodyStore>,
}

impl PreparedApp {
    pub async fn close_connections(&self) {
        self.activity_retention.close().await;
        // The resumer stops scheduling FIRST, synchronously, while the daemon controller
        // and the SQLite owner it holds are both still alive — a stopped worker cannot
        // start a new attempt during the transport teardown below.
        self.deletion_resumer.close();
        // #2234 CP6: the create-aftermath boundary closes alongside the resumer — before any
        // transport — so an in-flight onboarding/runtime-start leg is cancelled AND its
        // future awaited-to-drop before the daemon it drives is torn down.
        self.create_aftermath.close().await;
        shut_down_transports(
            &self.event_controller,
            &self.daemon_controller,
            &self.presence_timers,
        )
        .await;
        // #2256: drain and close the audit sink's writer task. Independent of the
        // transports above, so it runs last; idempotent, so the serve-path close below
        // and this method never double-close a shared instance into a hang.
        self.cli_audit_sink.close().await;
        // #2254/#2257: the agent-chat audit sink drains on the same footing — idempotent.
        self.agent_audit_sink.close().await;
        // #2255/#2257: the embedded-bootstrap token is disarmed and its unconsumed file
        // unlinked on this teardown path too; idempotent and ENOENT-tolerant.
        self.embedded_bootstrap_token.cleanup().await;
    }

    /// A clone of the composition-owned embedded-bootstrap token authority, for
    /// composition tests to witness the enable/disable decision (`consume` succeeds only
    /// on an armed authority) and its shutdown cleanup. The authority is `Arc`-backed, so
    /// the clone shares the one in-memory one-shot cell the mounted route consults.
    #[cfg(test)]
    fn embedded_bootstrap_token(&self) -> embedded_bootstrap::EmbeddedBootstrapTokenAuthority {
        self.embedded_bootstrap_token.clone()
    }
}

/// The one shutdown order shared by [`PreparedApp::close_connections`] and
/// [`serve`], so the two can never drift. The event transport closes and the INSTANT
/// it does the presence authority disposes its live grace windows — mirroring the TS
/// `eventWs.close()` → `ctx.presence.dispose()` sequence. A pending grace window is a
/// live intention, never a durable one, so shutdown drops it without emitting an
/// offline.
///
/// The daemon transport is unrelated to user presence, so its close runs CONCURRENTLY
/// with the whole event→dispose chain; it must never sit BETWEEN the event close and
/// the dispose. Sequencing `join!(event.close(), daemon.close())` before a bare
/// dispose only guarantees "after both", which lets the unrelated daemon close delay
/// the dispose — under a config where daemon shutdown outlasts the grace window, a
/// disconnect that already started its window could seal and publish an offline
/// mid-shutdown. The order is a contract, not a coincidence of today's 1.5s < 20s
/// constants.
async fn shut_down_transports(
    event_controller: &EventWebSocketController,
    daemon_controller: &DaemonDeliveryController,
    presence_timers: &PresenceGraceTimers<SqliteLastSeenSink, HubPresencePublisher>,
) {
    tokio::join!(
        async {
            event_controller.close().await;
            presence_timers.dispose();
        },
        daemon_controller.close(),
    );
}

async fn open<T, E>(
    name: &'static str,
    operation: impl Future<Output = Result<T, E>>,
) -> Result<T, StartupError>
where
    E: Error + Send + Sync + 'static,
{
    operation.await.map_err(|source| StartupError::Adapter {
        name,
        source: Box::new(source),
    })
}

/// Write the embedded-bootstrap token plaintext to a fresh file at `path`, created
/// mode-0600 on unix, via a same-directory temp + atomic rename so a reader never
/// observes a partial token and the final file is owner-only from the instant it is
/// linked into place. The parent directory must already exist (the launcher owns it).
/// The raw token flows only through here; it is never logged, and on any error the
/// `io::Error` is returned WITHOUT the token in its message.
async fn write_bootstrap_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let tmp = bootstrap_token_tmp_path(path);
    // Clear a leftover temp from a crashed prior boot so `create_new` below cannot fail
    // spuriously; a missing temp is the normal case.
    match tokio::fs::remove_file(&tmp).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let write_result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp).await?;
        file.write_all(token.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await
    }
    .await;

    if let Err(error) = write_result {
        // A write/sync/rename fault must not leave a plaintext temp token behind. Preserve
        // the original startup error; an unlink fault is separately observable and never
        // includes the raw token.
        if let Err(cleanup_error) = tokio::fs::remove_file(&tmp).await
            && cleanup_error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                error = silverhand_backend_server::foundation::safe_log_field::safe_display(
                    &cleanup_error
                ),
                "embedded bootstrap: failed to remove a partial token temp file"
            );
        }
        return Err(error);
    }
    Ok(())
}

/// The sibling temp path for the atomic token write (`<name>.tmp` in the same directory,
/// so the final rename stays on one filesystem).
fn bootstrap_token_tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Best-effort removal of both stale token artifacts when the authority is NOT armed (not
/// the embedded profile, or a non-loopback listener): the final file and the deterministic
/// same-directory `.tmp` that a crashed write may have left behind. A missing artifact is
/// success; any other unlink fault is observable as a warning and swallowed so cleanup
/// never aborts startup. Neither artifact path nor its raw contents enter the log.
async fn remove_stale_bootstrap_token(path: &Path) {
    for artifact in [path.to_path_buf(), bootstrap_token_tmp_path(path)] {
        match tokio::fs::remove_file(artifact).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    error =
                        silverhand_backend_server::foundation::safe_log_field::safe_display(&error),
                    "embedded bootstrap: failed to clear a stale token artifact"
                );
            }
        }
    }
}

pub async fn run(config: AppConfig) -> Result<(), StartupError> {
    // Reserve the private endpoint before any database migration or app composition.
    let metrics = observability::prepare_metrics(&config).await?;
    let (listener, app) = application_orchestration::bind_and_prepare(&config).await?;
    match metrics {
        None => serve(listener, app, shutdown_signal()).await,
        Some(metrics) => {
            let (stop, stopped) = oneshot::channel();
            let mut exporter = tokio::spawn(metrics.serve_metrics(stopped));
            let mut exporter_finished = false;
            let result = serve(listener, app, async {
                tokio::select! {
                    result = shutdown_signal() => result,
                    result = &mut exporter => {
                        exporter_finished = true;
                        Err(std::io::Error::other(format!("metrics listener terminated: {result:?}")))
                    }
                }
            }).await;
            let _ = stop.send(());
            if !exporter_finished {
                match tokio::time::timeout(std::time::Duration::from_secs(5), &mut exporter).await {
                    Ok(Ok(Ok(()))) => {}
                    Ok(other) if result.is_ok() => {
                        return Err(StartupError::Serve(std::io::Error::other(format!(
                            "metrics shutdown failed: {other:?}"
                        ))));
                    }
                    Err(_) => {
                        exporter.abort();
                        let _ = exporter.await;
                    }
                    _ => {}
                }
            }
            result
        }
    }
}

pub async fn serve<F, E>(
    listener: TcpListener,
    app: PreparedApp,
    shutdown: F,
) -> Result<(), StartupError>
where
    F: Future<Output = Result<(), E>> + Send,
    E: Error + Send + Sync + 'static,
{
    let PreparedApp {
        activity_retention,
        router,
        boot,
        event_controller,
        daemon_controller,
        presence_timers,
        deletion_resumer,
        create_aftermath,
        cli_audit_sink,
        agent_audit_sink,
        embedded_bootstrap_token,
        startup_generation,
        #[cfg(test)]
            test_runtime_credentials: _,
    } = app;
    // The serving boundary, witnessed by the already-bound `TcpListener` this function
    // was handed: reaching here means a listener exists, so `/readyz` may now report
    // `listen_ready`. Marked from the listener rather than by a public `PreparedApp`
    // method, so EVERY caller that serves a bound listener establishes the fact and no
    // caller can fake it without one — a prepared-but-unserved app stays 503.
    boot.mark_listen_ready();
    // #task:2343 — the out-of-process Desktop launcher startup handoff. When a launcher
    // minted a spawn generation, emit EXACTLY ONE `bound` frame on stdout carrying the
    // REAL bound address (read from this very listener, so a `:0` spawn reports its
    // resolved port) and the echoed generation. It is emitted here — after
    // `mark_listen_ready`, before the accept loop — so the launcher's `/readyz` poll is
    // the readiness gate; stdout carries ONLY this protocol (all logs go to stderr).
    //
    // A frame-emission failure (bad `local_addr`, oversize frame, stdout EPIPE) is a
    // startup failure — but it MUST run the SAME cleanup tail as every other termination
    // path (deletion/create aftermath close → transport shutdown → audit drain →
    // bootstrap-token cleanup), never an early `?` return that leaks composition-owned
    // resources and an unconsumed token. So the fallible emission is computed here and,
    // on error, we fall through to the shared cleanup tail with an `Err` outcome instead
    // of serving.
    let frame_result: Result<(), StartupError> = match startup_generation {
        Some(generation) => emit_startup_frame(&listener, generation),
        None => Ok(()),
    };
    if let Err(frame_error) = frame_result {
        // Never served; still close every composition-owned resource + clean the token,
        // exactly like the signal arm, so nothing leaks (frame failure joins the
        // cleanup tail). Transports/aftermath were composed but not driven; closing them
        // is the correct teardown.
        activity_retention.close().await;
        deletion_resumer.close();
        create_aftermath.close().await;
        shut_down_transports(&event_controller, &daemon_controller, &presence_timers).await;
        cli_audit_sink.close().await;
        agent_audit_sink.close().await;
        embedded_bootstrap_token.cleanup().await;
        return Err(frame_error);
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    // #2255/#2257: serve with per-connection `ConnectInfo<SocketAddr>` so the
    // embedded-bootstrap `LoopbackPeer` gate can read the real TCP peer. Every other
    // route ignores the extension; absent connect-info the gate fails closed to 404.
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    })
    .into_future();
    tokio::pin!(server);
    tokio::pin!(shutdown);
    let outcome = tokio::select! {
        result = &mut server => {
            // The server terminated on its own — an error return, or an unexpected
            // completion — WITHOUT our shutdown signal. The audit sink still owns lines
            // accepted before this instant; its drain+close runs in the shared tail
            // below, so this path can never lose them to a bare object drop.
            result.map_err(StartupError::Serve)
        }
        signal = &mut shutdown => {
            let _ = shutdown_tx.send(());
            // #2234 CP3: the resumer stops scheduling FIRST — synchronously, while the
            // daemon controller and its SQLite owner are both still alive — so no attempt
            // can be started during the transport teardown that follows.
            activity_retention.close().await;
        deletion_resumer.close();
            // #2234 CP6: the create-aftermath boundary closes alongside it — same window,
            // ahead of transport teardown — cancelling any in-flight leg and awaiting its
            // future to drop before the daemon it drives is torn down.
            create_aftermath.close().await;
            // The one shared shutdown order (see `shut_down_transports`): event close
            // then an immediate presence dispose, daemon close concurrent but never
            // wedged between the two.
            shut_down_transports(&event_controller, &daemon_controller, &presence_timers).await;
            server
                .as_mut()
                .await
                .map_err(StartupError::Serve)
                .and_then(|()| {
                    signal.map_err(|source| StartupError::ShutdownSignal(Box::new(source)))
                })
        }
    };
    // #2256: the audit sink's drain+close is the LAST step on EVERY termination path —
    // the graceful signal arm and a server-first error return alike — so accepted lines
    // are never lost to a bare object drop, and it runs after the transports the signal
    // arm tore down. Idempotent, so it is a safe no-op if any other entry point already
    // closed the shared instance.
    activity_retention.close().await;
    cli_audit_sink.close().await;
    // #2254/#2257: the agent-chat audit sink drains on the SAME last-step footing —
    // idempotent.
    agent_audit_sink.close().await;
    // #2255/#2257: the embedded-bootstrap token is disarmed and any unconsumed file
    // unlinked on EVERY termination path (graceful signal or server-first error) — the
    // one-shot cleanup is idempotent and safe after a consume, and never re-arms.
    embedded_bootstrap_token.cleanup().await;
    outcome
}

/// Emit the ONE `bound` startup frame for the launcher: read the real bound address from
/// the served listener (so `:0` reports its resolved port), build the versioned/length-
/// bounded frame with the echoed generation, and write it as a single stdout line.
/// Returns the typed startup error on any failure (bad `local_addr`, oversize frame,
/// stdout EPIPE) so the caller runs the shared cleanup tail rather than early-returning.
fn emit_startup_frame(listener: &TcpListener, generation: String) -> Result<(), StartupError> {
    let bound_addr = listener
        .local_addr()
        .map_err(StartupError::BoundListenerAddress)?;
    let frame =
        application_orchestration::startup_protocol::BoundFrame::new(generation, bound_addr);
    let line = frame
        .to_stdout_line()
        .map_err(|error| StartupError::StartupFrame(Box::new(error)))?;
    use std::io::Write as _;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| StartupError::StartupFrame(Box::new(error)))
}

/// The production graceful-shutdown trigger. Resolves on the FIRST of SIGINT (Ctrl-C)
/// or, on Unix, SIGTERM — the signal the Desktop embedded launcher (and any supervisor)
/// sends to ask for an orderly stop (#task:2343). Both must run the `serve()` graceful
/// tail (deletion/create aftermath close → transport shutdown → audit drain → bootstrap
/// token cleanup); a SIGTERM that fell through to default process termination would skip
/// all of it, so it is handled here rather than left to the default disposition.
async fn shutdown_signal() -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = sigterm.recv() => Ok(()),
            _ = sigint.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

pub fn describe(error: &(dyn Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut current = error.source();
    while let Some(source) = current {
        rendered.push_str(": ");
        rendered.push_str(&source.to_string());
        current = source.source();
    }
    rendered
}

#[cfg(test)]
#[path = "../../backend-sqlite/tests/support/current_schema_snapshot.rs"]
mod current_schema;
