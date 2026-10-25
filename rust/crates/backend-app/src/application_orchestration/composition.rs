use super::resources::*;
use super::{audit::AuditFacades, finalize::FinalAssembly, lane_mounts::*};
use crate::*;

pub(super) async fn compose_after_audit(
    config: &AppConfig,
    bound_listener_addr: SocketAddr,
    resources: PassiveResources,
    audit: AuditFacades,
) -> Result<FinalAssembly, StartupError> {
    let PassiveResources {
        boot,
        host_runtime:
            HostRuntimeResources {
                host_store,
                host_agent_store,
                computer_agents_read,
                custody_store,
                delivery_store,
                host_resources_cold_read,
                agent_runtime_state_cold_read: runtime_state_cold_read,
            },
        messaging:
            MessagingResources {
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
        workspace_identity:
            WorkspaceIdentityResources {
                access_store,
                workspace_cold_read,
                detail_read: workspace_detail_read,
                metadata_writes: workspace_metadata_writes,
                namespace: workspace_namespace,
                audience_read: workspace_audience_read,
            },
        agent_identity:
            AgentIdentityResources {
                availability_cold_read: agent_availability_cold_read,
                detail_cold_read: agent_detail_cold_read,
                list_cold_read: agent_list_cold_read,
                presence_cold_read: agent_presence_cold_read,
                profile_write: agent_profile_write,
                avatar_read: agent_avatar_read,
                avatar_design_read,
                profile_read: agent_profile_read,
                audit_read: agent_audit_read,
            },
        human_identity:
            HumanIdentityResources {
                session_store,
                handle_namespace,
                user_store,
                registration: human_registration,
                credential_store,
            },
        presence:
            PresenceResources {
                cold_read: presence_cold_read,
                last_seen_sink,
            },
        scheduling:
            SchedulingResources {
                create: schedule_create,
                driver: schedule_driver,
                list_read: schedule_list_read,
                cancel: schedule_cancel,
            },
        workspace_data_plane:
            WorkspaceDataPlaneResources {
                scope_store: workspace_scope_store,
            },
        embedded_bootstrap:
            EmbeddedBootstrapResources {
                store: embedded_bootstrap_store,
            },
        blob_store,
    } = resources;

    let server_mode = config.deployment_profile.server_mode();
    let deployment_mode = config.deployment_profile.deployment_mode();

    let notification_store = messaging_store.notifications();
    let reaction_store = messaging_store.reactions();
    let attachment_reads = messaging_store.attachment_reads();
    let attachment_writes = messaging_store.attachment_writes();
    let agent_attachments = messaging_store.agent_attachments();

    let hub = ScopeEventHub::production().map_err(|source| StartupError::Composition {
        name: "scope event hub",
        source: Box::new(source),
    })?;
    // The presence axis is one authority with two faces, built through the ONE
    // `compose_presence_vertical` factory so production and the composed E2E share
    // the identical construction. The `PresenceGraceTimers` inside is the
    // `OnlineViewers` the presence GET route reads AND, wrapped in a
    // `PresenceConnectionFeed`, the transport's connect/disconnect sink; a decided
    // offline fact projects once through the `HubPresencePublisher` into
    // `EventScope::App`; it is never re-injected downstream. Production seals the
    // disconnect instant with the system wall clock and takes the 20s grace TS
    // parity fixes (`index.ts` passes no override).
    let (presence_router, presence_feed, presence_timers) = compose_presence_vertical(
        presence_cold_read,
        last_seen_sink,
        hub.presence_publisher(),
        SystemDisconnectClock,
        GraceWindow::from_millis_or_default(None),
    );
    // #2223 CP3: the human-auth issue/revoke writes ride the SAME session store the
    // read authenticator holds — cloned as shared-pool handles (the store is an `Arc`
    // pool), captured before `HumanSessionAuthenticator::new` moves the original just
    // below. Not a second authority: the same `auth_sessions` table backs the issue
    // that login/signup performs, the revoke that logout performs, and the read the
    // `/api/me` perimeter performs.
    let session_issue = session_store.clone();
    let session_revoke = session_store.clone();
    // #2255/#2257: the embedded-bootstrap grant issues a human session through the SAME
    // `auth_sessions` store — cloned here before the authenticator moves the original.
    let embedded_bootstrap_sessions = session_store.clone();
    let sessions =
        HumanSessionAuthenticator::new(session_store, SystemSessionClock, SessionTtl::default());
    let membership_runtime_seats = delivery_store.clone();
    // Phase B (#2341): a second shared-pool seat handle for the workspace-delete
    // aftermath's runtime stop — the SAME `RuntimeControlSeatPort` the membership
    // runtime stop reads, cloned before `daemon_runtime` moves the original.
    let workspace_delete_runtime_seats = delivery_store.clone();
    let runtime_credentials = RuntimeCredentialAuthority::production(custody_store);
    let daemon_runtime = DaemonDeliveryRuntime::production(
        runtime_credentials.clone(),
        delivery_store,
        host_store.clone(),
        hub.runtime_activity_publisher(),
        hub.agent_presence_publisher(),
        hub.runtime_status_publisher(),
    )
    .map_err(|source| StartupError::Composition {
        name: "daemon delivery runtime",
        source: Box::new(source),
    })?;
    // The read-only control handles now; the daemon websocket router is built later, once
    // the deletion resumer exists to be its mandatory host-ready observer (both descend
    // from this runtime's one registry — the binder and the observer are one truth).
    let daemon_controller = daemon_runtime.controller();

    let target_address =
        messaging_store.target_address(hub.messaging_events(), TargetAddressRefusalLog);
    // Internal attachment upload resolves its canonical target through the SAME address
    // authority the send door uses. `SqliteTargetAddress` is a shared-pool clone and keeps
    // the same direct-pair-open hub + refusal observer; no parallel resolver is constructed.
    let agent_attachment_target = target_address.clone();
    // The task board list reads its board/capability/participation authority from the SAME
    // `MessagingStore` the send door holds; captured before `send_routers` consumes it as the
    // send unit-of-work.
    let task_list_boards = messaging_store.clone();
    let task_list_targets = target_address.clone();
    // The stale-message resolution route rides the SAME post authority (agent
    // participation + placement + half-dead narrowing) and the independent
    // `send_messages` capability the send door holds; both captured before
    // `send_routers` consumes the store as the send unit-of-work.
    let stale_admission = messaging_store.stale_message_resolution_admission();
    let stale_capability = messaging_store.clone();
    // The five channel directory/detail read doors ride the SAME `MessagingStore` the
    // send door holds — each `&self` builder clones only the shared pool, opening no
    // second authority. Captured before `send_routers` consumes the store as the send
    // unit-of-work. One handle satisfies the directory factory's three-port bound.
    let channel_directory_read = messaging_store.channel_directory();
    let channel_detail_read = messaging_store.channel_lifecycle();
    let conversation_agent_directory_read = messaging_store.conversation_agent_directory();
    // Wave 3A checkpoint A2: the four runtime/activity projection doors ride the SAME
    // `MessagingStore` (each `&self` builder clones only the shared pool, no second
    // authority) and — for the live run-state map — the SAME `daemon_runtime` owner
    // already composed for delivery, cloned at the merge (a clone shares the owner's
    // `Arc<Mutex<..>>` state; it is never a second runtime snapshot). The run-states
    // admission takes its own `channel_lifecycle()` handle because the A1 detail handle
    // above is moved into its own router.
    let conversation_agent_runtime_read = messaging_store.conversation_agent_runtime();
    let conversation_agent_activity_read = messaging_store.conversation_agent_activity();
    let activity_retention_store = messaging_store.clone();
    let activity_retention_hub = hub.clone();
    let activity_read_model = messaging_store.activity_read_model();
    let agent_run_states_admission = messaging_store.channel_lifecycle();
    // #2234 CP4: the two runtime write faces' messaging-store dependencies, captured (like
    // the read doors below) before `send_routers` moves the store — each `&self` builder
    // clones only the shared pool, never a second authority. clear-session's seat admission
    // ladder and runtime-switch's conversation-coordination read both ride the ONE
    // `MessagingStore`.
    let agent_clear_session_admission = messaging_store.runtime_control_admission();
    let agent_switch_coordination = messaging_store.switch_conversation_coordination();
    // Wave 3A checkpoint A3: the five message navigation/search read doors ride the SAME
    // `MessagingStore` (each `&self` builder clones only the shared pool, no second
    // authority), captured before `send_routers` moves the store as the send UoW. The
    // message-window GET shares its path with the send POST — see the merge below.
    let conversation_ref_read = messaging_store.conversation_refs();
    let conversation_unread_read = messaging_store.conversation_unread();
    let conversation_latest_read = messaging_store.conversation_latest();
    let conversation_threads_read = messaging_store.conversation_threads();
    let conversation_message_window_read = messaging_store.conversation_message_window();
    let human_search_read = messaging_store.human_search();
    // Wave 3A checkpoint A4: the five unread/attention read doors ride the SAME
    // `MessagingStore` (each `&self` builder clones only the shared pool, no second
    // authority), captured before `send_routers` moves the store as the send UoW. One
    // `message_unread()` handle satisfies the three-port unread bound (workspace roll-up,
    // viewer summary, per-conversation).
    let message_unread_read = messaging_store.message_unread();
    let attention_projection_read = messaging_store.attention_projection();
    let lane_attention_projection = attention_projection_read.clone();
    // The two viewer-owned message-state lanes ride the SAME `MessagingStore`
    // (each `&self` builder clones only the shared pool, no second authority), captured
    // before `write_doors` moves the store below. `saved` serves both the read and the
    // mutation port; `thread_follow` is the follow/unfollow write door.
    let saved_store = messaging_store.saved();
    let thread_follow_store = messaging_store.thread_follow();
    // The workspace-rename fan-out's three read/author ports (channel
    // population, per-channel system-message author, per-channel agent audience) are
    // answered by the SAME `MessagingStore` — one `&self` builder handle, captured
    // before `write_doors` moves the store below. The adapter that wraps this with the
    // daemon delivery effect and the hub publication is assembled just before the merge.
    let workspace_rename_messaging = messaging_store.workspace_rename_messaging();
    let lane_agent_audience = workspace_rename_messaging.clone();
    // Channel-fork aftermath re-reads the LIVE conversation roster at effect time
    // via the same `ConversationAgentsReadPort` the rename lane uses.
    let channel_fork_agent_audience = workspace_rename_messaging.clone();
    // `/readyz`'s live write probe rides the SAME `MessagingStore` pool handle the send
    // door holds — it does NOT reopen SQLite by path. Captured before `write_doors`
    // moves the store below. The probe is the one thing readiness cannot answer from
    // cached boot state: a fully booted process can still have lost its database.
    let readiness_write_probe = messaging_store.write_probe();
    // The create route's onboarding leg is a two-step post-commit effect
    // (durable inbox-ensure THEN daemon wake). Its ensure half is the SAME `MessagingStore`
    // pool, in a narrow role that composes three EXISTING statements — message-in-conversation
    // read, canonical participation read, idempotent delivery insert — in one transaction, so
    // it never parallel-copies the send fan-out's SQL. Captured here as a shared-pool handle
    // before `write_doors` moves the store below.
    let agent_create_onboarding_ensure = messaging_store.onboarding_inbox_ensure();
    let lane_committed_message_ensure = messaging_store.committed_message_inbox_ensure();
    let lane_observation = messaging_store.observation();
    let lane_attention_preferences = messaging_store.attention_preferences();
    let lane_channel_create = messaging_store.channel_directory_create();
    let lane_channel_mutation = messaging_store.channel_lifecycle_mutation();
    let lane_log_debug_admission = LogDebugAdmission(messaging_store.conversation_participation());
    let lane_task_boards = TaskBoards(messaging_store.clone());
    let lane_task_door = messaging_store.clone();
    let lane_human_post = HumanPost(messaging_store.clone());
    let lane_conversation_membership = messaging_store.clone();
    let lane_target_address = target_address.clone();
    let lane_task_target =
        CanonicalTaskTarget::new(target_address.clone(), lane_task_boards.clone());
    let lane_schedule_target = CanonicalScheduleTarget(target_address.clone());
    let lane_schedule_capability = ScheduleCapability(messaging_store.clone());
    let lane_unread = unread_read_model.clone();
    let lane_schedule_unread = unread_read_model.clone();
    let lane_schedule_wake_write = messaging_store.schedule_wake_write();
    // The thread-follow post-commit unread axis binds the SAME two authorities the
    // messaging aftermath's unread publisher binds — this `hub` and this `unread_read_model`
    // (cloned as a shared-pool handle, no second audience authority) — before the messaging
    // aftermath moves the original just below. It is a separate publisher instance over the
    // same authorities, not a second unread path. This factory does not touch
    // `messaging_aftermath`'s signature — the two events thread through separate hub
    // factories by design (#2205 seam left untouched).
    let thread_follow_aftermath = hub.thread_follow_aftermath(unread_read_model.clone());
    // The one post-commit fan-out, built by the hub over itself: the publication authority
    // and the hub the unread axis routes through are both `hub`, and this statement has no
    // hub argument that could name a second transport.
    let aftermath = hub.messaging_aftermath(
        daemon_runtime.effects(),
        messaging_store.clone(),
        unread_read_model,
    );
    let lane_delivery = ConversationMessageDeliveryAdapter::new(
        lane_committed_message_ensure,
        daemon_runtime.effects(),
    );
    let lane_effects = LaneEffects::new(
        hub.clone(),
        lane_unread,
        lane_attention_projection,
        lane_agent_audience,
        lane_delivery.clone(),
        ProductionMembershipRuntimeStop::new(
            membership_runtime_seats,
            daemon_runtime.scope_runtime_stop(),
        ),
    );
    let schedule_driver_handle = ScheduleDriverHandle::start(
        schedule_driver,
        ProductionScheduleWake::new(
            lane_schedule_wake_write,
            hub.clone(),
            lane_schedule_unread,
            lane_delivery,
        ),
    )
    .await
    .map_err(|source| StartupError::Composition {
        name: "schedule fire driver",
        source: Box::new(source),
    })?;
    // Every authenticated write door — the two
    // ordinary sends and the human direct-pair initiation — rides the SAME
    // `MessagingStore`, in every write role it needs: channel access, send capability,
    // direct-line authority, send unit-of-work, and the get-or-create initiation source.
    // One statement, one store, and no argument for the transport the winner-only
    // pair-opened fact leaves on: the seam binds the initiation port to the one aftermath
    // above, which is itself that transport. So this mount cannot announce a pair-open on
    // one hub while the pair's first message publishes on another — there is no parameter
    // to say it in.
    // The caller-side truth the internal peer-profile read consults, captured
    // before `write_doors` below moves `messaging_store`. It is the SAME ONE `MessagingStore`
    // the send/task-board gates hold — a clone shares its pool, and this ONE value answers all
    // three caller ports (`CallerConversationPlacement` / `CallerWorkspaceHorizon` /
    // `CallerReadMessagesCapability`), so the peer route reads the placement floor, the
    // cross-workspace horizon, and the `read_messages` capability through the same authority,
    // never a second store minted for the internal family.
    let peer_profile_caller = messaging_store.clone();
    // The six viewer-sort / favorite faces ride the SAME
    // `MessagingStore` — each `&self` builder clones only the shared pool, never a
    // second authority. Captured before `write_doors` moves the store below. The
    // star router's channel-home read + the ONE transactional sort-mutation writer
    // (which re-resolves list membership/visibility/star-exclusion UNDER its own
    // write lock, writes, and returns the committed post-image), the favorites read
    // model. No pre-lock authority witness escapes into a route.
    let sort_channel_home = messaging_store.viewer_sort();
    let sort_star_writer = messaging_store.viewer_sort();
    let sort_mutation_writer = messaging_store.viewer_sort();
    let sort_favorites = messaging_store.favorites();
    // The workspace-delete write authority — the ONE `BEGIN
    // IMMEDIATE` that reads the subject under the lock, decides, captures the
    // teardown plan, and cascades the row. A shared-pool clone of this store.
    let workspace_delete_writes = messaging_store.workspace_deletion();
    // `POST /api/workspaces` saga handles. Durable create
    // writer + member-display read are shared-pool clones of the ONE messaging
    // store; the name gate reuses the SAME `SqliteRegularWorkspaceNamespace` the
    // name-availability probe consumes (cloned before that mount moves it); the
    // ownership gate + per-agent enrollment reuse the ONE `SqliteAccessStore`; ids
    // are minted from the process `HostEnvironment`; the onboarding aftermath reuses
    // the SAME ensure-then-wake `OnboardingDeliveryAdapter` + daemon runtime-start
    // the agent-create saga uses — never a second delivery/start authority.
    let workspace_create_writer = messaging_store.workspace_creation();
    let workspace_create_onboarding_ensure = messaging_store.onboarding_inbox_ensure();
    // The agent workspace-list read for `GET
    // /internal/agents/{aid}/list-workspaces` — a shared-pool clone of the ONE store.
    let agent_list_workspaces_read = messaging_store.agent_workspace_list();
    // `GET /internal/agents/{aid}/list-channels` — the visible-channel
    // read + the caller peer-profile authorities (placement/horizon/read_messages), all
    // shared-pool clones of the ONE messaging store.
    let agent_list_channels_read = messaging_store.agent_channel_list();
    // The agent thread-follow/unfollow routes — the ONE
    // `thread_follow` store serves both `RootMessageLocate` and
    // `AgentThreadFollowWrite`; the `read_messages` capability is the ONE store.
    let agent_thread_follow_locate = messaging_store.thread_follow();
    let agent_thread_follow_writer = messaging_store.thread_follow();
    // `POST /internal/agents/{aid}/search-messages` — the LIKE search
    // read + the same peer-profile placement/horizon/read_messages authorities.
    let agent_search_read = messaging_store.agent_search();
    // `POST /internal/agents/{aid}/read-history` — the windowed read
    // + its workspace resolver (both the ONE agent-read-history store), the target
    // address resolver (the ONE `target_address`), and the read_messages capability.
    let agent_read_history_read = messaging_store.agent_read_history();
    let agent_read_history_target = target_address.clone();
    //`POST /internal/agents/{aid}/create-channel` fork saga — the
    // fork writer + source placement + create_channels capability, all the ONE store;
    // the target resolver is the ONE `target_address`; the aftermath is `lane_effects`.
    let agent_create_channel_writer = messaging_store.channel_fork();
    let agent_create_channel_placement = messaging_store.clone();
    let agent_create_channel_target = target_address.clone();
    // The create-channel awaited aftermath delivers the sealed context/prompt
    // seeds and starts member runtimes; it reuses the SAME durable inbox-ensure +
    // daemon delivery/start seams as workspace-create onboarding (fresh cheap handles).
    let agent_create_channel_message_ensure = messaging_store.committed_message_inbox_ensure();
    let profile_message_counts =
        silverhand_backend_server::identity_access::profile_message_counts::router(
            messaging_store.clone(),
            user_store.clone(),
            agent_detail_cold_read.clone(),
            SystemProcessClock,
        );
    let first_send_uploads = FirstSendAttachmentStore::new(
        messaging_store.attachment_reads(),
        messaging_store.attachment_writes(),
        blob_store.clone(),
        HostAttachmentEnvironment,
    );
    let (human_writes, agent_writes) =
        aftermath.write_doors(messaging_store, target_address, first_send_uploads);

    let host = host_runtime::router(
        CreateHostInvite::new(host_store.clone(), OsEntropy, SystemClock),
        config.daemon_npm_spec.clone(),
    );
    // The host-invite create door plus the read/list, connect-token resolve,
    // claim-or-create, and token-rotation family. Three (list/claim/rotate) sit behind the
    // human session perimeter; `resolve` is the one door the product ALLOWLISTS past the
    // bearer gate (it is daemon-authenticated: the caller proves possession of its connect
    // token in the handler, not a human session), so it mounts anonymously at the top level
    // with the other allowlisted faces, mirroring `installPublicBearerMiddleware`'s
    // allowlist (`health`, `server/info`, `hostConnectTokenResolve`). The remaining four
    // handlers (detail/revoke/connect-token rotate/delete) are mounted by #2223 CP5 below,
    // now that the convergence supplies their effect authorities. All bind the ONE
    // `SqliteHostRuntimeStore` (its Arc pool clones cheaply), never a second store.
    let host_list = host_runtime::list::router(host_store.clone());
    let host_resolve =
        host_runtime::resolve::router(host_store.clone(), host_store.clone(), SystemProcessClock);
    let host_claim = host_runtime::claim::router(ClaimLocalComputer::new(
        host_store.clone(),
        host_store.clone(),
        host_store.clone(),
        OsEntropy,
        SystemClock,
    ));
    // rotate-token's arm is the deployment profile, exactly like the local-runtime-catalog
    // router below.
    // EMBEDDED mints in-process and reveals the secret in the HTTP body. REMOTE-ONLY
    // delivers the replacement over the daemon's OWN websocket — the transport the
    // `daemon_router` accepts in either profile — and returns no secret, matching the TS
    // `standaloneMode` branch that drives `hostWsController.sendCommand(id,'rotate_token')`
    // rather than refusing. So the coordinator is `Some`, bound to the live delivery
    // registry via `host_token_rotation()`; a `transport_no_connection`/`503` is the
    // coordinator's own answer when that daemon is offline, NOT a composition-time `None`
    // stub (which would be a parity break — TS never categorically refuses here).
    let host_rotate_token = match config.deployment_profile {
        DeploymentProfile::Embedded => host_runtime::rotate_token::embedded_router(
            host_store.clone(),
            RotateHostToken::new(host_store.clone(), OsEntropy, SystemClock),
        ),
        DeploymentProfile::RemoteOnly => host_runtime::rotate_token::standalone_router(
            host_store.clone(),
            Some(RotateHostTokenThroughDaemon::new(
                RotateHostToken::new(host_store.clone(), OsEntropy, SystemClock),
                daemon_controller.host_token_rotation(),
            )),
        ),
    };
    // The four host residual routes whose effect authorities the convergence
    // now supplies — detail, revoke, connect-token rotate, and delete — mounted behind the
    // same human perimeter as their host siblings. Each reads the ONE
    // `SqliteHostRuntimeStore` as its `ComputerReadModel`; their live-control and cascade
    // effects all come from the ONE `daemon_controller` (self-reported connected name,
    // generation-bound disconnect, exact-seat stop + host-local purge) and the ONE `hub`
    // (per-viewer availability announce, agent-deletion `agent_changed`) — never a second
    // registry, store, hub, or blob root.
    let host_detail =
        host_runtime::detail::router(host_store.clone(), daemon_controller.connected_host_names());
    let host_revoke = host_runtime::revoke::router(
        host_store.clone(),
        RevokeComputer::new(
            host_store.clone(),
            hub.agent_availability_announcer(
                computer_agents_read.clone(),
                agent_availability_cold_read.clone(),
            ),
            daemon_controller.connection_control(),
            SystemClock,
        ),
    );
    // The host-management family's post-commit label-change effect. It reuses
    // the one computer-agents read authority and the one scope-event hub already in this
    // composition (both `.clone()`d — `hub` is moved into `foundation::scope_event_transport::router` below),
    // publishing the existing `agent_changed` invalidation for every agent on the renamed
    // computer. No second read model, no second publisher.
    let host_label_change = hub.host_label_change(computer_agents_read.clone());
    let host_rotate_connect_token = host_runtime::rotate_connect_token::router(
        host_store.clone(),
        RotateConnectToken::new(
            host_store.clone(),
            OsEntropy,
            SystemClock,
            daemon_controller.connection_control(),
        ),
        config.daemon_npm_spec.clone(),
    );
    // The computer-cascade teardown effects, composed from three existing authorities and
    // minting none of them: the deletion `agent_changed` producer over the one hub, the
    // daemon's exact-seat stop + host-local file purge, and physical blob deletion on the
    // one local blob root. The teardown read/write ports are the one `SqliteHostAgentStore`.
    let cascade_effects = host_runtime::cascade_effects::ProductionComputerCascadeEffects::new(
        hub.agent_deleted_announcer(),
        daemon_controller.cascade_daemon_effects(),
        blob_store.clone(),
    );
    // The ONE agent-deletion resumer, the single async worker every deletion
    // entrypoint shares. Assembled here — before `DeleteComputer::new` below consumes
    // `host_agent_store` and `cascade_effects` — from three already-composed authorities:
    // the durable `deletion_started_at` lifecycle store, the daemon fence binder minted
    // from the ONE registry (`daemon_controller.host_connections()`), and the SAME cascade
    // residue the host-delete path holds (cloned, never a second announce root). It is
    // cloned into exactly four places and nowhere else — a clone shares its
    // `Arc<Mutex<..>>` state, so there is one worker, one pending set, one retry ladder:
    //   1. boot `recover` (below, before this fn returns — before any listener is ready);
    //   2. the daemon attach `on_host_ready` observer (baked into the daemon router as a
    //      REQUIRED argument, so the seal edge can never land without the recovery push);
    //   3. the public DELETE route's `try_now` dispatch;
    //   4. shutdown `close` (via the `PreparedApp` field, ahead of transport teardown).
    let deletion_resumer = host_runtime::cascade_effects::production_agent_deletion_resumer(
        host_agent_store.clone(),
        cascade_effects.clone(),
        daemon_controller.host_connections(),
    );
    // The DELETE face's narrow ports, captured before the host-delete unit-of-work below
    // moves `host_agent_store`/`cascade_effects`: the SAME SQLite store as its teardown
    // (row-only branch) and its durable begin marker (active branch), the SAME cascade
    // residue for the row-only announce, and the ONE hub availability edge fired when a
    // full delete begins. No second store, marker authority, announcer, or audience.
    let agent_delete_teardown = host_agent_store.clone();
    let agent_delete_lifecycle = host_agent_store.clone();
    let agent_delete_residue = cascade_effects.clone();
    let agent_delete_availability =
        hub.agent_availability_edge(agent_availability_cold_read.clone());
    // The runtime-switch saga's four host-agent ports, captured before the
    // host-delete unit-of-work below moves `host_agent_store`. The switch snapshot read and
    // the three durable transition ports (lock write, seat read, inbox-reclaim write) are the
    // SAME ONE `SqliteHostAgentStore` the delete path holds — a clone shares its pool, so the
    // transition lock, seat, and inbox reclaim are ONE durable authority, never a second
    // store the fence could diverge across.
    let agent_switch_snapshot = host_agent_store.clone();
    let agent_switch_transitions = host_agent_store.clone();
    let agent_switch_seats = host_agent_store.clone();
    let agent_switch_inboxes = host_agent_store.clone();
    // The bulk-stop seat snapshot read for `POST /api/agents/{id}/stop`, captured
    // before the host-delete unit-of-work below moves `host_agent_store`. It is the SAME ONE
    // `SqliteHostAgentStore` — a clone shares its pool, so the stop route's seat snapshot is the
    // same durable seat authority the delete/switch paths hold, never a second store. The stop
    // route's gate read stays the separate `agent_detail_cold_read` authority (below); the two
    // reads are carried on two params, never fused into one combining read type.
    let agent_stop_seats = host_agent_store.clone();
    // The three host-agent ports the `POST /api/agents` create saga drives —
    // materialization lookup+write, arm provisioning, and the agent-graph read — captured
    // before the host-delete unit-of-work below moves `host_agent_store`. All three are the
    // SAME ONE `SqliteHostAgentStore` the delete/switch/stop paths hold: a clone shares its
    // pool, so the reservation/materialization write, the dependent-graph provisioning, and
    // the graph read are ONE durable authority, never a second store the saga could diverge
    // across.
    let agent_create_materialization = host_agent_store.clone();
    let agent_create_provisioning = host_agent_store.clone();
    let agent_create_graph = host_agent_store.clone();
    // The placement-read authority the internal profile-MCP data plane consults
    // for `{agent, host, deleting}`, captured before the host-delete unit-of-work below moves
    // `host_agent_store`. It is the SAME ONE `SqliteHostAgentStore` the delete/switch/stop/create
    // paths hold — a clone shares its pool — so the MCP placement lookup is the same durable
    // authority, never a second store.
    let agent_mcp_placement = host_agent_store.clone();
    // The list-workspaces `agent_workspace_index` probe resolves the agent's host via
    // the SAME placement authority; a clone shares the pool.
    let agent_dir_placement = host_agent_store.clone();
    // The create saga's remaining two read ports, captured before their originals are moved
    // downstream: the computer read model is the SAME ONE `SqliteHostRuntimeStore`
    // (`host_store`) host-detail/revoke read, and the handle namespace is the SAME ONE
    // `SqliteHandleNamespace` the `name-availability` face consumes (that mount moves the
    // original un-cloned).
    let agent_create_computers = host_store.clone();
    let agent_create_handles = handle_namespace.clone();
    // The ONE owned lifecycle boundary for the create route's two post-commit
    // remote legs (onboarding wake + runtime start). A clone shares its `Arc<Mutex<..>>`
    // state, so the instance the route dispatches into and the `PreparedApp` field that
    // `close`s it at shutdown are the same owner — there is never a second tracker a leg
    // could outlive. Mirrors the deletion resumer's composition pattern (built here, cloned
    // into the route, held by `PreparedApp`, closed FIRST at shutdown), never reusing that
    // deletion state machine.
    let create_aftermath = CreateAftermath::new();
    // The daemon websocket router, built now that the resumer exists: the resumer is its
    // MANDATORY host-ready observer. Binder (from `host_connections()`) and observer are
    // the same resumer over the same runtime registry — one generation truth.
    let daemon_router = daemon_runtime.router(std::sync::Arc::new(deletion_resumer.clone()));
    // ONE production runtime control for both agent runtime write faces.
    // clear-session (POST .../clear-session) and runtime-switch (PUT .../runtime) each hold a
    // clone of this SAME `ProductionRuntimeControl`, so both reach the one sealed-seat clear,
    // the one data-plane, and the one `RuntimeClearSessionWaiters` — constructed exactly once
    // here, never a second control. The switch's live-availability probe reuses the existing
    // typed `owning_host_data_plane()` accessor: only the `OwningHostDataPlane` capability
    // escapes, never the raw registry/dispatcher.
    let runtime_control = daemon_runtime.runtime_control();
    let agent_switch_availability =
        DataPlaneRuntimeAvailabilityProbe::new(daemon_controller.owning_host_data_plane());
    let host_delete = host_runtime::delete::router(DeleteComputer::new(
        host_agent_store.clone(),
        host_agent_store,
        cascade_effects,
        daemon_controller.connection_control(),
    ));
    // `/api/runtimes` is the one root face that is NOT anonymous (it sits under the
    // product's `/api/*` bearer gate with no allowlist entry), so it mounts behind this
    // same human-session perimeter. The profile — not a per-request flag — decides which
    // router: an embedded backend constructs the process's ONE runtime detector and can
    // answer; a control-plane backend is handed
    // [`local_runtime_catalog::control_plane_router`], which
    // has no detector field to hold and so can only refuse `503`. The detector is
    // constructed ONLY in the embedded arm, so a standalone process never obtains one.
    let local_runtime_catalog_router = match config.deployment_profile {
        DeploymentProfile::Embedded => {
            local_runtime_catalog::detecting_router(MachineRuntimeDetector::for_this_process())
        }
        DeploymentProfile::RemoteOnly => local_runtime_catalog::control_plane_router(),
    };
    // Assemble the two workspace-metadata-write outbound authorities and the
    // human-auth deps from real production adapters, BEFORE `hub`/`sessions` are moved
    // into the event transport below. The rename fan-out is ONE whole effect — author +
    // daemon delivery + conversation-scope publication — never a half-effect: the
    // messaging store answers author/read, `daemon_runtime.effects()` is the per-agent
    // runtime push, and the hub is the conversation-scope `message` publication. The
    // `metadata_changed` viewer hint routes through the SAME hub over the audience read.
    let workspace_rename_fanout = WorkspaceRenameFanoutAdapter::new(
        workspace_rename_messaging,
        daemon_runtime.effects(),
        hub.workspace_rename_publisher(),
    );
    let workspace_metadata_sink = hub.workspace_metadata_changed_sink(workspace_audience_read);
    // The five human identity/session routes, generic over each capability. In
    // production several resolve to one store: the user store answers email lookup +
    // profile read + profile write, and the session store answers issue + revoke. The
    // hasher is the real spawn-blocking Argon2id KDF; the mint is the real CSPRNG bearer
    // mint; `user_changed` is the production hub sink (signup + profile edit announce).
    // The router self-applies `require_human_session` to only its `me` pair, so it must
    // mount at the TOP LEVEL — inside `human_products` the anonymous signup/login/logout
    // trio would be gated before a session can exist. It is handed the SAME session-read
    // authority the outer perimeter uses.
    let embedded_profile_router =
        silverhand_backend_server::identity_access::embedded_profile::router(
            user_store.clone(),
            hub.user_changed_sink(),
            SystemSessionClock,
            config.deployment_profile,
            sessions.clone(),
        );
    let password_hasher = SpawnBlockingHasher::new(Argon2idPasswordHasher::production(OsEntropy));
    let password_change_router =
        silverhand_backend_server::identity_access::password_change::router(
            credential_store.clone(),
            password_hasher.clone(),
            SystemSessionClock,
            sessions.clone(),
        );
    let recovery_mailer = config.recovery_mailer();
    let password_recovery_enabled = recovery_mailer.is_some();
    let password_recovery_router =
        silverhand_backend_server::identity_access::password_recovery::router(
            silverhand_backend_server::identity_access::password_recovery::PasswordRecoveryDeps {
                store: credential_store.clone(),
                hasher: password_hasher.clone(),
                clock: SystemSessionClock,
                entropy: OsEntropy,
                key: RecoveryCodeKey::generate(&OsEntropy).map_err(|source| {
                    StartupError::Composition {
                        name: "password recovery key",
                        source: Box::new(source),
                    }
                })?,
                mailer: recovery_mailer,
            },
        );
    let human_auth_router = human_auth::router(
        HumanAuthDeps {
            handles: handle_namespace.clone(),
            emails: user_store.clone(),
            hasher: password_hasher.clone(),
            registration: human_registration,
            mint: EntropySessionTokenMint::new(OsEntropy),
            sessions: session_issue,
            revoke: session_revoke,
            credentials: credential_store,
            reads: user_store.clone(),
            writes: user_store.clone(),
            user_changed: hub.user_changed_sink(),
            clock: SystemSessionClock,
            signup_enabled: config.signup_enabled,
        },
        sessions.clone(),
    );
    // The human-directory + avatar family. All four routes (`GET /api/users`,
    // `POST`/`DELETE /api/me/avatar`, `GET /api/users/{id}/avatar`) resolve every
    // identity capability — directory read, avatar serve-ref read, profile read, and the
    // avatar pointer write unit-of-work — off the SAME `SqliteUserStore` handle, share
    // the production `user_changed` hub sink (the avatar upload/clear announce), and the
    // production wall clock. The blob capability is the ONE `LocalBlobStore` opened
    // above, wrapped by the `SingleKindAvatarBlobs` adapter under its ambient
    // `StorageKind::Local` — the composition-root pairing the adapter's construction
    // contract names. Like `human_auth`, `users::router` self-applies
    // `require_human_session` (all four routes are behind it), so it mounts at the TOP
    // LEVEL, not inside `human_products`.
    let avatar_blobs = SingleKindAvatarBlobs::new(blob_store.clone(), StorageKind::Local);
    // The ONE avatar-blob root the three agent-avatar faces share (GET
    // serve, POST upload, DELETE clear), over the SAME `LocalBlobStore` the human
    // avatar family uses, under the ambient `StorageKind::Local`. Cloned (not moved) so
    // the raw `blob_store` handle survives for the #2254 agent-chat audit attachment
    // serve below, which reads blobs directly, NOT through the avatar single-kind wrapper.
    let agent_avatar_blobs = SingleKindAvatarBlobs::new(blob_store.clone(), StorageKind::Local);
    let related_users =
        silverhand_backend_server::identity_access::related_users::router(user_store.clone());
    let account_deletion_router =
        silverhand_backend_server::identity_access::account_deletion::router(
            silverhand_backend_server::identity_access::account_deletion::AccountDeletionDeps {
                store: user_store.clone(),
                hasher: password_hasher.clone(),
                avatars: avatar_blobs.clone(),
                changed: hub.user_changed_sink(),
                daemons: daemon_controller.connection_control(),
                profile: config.deployment_profile,
            },
            sessions.clone(),
        );
    let me_avatar_design_router = require_human_session(
        avatar_design::me_router(avatar_design_read.clone()),
        sessions.clone(),
        PublicJsonUnauthorized,
    );
    let users_router = users::router(
        UsersDeps {
            directory: user_store.clone(),
            avatar_refs: user_store.clone(),
            reads: user_store.clone(),
            avatar_writes: user_store,
            blobs: avatar_blobs,
            user_changed: hub.user_changed_sink(),
            clock: SystemSessionClock,
        },
        sessions.clone(),
    );
    // The internal, runtime-credentialed self-scope family (five faces),
    // assembled here — before `human_products` below moves `agent_profile_write`,
    // `agent_avatar_blobs`, and `agent_detail_cold_read` into the PUBLIC avatar mount and
    // `access_store` into the workspace-access mount — then merged behind the SAME
    // `require_runtime_credential` layer its already-mounted siblings sit behind (so the
    // layer self-pins `{aid}` to the caller's own agent, and an unlisted method on any shared
    // node is axum's 405). Every owner is the ONE production authority its public counterpart
    // holds — the ONE `SqliteAgentProfileWrite` and ONE `SingleKindAvatarBlobs` #2225's
    // `agent_avatar_write` reprojects through, the ONE `agent_changed` hub sink, the ONE
    // `agent_detail_cold_read` full-agent reprojector for the avatar echo, and the ONE narrow
    // `agent_profile_read` for the profile/peer reads — cloned, never a second store. The
    // three shared internal nodes carry disjoint-method unions: `/profile` (GET read + POST
    // write, across two factories), `/avatar` (POST set + DELETE clear, one factory).
    // convergence: all six residual adapters were acquired in `PassiveResources`
    // before either audit writer started. This phase only consumes those typed owners.
    // The embedded-bootstrap token file write itself remains deferred to the final
    // fallible provision step after `recover()` below.
    let embedded_bootstrap_mint = EntropySessionTokenMint::new(OsEntropy);
    let embedded_bootstrap_user_changed = hub.user_changed_sink();
    // The ONE agent-profile-MCP data plane, lifted to a binding so BOTH the
    // internal self-scope face (below) and the PUBLIC read+probe pair (#2249, in
    // `human_products`) ride the identical `ProductionAgentProfileMcpDataPlane` over the
    // shared placement authority and the one live daemon registry — never a second data
    // plane. It is `Clone` (shares the placement handle + registry accessor).
    let mcp_data_plane = daemon_runtime.agent_profile_mcp_data_plane(agent_mcp_placement);
    // The real `agent_workspace_index` probe (#2341 #1/#33): reuses the ONE
    // OwningHostDataPlane transport; replaces the retired Null production mount.
    let agent_dir_probe = daemon_runtime.agent_dir_probe(agent_dir_placement);
    // #2262: assemble the split-perimeter reaction and attachment families before the
    // shared handles they borrow are moved into their public siblings. Both reaction
    // routers share the ONE SQLite mutation/read authority and the real `ScopeEventHub`
    // aftermath. All six public + four internal attachment faces share one read adapter,
    // one write adapter, one agent adapter, the process's ONE blob root, and the SAME hub
    // as the cursor-inert `attachment_changed` publisher.
    let human_reactions = reaction::human_router(
        reaction_store.clone(),
        reaction_store.clone(),
        hub.reaction_publisher(),
    );
    let agent_reactions = reaction::agent_router(
        reaction_store.clone(),
        reaction_store,
        hub.reaction_publisher(),
    );
    let human_attachments = attachment::router(
        attachment_reads,
        attachment_writes,
        blob_store.clone(),
        HostAttachmentEnvironment,
        hub.attachment_publisher(),
    );
    let agent_attachment_products = agent_attachment_upload_router(
        agent_attachment_target,
        agent_attachments.clone(),
        blob_store.clone(),
        HostAttachmentEnvironment,
    )
    .merge(agent_attachment_update_router(
        agent_attachments.clone(),
        blob_store.clone(),
        hub.attachment_publisher(),
    ))
    .merge(agent_attachment_locate_router(
        agent_attachments.clone(),
        blob_store.clone(),
    ))
    .merge(agent_attachment_download_router(
        agent_attachments,
        blob_store.clone(),
    ));
    let notifications = notification::router(
        notification_store.clone(),
        notification_store,
        hub.notification_aftermath(),
    );
    let self_profile_products = self_profile_router(agent_profile_read.clone())
        .merge(internal_profile_write_router(InternalProfileWriteState {
            writes: agent_profile_write.clone(),
            blobs: agent_avatar_blobs.clone(),
            announce: hub.agent_changed_sink(),
            reader: agent_profile_read.clone(),
            clock: SystemProcessClock,
        }))
        .merge(internal_avatar_write_router(InternalAvatarWriteState {
            writes: agent_profile_write.clone(),
            blobs: agent_avatar_blobs.clone(),
            announce: hub.agent_changed_sink(),
            reader: agent_detail_cold_read.clone(),
            clock: SystemProcessClock,
        }))
        .merge(peer_profile_router(PeerProfilePorts {
            placement: peer_profile_caller.clone(),
            horizon: peer_profile_caller.clone(),
            capability: peer_profile_caller,
            reader: agent_profile_read,
            membership: access_store.clone(),
        }))
        // #2234 CP7b: the three internal profile-MCP faces — `/profile/mcp` GET (read config)
        // + POST (write config), and `/profile/mcp/probe` POST. All three ride the ONE
        // production `ProductionAgentProfileMcpDataPlane`, whose State IS the port itself. It
        // consults the shared `SqliteHostAgentStore` placement authority (`agent_mcp_placement`)
        // for the `{agent, host, deleting}` fact, then the composition's ONE live daemon
        // registry (via `daemon_runtime`'s owning-host data plane) for the exact-custody wire —
        // never a second placement store or a second registry.
        .merge(profile_mcp_router(mcp_data_plane.clone()));
    let lane_human_products = activity::observe::router(lane_observation, lane_effects.clone())
        .merge(attention_preference::router(
            lane_attention_preferences,
            lane_effects.clone(),
        ))
        .merge(channel_directory::create_router(
            lane_channel_create,
            lane_effects.clone(),
        ))
        .merge(channel_lifecycle::mutation_router(
            lane_channel_mutation,
            lane_effects.clone(),
        ))
        .merge(conversation_log_debug::router(
            lane_log_debug_admission,
            conversation_log_debug::ProductionConversationLogDebugControl::process(),
            SystemProcessClock,
        ))
        .merge(conversation_runtime_control::router(
            agent_clear_session_admission.clone(),
            runtime_control.clone(),
        ))
        .merge(access::workspace_membership::router(
            access_store.clone(),
            lane_effects.clone(),
        ))
        .merge(access::workspace_membership::ownership_transfer_router(
            access_store.clone(),
            lane_effects.clone(),
        ))
        .merge(conversation_membership::public_router(
            lane_conversation_membership.clone(),
            SystemEnrollmentClock,
            lane_effects.clone(),
        ))
        .merge(human_status_router(HumanStatusState {
            write: task_status_write.clone(),
            aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
        }))
        .merge(human_claim_router(HumanClaimState {
            write: task_claim_write.clone(),
            aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
            assignees: lane_task_door.clone(),
        }))
        .merge(human_edit_router(HumanEditState {
            write: task_edit_write.clone(),
            aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
        }))
        .merge(human_create_router(HumanCreateState {
            post: lane_human_post,
            boards: lane_task_boards.clone(),
            write: task_create_write.clone(),
            aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
        }))
        .merge(human_task_board_router(HumanTaskBoardState {
            read: task_store.clone(),
        }))
        .merge(human_task_point_router(HumanTaskPointState {
            read: task_store.clone(),
        }));
    let lane_agent_products = conversation_membership::internal_router(
        lane_conversation_membership,
        lane_target_address,
        SystemEnrollmentClock,
        lane_effects.clone(),
    )
    .merge(task_update_status_router(TaskStatusUpdateState {
        door: lane_task_door.clone(),
        numbers: task_store.clone(),
        write: task_status_write,
        names: lane_task_door.clone(),
        aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
    }))
    .merge(task_claim_router(TaskClaimState {
        door: lane_task_door.clone(),
        numbers: task_store.clone(),
        write: task_claim_write.clone(),
        names: lane_task_door.clone(),
        aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
    }))
    .merge(task_unclaim_router(TaskUnclaimState {
        door: lane_task_door.clone(),
        numbers: task_store.clone(),
        write: task_claim_write,
        aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
    }))
    .merge(task_edit_router(TaskEditState {
        door: lane_task_door.clone(),
        numbers: task_store.clone(),
        write: task_edit_write,
        names: lane_task_door.clone(),
        aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
    }))
    .merge(task_create_router(TaskCreateState {
        resolver: lane_task_target,
        door: lane_task_door.clone(),
        write: task_create_write,
        names: lane_task_door.clone(),
        aftermath: PublishingTaskAftermath::new(lane_effects.clone()),
    }))
    .merge(task_detail_router(TaskDetailState {
        boards: lane_task_door,
        read: task_store.clone(),
    }))
    .merge(internal_schedule_list_router(ScheduleListState {
        list_reads: schedule_list_read.clone(),
    }))
    .merge(internal_schedule_cancel_router(ScheduleCancelState {
        cancel: schedule_cancel.clone(),
    }))
    .merge(schedule_create_router(ScheduleCreateState {
        resolver: lane_schedule_target,
        capability: lane_schedule_capability,
        store: schedule_create,
        cron: ProductionCronEvaluator,
        committed_effect: schedule_driver_handle,
    }));
    let human_products = require_human_session(
        host.merge(profile_message_counts)
            .merge(related_users)
            .merge(human_writes)
            .merge(lane_human_products)
            // Wave 3A checkpoint A6 (#2203): `POST /api/agents/{id}/direct-messages`, the
            // human direct-pair initiation write door, is not merged here — it arrives
            // inside `human_writes`, bound to the aftermath by the seam above. Its
            // method/path is distinct from the direct-pair read door merged next, so
            // neither shadows the other.
            .merge(direct_pairs::router(direct_pair_read_model))
            .merge(workspaces::router(workspace_cold_read))
            // #2223 CP3: the three authenticated workspace-metadata routes, behind this
            // same human perimeter. `workspace_detail` GET and `workspace_metadata_write`
            // PATCH share the `/api/workspaces/{id}` node — axum accumulates the disjoint
            // methods into one method router, so neither shadows the other, and both share
            // the parent `/api/workspaces` with the static `name-availability` GET and the
            // list GET already merged above. The metadata-write door binds the governance
            // read (`access_store`), the write UoW, the `metadata_changed` viewer-hint sink
            // and the whole rename fan-out assembled above, and the one shared process
            // clock (`SystemProcessClock`) — the same wall-clock owner every other
            // signed-millis consumer holds, never a second reader minted here.
            .merge(workspace_detail::router(workspace_detail_read))
            .merge(workspace_name_availability::router(workspace_namespace))
            .merge(workspace_metadata_write::router(
                access_store.clone(),
                workspace_metadata_writes,
                workspace_metadata_sink,
                workspace_rename_fanout,
                SystemProcessClock,
            ))
            .merge(availability::router(
                agent_availability_cold_read,
                daemon_controller.clone(),
            ))
            .merge(agent_detail::router(agent_detail_cold_read.clone()))
            .merge(agent_list::router(agent_list_cold_read))
            .merge(agent_presence::router(
                agent_presence_cold_read,
                SystemProcessClock,
            ))
            // #2234 CP2 — the thin public-agent write faces (#2225), behind the SAME
            // `require_human_session` perimeter as the agent read routes above. PATCH and
            // both avatar writes share ONE `SqliteAgentProfileWrite` authority and the ONE
            // production `agent_changed` hub sink; all three avatar faces (GET serve +
            // POST upload + DELETE clear) share ONE `SingleKindAvatarBlobs` root over the
            // ONE `LocalBlobStore`. PATCH accumulates onto the `/api/agents/{id}` node the
            // GET detail route owns (disjoint methods merge; GET stays the detail handler).
            .merge(agent_name_availability::router(handle_namespace))
            .merge(agent_patch::router(
                agent_detail_cold_read.clone(),
                agent_profile_write.clone(),
                hub.agent_changed_sink(),
                SystemProcessClock,
            ))
            // #2234 CP3: DELETE accumulates onto the SAME `/api/agents/{id}` node GET
            // detail and PATCH own (disjoint methods merge; GET/PATCH keep their handlers).
            // Its async `try_now` dispatch IS the one shared `deletion_resumer` — the same
            // worker boot `recover` armed and the daemon attach edge drives through
            // `on_host_ready`. Row-only teardown + durable begin marker are the ONE
            // `SqliteHostAgentStore`, the residue is the SAME cascade announce the resumer
            // holds, the availability edge is the ONE hub fan-out fired when a full delete
            // begins, and the clock stamps `deletion_started_at`. No second authority.
            .merge(agent_delete::router(
                agent_detail_cold_read.clone(),
                agent_delete_teardown,
                agent_delete_lifecycle,
                deletion_resumer.clone(),
                agent_delete_residue,
                agent_delete_availability,
                SystemProcessClock,
            ))
            // #2234 CP4 — the two agent runtime write faces, behind the SAME
            // `require_human_session` perimeter as the reads and the other writes above.
            // clear-session and runtime-switch hold the ONE `ProductionRuntimeControl`
            // (`runtime_control` clones), so both reach the one sealed-seat clear path.
            // runtime-switch's five #2232 owners resolve from exactly the two genuine
            // authorities and no more: transition-lock/seat/inbox + snapshot are the ONE
            // `SqliteHostAgentStore`; custody + presentation-focus are the ONE
            // `RuntimeCredentialAuthority` — custody exposed only through its read-only
            // `RuntimeCustodyReadPort` face, so `claim_or_current`/`finalize` can never leave
            // the authority. Each accumulates onto its own `.../clear-session` and
            // `.../runtime` path node (disjoint from the `{id}` node above).
            .merge(agent_clear_session::router(
                agent_detail_cold_read.clone(),
                agent_clear_session_admission,
                runtime_control.clone(),
            ))
            .merge(agent_runtime_switch::router(
                agent_detail_cold_read.clone(),
                agent_switch_snapshot,
                agent_switch_availability,
                agent_switch_coordination,
                agent_switch_transitions,
                agent_switch_seats,
                agent_switch_inboxes,
                runtime_credentials.clone(),
                runtime_credentials.clone(),
                runtime_control,
                hub.agent_changed_sink(),
                SystemProcessClock,
            ))
            // #2234 CP5 — `POST /api/agents/{id}/stop`, behind the SAME
            // `require_human_session` perimeter. Its two reads stay two distinct sole
            // authorities, carried on two params: the visibility+authority gate read is the
            // ONE `agent_detail_cold_read` every sibling face uses; the seat snapshot read is
            // the ONE `SqliteHostAgentStore` the delete/switch paths hold (`agent_stop_seats`
            // clone shares its pool). Neither is fused into a combining read type. Each running
            // seat is hard-stopped through `daemon_runtime.scope_runtime_stop()` — the exact
            // scope-keyed transport bound to the daemon's own host credentials + live registry,
            // never re-derived here; a single seat's refusal/fault is kept as ordered residue,
            // never short-circuiting the remaining seats.
            .merge(agent_stop::router(
                agent_detail_cold_read.clone(),
                agent_stop_seats,
                daemon_runtime.scope_runtime_stop(),
            ))
            // #2234 CP6 — `POST /api/agents` (agent create), behind the SAME
            // `require_human_session` perimeter. It accumulates onto the `/api/agents` node
            // the `GET` list already owns (disjoint methods merge; GET stays the list
            // handler). Its fourteen ports resolve to the genuine production authorities and
            // no more: the reserve→materialize→provision-graph saga's seven core authorities
            // are the ONE `SqliteHostRuntimeStore` (computer read), the ONE
            // `SqliteHandleNamespace` (handle read), the ONE `SqliteHostAgentStore`
            // (materialization write + arm provisioning + graph read — three faces of one
            // store), the daemon runtime's ONE `ProductionAgentMaterialize`, and the process
            // `HostEnvironment` entropy. The post-commit fan-out rides the SAME sinks its
            // siblings use — the ONE `agent_detail_cold_read` reprojection, the ONE
            // `agent_changed` hub sink, the ONE `DirectPairsChanged` hub edge (fired only on
            // a fresh creator↔agent pair), the runtime start through the daemon effects
            // facade, and — for the onboarding leg — the composed `OnboardingDeliveryAdapter`
            // that runs the ONE durable inbox-ensure authority (proving message-in-conversation
            // and canonical participation against committed rows) ahead of that SAME daemon
            // wake, mirroring the reference `deliverMessage`'s ensure-then-wake. The two remote
            // legs are handed to the ONE owned `create_aftermath` boundary, so the `201` seals
            // before either touches the host and shutdown cancels any in-flight leg.
            .merge(agent_create::router(
                agent_create_computers,
                agent_create_handles,
                agent_create_materialization,
                daemon_runtime.agent_materialize(),
                agent_create_provisioning,
                agent_create_graph,
                HostEnvironment,
                agent_detail_cold_read.clone(),
                hub.agent_changed_sink(),
                daemon_runtime.effects(),
                OnboardingDeliveryAdapter::new(
                    agent_create_onboarding_ensure,
                    daemon_runtime.effects(),
                ),
                hub.direct_pairs_changed_sink(),
                create_aftermath.clone(),
                SystemProcessClock,
            ))
            .merge(agent_avatar::router(
                agent_detail_cold_read.clone(),
                agent_avatar_read,
                agent_avatar_blobs.clone(),
            ))
            .merge(avatar_design::agent_router(
                agent_detail_cold_read.clone(),
                avatar_design_read.clone(),
            ))
            .merge(agent_avatar_write::router(
                agent_detail_cold_read.clone(),
                agent_profile_write,
                agent_avatar_blobs,
                hub.agent_changed_sink(),
                SystemProcessClock,
            ))
            // #2257 convergence — the ten residual-wave human-perimeter faces, each behind
            // this same `require_human_session` gate, each binding the SOLE production
            // adapter opened above (no second store, no duplicate data plane).
            //
            // #2253: agent runtime-state projection (GET `/api/agents/{id}/runtime-state`).
            // The cold-read answers the persisted shape; the ONE runtime-credential
            // authority is BOTH the custody read and the slot-presentation focus port.
            .merge(agent_runtime_state::router(
                runtime_state_cold_read,
                runtime_credentials.clone(),
                runtime_credentials.clone(),
            ))
            // #2254: the three owner-only agent-chat audit faces (chats list, transcript,
            // attachment). The read port is the source-bound `SqliteAgentAuditRead`; blobs
            // are the RAW `LocalBlobStore` (attachments are not avatars); the durable audit
            // sink is the required `AgentAuditFileSink` built above.
            .merge(agent_audit::router(
                agent_audit_read,
                blob_store.clone(),
                audit.agent_records.clone(),
            ))
            // #2251: the two schedule faces — list (GET `/api/agents/{id}/schedules`) and
            // cancel (POST `.../schedules/{scheduleId}/cancel`). Both re-use the ONE
            // agent-detail cold read for source-binding; the list read and the cancel UoW
            // are their own sole adapters.
            .merge(schedule_list_router(
                agent_detail_cold_read.clone(),
                schedule_list_read,
            ))
            .merge(schedule_cancel_router(
                agent_detail_cold_read.clone(),
                schedule_cancel,
            ))
            // #2252: the two workspace data-plane faces (tree + file), the public agent-pane
            // read. Agent-detail cold read source-binds; the scope store answers the
            // workspace-scope fact; the data plane is the daemon runtime's ONE accessor.
            .merge(workspace_data_plane::router(
                agent_detail_cold_read.clone(),
                workspace_scope_store,
                daemon_runtime.workspace_data_plane(),
            ))
            // #2249: the public profile-MCP read + probe pair (GET `/api/agents/{id}/profile/mcp`
            // + POST `.../profile/mcp/probe`), riding the SAME `mcp_data_plane` lifted above
            // that the internal self-scope face consumes — one data plane, two perimeters.
            .merge(public_profile_mcp::router(
                agent_detail_cold_read.clone(),
                mcp_data_plane,
            ))
            .merge(channel_directory::router(channel_directory_read))
            .merge(channel_lifecycle::router(channel_detail_read))
            .merge(conversation_agent_directory::router(
                conversation_agent_directory_read,
            ))
            .merge(conversation_agent_runtime::router(
                conversation_agent_runtime_read,
            ))
            // The reader owner is not chosen here: the seam binds it to the
            // delivery runtime by construction, so this line cannot fork the
            // HTTP reader onto an owner other than the one delivery writes into.
            .merge(daemon_runtime.agent_run_states_router(agent_run_states_admission))
            .merge(conversation_agent_activity::agent_router(
                conversation_agent_activity_read.clone(),
            ))
            .merge(conversation_agent_activity::router(
                conversation_agent_activity_read,
            ))
            .merge(activity::router(activity_read_model))
            .merge(conversation_ref::router(conversation_ref_read))
            .merge(conversation_unread::router(conversation_unread_read))
            .merge(conversation_latest::router(conversation_latest_read))
            .merge(conversation_threads::router(conversation_threads_read))
            // #2217: the two viewer-owned message-state lanes, mounted behind this same
            // authenticated human perimeter. Thread follow/unfollow rides its store and
            // the reused unread aftermath; saved read+mutation ride the one saved store,
            // with the hub itself as the scope-addressed `saved_changed` aftermath.
            .merge(thread_follow::router(
                thread_follow_store,
                thread_follow_aftermath,
            ))
            .merge(saved::router(
                saved_store.clone(),
                saved_store,
                hub.saved_publisher(),
            ))
            // Same path as the send POST already merged via `human_writes`. axum
            // accumulates the GET and the POST into one method router on the
            // shared path node, so neither shadows the other; only a repeated
            // *method* would panic, and GET vs POST is disjoint.
            .merge(conversation_message_window::router(
                conversation_message_window_read,
            ))
            .merge(human_search::router(human_search_read))
            // Phase A (#2341): the six viewer-sort / favorite faces, behind this
            // same human perimeter. All ride the ONE messaging store: the star
            // router resolves the channel home + `channels:<ws>` active directory
            // and writes/echoes through the sort authority; the touch/mode/order
            // router resolves list membership through the ONE `resolveListGroups`
            // resolver (which composes the channel-directory + favorites +
            // visible-workspaces reads) and writes/echoes through the same sort
            // authority; favorites GET reads the favorites projection. No second
            // sort store, resolver, or permission model.
            .merge(star_router(ChannelStarState {
                homes: sort_channel_home,
                writer: sort_star_writer,
            }))
            .merge(sort_mutation_router(SortMutationState {
                writer: sort_mutation_writer,
                hub: hub.clone(),
            }))
            .merge(favorites_router(FavoritesState {
                reads: sort_favorites,
            }))
            // Phase B (#2341): `DELETE /api/workspaces/{id}` accumulates onto the
            // `/api/workspaces/{id}` node the GET detail + PATCH metadata already
            // own (disjoint methods merge). Its write authority is the ONE
            // `BEGIN IMMEDIATE` delete over this store; its post-commit aftermath is
            // the SHARED `lane_effects` (runtime stop per torn-down conversation's
            // seats + `viewer_workspaces_changed { removed }` fan-out), never a
            // second effect owner.
            .merge(workspace_delete_router(WorkspaceDeleteState {
                writes: workspace_delete_writes,
                aftermath: ProductionWorkspaceDeletionAftermath::new(
                    hub.clone(),
                    daemon_runtime.conversation_run_terminalizer(),
                    ProductionMembershipRuntimeStop::new(
                        workspace_delete_runtime_seats,
                        daemon_runtime.scope_runtime_stop(),
                    ),
                    create_aftermath.clone(),
                    blob_store.clone(),
                ),
            }))
            // Phase B create (#2341): `POST /api/workspaces` accumulates onto the
            // `/api/workspaces` node the GET list owns (disjoint methods merge). The
            // saga reuses the SAME name namespace (409 gate), access store (ownership
            // 403 gate + per-agent enrollment), durable create writer + member-display
            // read, process entropy for id minting, and the SAME onboarding
            // deliver+start the agent-create saga uses — never a second authority.
            .merge(workspace_create_router(WorkspaceCreateState {
                membership_events: hub.workspace_membership_publisher(),
                writer: workspace_create_writer,
                entropy: HostEnvironment,
                clock: SystemCreationClock,
                aftermath: ProductionWorkspaceCreationAftermath::new(
                    OnboardingDeliveryAdapter::new(
                        workspace_create_onboarding_ensure,
                        daemon_runtime.effects(),
                    ),
                    daemon_runtime.effects(),
                    create_aftermath.clone(),
                ),
            }))
            // Unread/attention read lane, Wave 3A checkpoint A4. One `message_unread`
            // handle serves all three unread roll-up doors; `attention_projection` serves
            // the two tiered-attention doors.
            .merge(message_unread::router(message_unread_read))
            .merge(attention_projection::router(attention_projection_read))
            // #2262: the 3 notification, 3 human reaction, and 6 human attachment faces
            // all land behind the ONE human-session perimeter. The factories are split
            // where a route family crosses perimeters, so no internal face can leak into
            // this router and no public face can inherit runtime credentials.
            .merge(notifications)
            .merge(human_reactions)
            .merge(human_attachments)
            // Host-resources read lane, Wave 3A checkpoint A5 (#2198). Rides its own
            // by-path cold-read pool captured above, kept distinct from the teardown
            // resource authority.
            .merge(host_resources::router(host_resources_cold_read))
            .merge(presence_router)
            .merge(access::router(
                access_store.clone(),
                access_store,
                hub.workspace_membership_publisher(),
                SystemEnrollmentClock,
            ))
            // #2219 CP4 batch B, behind this same human perimeter. `host_list`'s
            // `GET /api/hosts` shares the path node with the invite `POST /api/hosts`
            // (`host` above): axum accumulates the two methods into one method router, so
            // neither shadows the other — only a repeated *method* would panic, and
            // GET vs POST is disjoint. `host_resolve` is NOT merged here: it is the one
            // allowlisted (anonymous) host door and mounts at the top level below. The
            // remaining two carry their own distinct paths.
            .merge(host_list)
            .merge(host_claim)
            .merge(host_rotate_token)
            // #2223 CP5: the four host residual routes, behind this same human perimeter.
            // `host_detail` GET and `host_delete` DELETE share the `/api/hosts/{id}` node —
            // axum accumulates their disjoint methods into one method router, so neither
            // shadows the other. `host_revoke` and `host_rotate_connect_token` carry their
            // own distinct sub-paths. All four sit in the `{id}` param subtree, disjoint
            // from the anonymous static `/api/hosts/connect-token/resolve` mounted below.
            .merge(host_detail)
            .merge(host_revoke)
            .merge(host_rotate_connect_token)
            .merge(host_delete)
            // #2234 CP1: the remaining host-management family (6 method faces), behind this
            // same human perimeter. `management::router` composes disconnect (POST
            // `/api/hosts/{id}/disconnect`), proxy-config (GET+PUT `/api/hosts/{id}/proxy-config`),
            // runtime-catalog (GET `/api/hosts/{id}/runtime-catalog`), update (PATCH
            // `/api/hosts/{id}` — accumulates onto the existing GET/DELETE node) and status
            // (GET `/api/hosts/{id}/status`). Every dependency is a real process owner already
            // in this scope: the one SQLite host store (read + update UoW + status read), the
            // live owning-host data plane / status seeds / admission / connection-control off
            // the daemon controller, the process clock, and the scope-event label-change effect.
            .merge(management::router(
                host_store.clone(),                         // computers (ComputerReadModel)
                host_store.clone(),                         // updates (UpdateComputerUnitOfWork)
                host_store.clone(), // status_read (HostStatusStoredReadModel)
                host_label_change,  // label_change (HostLabelChangeEffect)
                daemon_controller.owning_host_data_plane(), // data_plane
                daemon_controller.host_status_seeds(), // seeds
                daemon_controller.host_admission(), // admission
                daemon_controller.connection_control(), // disconnect
                SystemProcessClock, // clock
            ))
            // #2218: `/api/runtimes`, behind the human perimeter — anonymous → 401,
            // exactly as the product's un-allowlisted `/api/*` bearer gate refuses it.
            .merge(local_runtime_catalog_router),
        sessions.clone(),
        PublicJsonUnauthorized,
    );
    // #2234 CP7a (test-only): capture a clone of the runtime-credential authority BEFORE it is
    // moved into the perimeter below. The authority is `Arc`-backed (its process-local token
    // map is an `Arc<RwLock<…>>`), so the clone and the perimeter share ONE token store — a
    // bearer a test mints through this clone authenticates against the mounted router.
    #[cfg(test)]
    let test_runtime_credentials = runtime_credentials.clone();
    // #2250/#2261: both CLI audit facades ride this runtime-credential perimeter. The
    // invoke middleware is inside auth (so 401/auth-storage failures are zero-record) and
    // outside handlers; cli-trace uses the sibling preflight facade and is explicitly
    // excluded from invoke counting. Both share the ONE owner closed by PreparedApp.
    let observed_agent_products = audit_cli_invocations(
        agent_writes
            .merge(lane_agent_products)
            .merge(task_list_router(TaskListState {
                boards: task_list_boards,
                fetch: task_store,
                targets: task_list_targets,
            }))
            .merge(stale_message_resolution_router(
                stale_admission,
                stale_capability,
                TracingStaleMessageResolutionObserver,
            ))
            // #2234 CP7a: the internal self-scope profile/avatar/peer family assembled above,
            // merged into the SAME runtime-credential perimeter as the send-message/task-list/
            // stale-resolution siblings. Its `/internal/agents/{aid}/…` paths are all fresh
            // nodes (no collision with the public `/api/agents/…` family), and the wrapping
            // layer below self-pins `{aid}` so a foreign or missing/invalid bearer is a uniform
            // 401 across every listed method.
            .merge(self_profile_products)
            // #2262: the 2 reaction + 4 attachment internal faces are inside the typed
            // `cli.invoke` middleware and, because runtime auth wraps that middleware
            // below, record exactly once only after a principal has authenticated.
            .merge(agent_reactions)
            .merge(agent_attachment_products)
            // #2250: the CLI preflight-trace door (POST `/internal/agents/{aid}/cli-trace`),
            // behind the SAME runtime-credential perimeter, recording each typed preflight
            // failure through the required audit sink exactly once. The route and the
            // PreparedApp-held original share (via `Arc`) the SAME writer task.
            .merge(cli_trace_router(audit.cli_preflight.clone()))
            // Phase C (#2341): `GET /internal/agents/{aid}/list-workspaces`, behind the
            // SAME runtime-credential perimeter (the layer self-pins `{aid}`). The
            // workspace facts ride the ONE messaging store's agent-workspace-list read;
            // the per-workspace `agent_dir` is the REAL daemon `agent_workspace_index`
            // probe over the ONE OwningHostDataPlane transport (#2341 #1/#33): a
            // transport/host/malformed fault is a whole-response typed failure, an id
            // the daemon leaves unresolved degrades per-entry to `null`.
            .merge(agent_list_workspaces_router(AgentListWorkspacesState {
                reads: agent_list_workspaces_read,
                dirs: agent_dir_probe,
            }))
            // Phase C (#2341): `GET /internal/agents/{aid}/list-channels`, same runtime
            // perimeter. The visible-channel read + the caller placement/horizon/
            // read_messages authorities are all the ONE messaging store.
            .merge(agent_list_channels_router(AgentListChannelsState {
                reads: agent_list_channels_read,
            }))
            // Phase D (#2341): `POST /internal/agents/{aid}/follow-thread` +
            // `.../unfollow-thread`, same runtime perimeter. The ONE `thread_follow`
            // store serves the root-message locate + the agent follow/unfollow write
            // (upsert `thread_followers` + on-insert `observe` inbox backfill); the
            // `read_messages` capability is the ONE messaging store.
            .merge(agent_thread_follow_router(AgentThreadFollowState {
                locate: agent_thread_follow_locate,
                writer: agent_thread_follow_writer,
            }))
            // Phase C (#2341): `POST /internal/agents/{aid}/search-messages`, same
            // runtime perimeter. LIKE search read + peer-profile authorities.
            .merge(agent_search_router(AgentSearchState {
                reads: agent_search_read,
            }))
            // Phase C (#2341): `POST /internal/agents/{aid}/read-history`, same runtime
            // perimeter. Target-address resolve (message_history read intent) + workspace
            // resolver + read_messages capability + the windowed read.
            .merge(agent_read_history_router(AgentReadHistoryState {
                resolver: agent_read_history_target,
                reads: agent_read_history_read,
            }))
            // Phase D (#2341): `POST /internal/agents/{aid}/create-channel` fork saga,
            // same runtime perimeter. Target resolve (fork source, thread-suffix
            // refused) + source workspace + create_channels cap + the ONE fork writer
            // (canonical seed messages) + `lane_effects` workspace_channels_changed.
            .merge(agent_create_channel_router(AgentCreateChannelState {
                resolver: agent_create_channel_target,
                placement: agent_create_channel_placement,
                writer: agent_create_channel_writer,
                entropy: HostEnvironment,
                clock: SystemCreationClock,
                aftermath: ProductionChannelForkAftermath::new(
                    hub.clone(),
                    ConversationMessageDeliveryAdapter::new(
                        agent_create_channel_message_ensure,
                        daemon_runtime.effects(),
                    ),
                    daemon_runtime.effects(),
                    channel_fork_agent_audience,
                ),
            })),
        audit.cli_invoke,
    );
    let agent_products = require_runtime_credential(observed_agent_products, runtime_credentials);

    let event_config = EventWebSocketConfig::production(embedded_event_ws_allowed_origins(
        config,
        bound_listener_addr,
    )?)
    .map_err(|source| StartupError::Composition {
        name: "event websocket configuration",
        source: Box::new(source),
    })?;
    let (event_router, event_controller) =
        silverhand_backend_server::foundation::scope_event_transport::router(
            hub,
            sessions,
            scope_authority,
            // The transport's connect/disconnect sink is the presence feed sharing the
            // single `PresenceGraceTimers` above with the presence GET route. A live WS
            // connection marks the viewer online in the very set that route reads.
            presence_feed,
            event_config,
        );

    // `daemon_router` has registered DaemonRuntimeOwner as its event sink, with
    // activity/presence/status publishers on this same hub. The viewer transport
    // above completes that production fanout installation in both profiles.
    // This witnesses backend wiring, never whether a daemon is currently attached.
    boot.mark_outbound_event_fanout_installed();

    // The three ANONYMOUS root faces — mounted at the top level, ahead of no perimeter,
    // because a supervisor probes liveness/readiness and a client reads discovery before
    // it holds any session (the product allowlists `server/info` + `health` past the
    // bearer gate and registers `/readyz` outside `/api/*`). All three project from the
    // ONE deployment profile and the ONE process clock; `/readyz` additionally reads the
    // live `boot` holder and the shared-pool write probe.
    let server_info_router = server_info::router(ServerAdvertisement {
        signup_enabled: config.signup_enabled,
        password_recovery_enabled,
        account_deletion_enabled:
            silverhand_backend_server::identity_access::account_deletion::offered(
                config.deployment_profile,
            ),
        bind_address_scope: classify_bind_address_scope(&bound_listener_addr.ip().to_string()),
    });
    let health_router = process_health::router(server_mode, SystemProcessClock);
    let readyz_router = process_readiness::router(
        server_mode,
        deployment_mode,
        boot.clone(),
        readiness_write_probe,
        SystemProcessClock,
    );

    // #2234 CP3: boot recovery, fail-closed and BEFORE this fn returns — so it always
    // precedes `serve`'s `mark_listen_ready`, never racing a listener. `recover` re-lists
    // every durable `deletion_started_at` marker into the one resumer's pending set; a
    // marker whose host is not yet attached simply waits for that host's `on_host_ready`
    // (driven by the daemon attach edge once the listener is up). A persistence failure
    // here aborts startup rather than serving with an unrecovered deletion backlog.
    deletion_resumer
        .recover()
        .await
        .map_err(|source| StartupError::Composition {
            name: "agent deletion resumer recover",
            source: Box::new(source),
        })?;

    // Console is opt-in on the product listener, with its own bearer perimeter.
    // The HTTPS edge forwards these routes to the same upstream as the product API;
    // the private metrics listener never serves Console routes.
    let console_router = crate::foundation::operator_console::router(config)
        .await?
        .unwrap_or_default();

    // #2255/#2257: the embedded-bootstrap one-shot token authority is the LAST fallible
    // startup step. The listener has already bound, and `bound_listener_addr` is the
    // `local_addr()` witness from that exact listener; only now may the composition decide
    // whether to mint/write, still before `serve` can accept. The arm decision uses the
    // SAME loopback predicate the route applies to the live peer (`is_loopback_ip`, incl
    // the IPv4-mapped form), gated on the embedded profile AND a launcher-provided path.
    // When armed: mint a fresh CSPRNG plaintext, write it to a 0600 same-dir temp then
    // atomically rename it into place, and retain only its hash — the raw token is never
    // logged and is dropped here. Otherwise the authority stays disabled while startup
    // best-effort removes both the final token file and its deterministic temp sibling.
    // Missing artifacts are normal; unlink faults remain observable as warnings without
    // exposing either artifact path or raw token contents.
    let embedded_bootstrap_token =
        super::bootstrap::provision(config, bound_listener_addr, &embedded_bootstrap_mint).await?;
    // The router is built infallibly AFTER the token decision; it shares the one authority
    // `Arc` with the PreparedApp-held handle, so a shutdown/rollback cleanup on that handle
    // also disarms this route's view.
    let embedded_bootstrap_router =
        embedded_bootstrap::router(embedded_bootstrap::EmbeddedBootstrapDeps {
            bootstrap: embedded_bootstrap_store,
            mint: embedded_bootstrap_mint,
            sessions: embedded_bootstrap_sessions,
            user_changed: embedded_bootstrap_user_changed,
            clock: SystemSessionClock,
            token: embedded_bootstrap_token.clone(),
        });

    let activity_retention = super::activity_retention::ActivityRetention::start(
        activity_retention_store,
        activity_retention_hub,
        SystemProcessClock,
    )
    .await?;
    Ok(FinalAssembly {
        activity_retention,
        router: Router::new()
            .merge(human_products)
            // #2223 CP3: the five human identity/session routes mount at the top level,
            // NOT inside `human_products`. The router self-wraps `require_human_session`
            // around only its `me` pair; the anonymous signup/login/logout trio is merged
            // outside any perimeter, exactly as the product allowlists them past the
            // bearer gate — placing it behind the outer perimeter would make login
            // unreachable before a session exists.
            .merge(human_auth_router)
            .merge(password_change_router)
            .merge(account_deletion_router)
            .merge(password_recovery_router)
            .merge(embedded_profile_router)
            // #2223 CP4: the human-directory + avatar family also mounts at the top level.
            // `users::router` self-wraps `require_human_session` around all four of its
            // routes, so it is merged here rather than inside `human_products` (a second
            // outer perimeter would double-authenticate). Its paths are disjoint from every
            // sibling: `/api/users` and `/api/users/{id}/avatar` share no method node with
            // any mounted route, and `/api/me/avatar` is a child path of `human_auth`'s
            // `/api/me`, never the same node.
            .merge(users_router)
            .merge(me_avatar_design_router)
            .merge(agent_products)
            .merge(event_router)
            .merge(daemon_router)
            .merge(server_info_router)
            .merge(health_router)
            // #2219: the connect-token resolve door is allowlisted past the bearer gate
            // (daemon-authenticated in-handler), so it mounts here at the top level ahead
            // of the human perimeter, alongside the other anonymous faces — never behind
            // `require_human_session`, which would reject the daemon's connect-token bearer
            // before the handler's own verification could run.
            .merge(host_resolve)
            // #2255/#2257: the embedded-bootstrap first-run grant mounts at the TOP LEVEL,
            // ahead of every session perimeter — it is the door through which the very
            // first local viewer obtains a session, so it cannot sit behind
            // `require_human_session`. Its own per-request `LoopbackPeer` gate and the
            // one-shot token authority are the whole admission; a non-loopback peer,
            // disabled authority, or wrong/spent token all collapse to the
            // indistinguishable 404.
            .merge(embedded_bootstrap_router)
            .merge(readyz_router)
            .merge(console_router)
            .route(
                "/metrics",
                axum::routing::any(|| async { axum::http::StatusCode::FORBIDDEN }),
            )
            // The one app/proxy request-body ceiling (128 MiB resource cap), applied
            // as the OUTERMOST layer over the fully-composed router — ahead of every
            // auth perimeter and domain check, and covering both deployment arms since
            // they are already merged into this single router. Product routes keep their
            // tighter in-stream caps; this only bounds the outer envelope.
            .layer(axum::middleware::from_fn(
                crate::foundation::request_perimeter::enforce_request_body_ceiling,
            ))
            .layer(axum::middleware::from_fn(
                crate::foundation::observability::observe_http,
            )),
        boot,
        event_controller,
        daemon_controller,
        presence_timers,
        deletion_resumer,
        create_aftermath,
        embedded_bootstrap_token,
        #[cfg(test)]
        test_runtime_credentials,
    })
}

/// #task:2343 — the event-WS origin allowlist for the composed backend.
///
/// The launcher-provided `SILVERHAND_ALLOWED_ORIGINS` are honored verbatim. For the
/// EMBEDDED profile bound to a LOOPBACK address, the finalized bound loopback origin
/// (`http://127.0.0.1:<port>` / `http://[::1]:<port>`) is ADDED to the allowlist —
/// mirroring the TypeScript embedded backend, which pins the event-WS allowlist to the
/// exact origin the desktop reaches it at. Embedded runs on an ephemeral `:0` port the
/// launcher cannot know before the child binds, so the bound origin is derived HERE (from
/// this listener's real `local_addr`) rather than pre-computed launcher-side. A non-
/// loopback or non-embedded bind adds nothing (standalone/remote declare their allowlist
/// explicitly). The bound origin is de-duplicated against the launcher-provided set.
fn embedded_event_ws_allowed_origins(
    config: &crate::AppConfig,
    bound_listener_addr: std::net::SocketAddr,
) -> Result<
    Vec<silverhand_backend_server::foundation::scope_event_transport::AllowedOrigin>,
    StartupError,
> {
    use silverhand_backend_server::embedded_bootstrap::is_loopback_ip;
    use silverhand_backend_server::foundation::scope_event_transport::AllowedOrigin;

    let mut origins = config.allowed_origins().to_vec();
    let is_embedded_loopback = matches!(config.deployment_profile(), DeploymentProfile::Embedded)
        && is_loopback_ip(bound_listener_addr.ip());
    if is_embedded_loopback {
        let ip = bound_listener_addr.ip();
        let authority = if ip.is_ipv6() {
            format!("http://[{}]:{}", ip, bound_listener_addr.port())
        } else {
            format!("http://{}:{}", ip, bound_listener_addr.port())
        };
        let bound_origin =
            AllowedOrigin::parse(authority).map_err(|source| StartupError::Composition {
                name: "embedded bound event-ws origin",
                source: Box::new(source),
            })?;
        if !origins.iter().any(|existing| existing == &bound_origin) {
            origins.push(bound_origin);
        }
    }
    Ok(origins)
}

#[cfg(test)]
mod event_ws_origin_tests {
    use super::embedded_event_ws_allowed_origins;
    use crate::AppConfig;
    use silverhand_backend_server::foundation::deployment_profile::DeploymentProfile;
    use silverhand_backend_server::foundation::scope_event_transport::AllowedOrigin;
    use std::net::SocketAddr;

    fn config(profile: DeploymentProfile, origins: &[&str]) -> AppConfig {
        AppConfig::new(
            "db.sqlite".into(),
            "blobs".into(),
            "127.0.0.1:0".parse().unwrap(),
            origins
                .iter()
                .map(|o| AllowedOrigin::parse((*o).to_owned()).unwrap())
                .collect(),
            None,
            profile,
            true,
            Some("logs".into()),
            None,
            None,
        )
        .unwrap()
    }

    fn origin(raw: &str) -> AllowedOrigin {
        AllowedOrigin::parse(raw.to_owned()).unwrap()
    }

    #[test]
    fn embedded_loopback_bind_adds_the_finalized_bound_origin() {
        let cfg = config(DeploymentProfile::Embedded, &["http://localhost:5173"]);
        let bound: SocketAddr = "127.0.0.1:51763".parse().unwrap();
        let origins = embedded_event_ws_allowed_origins(&cfg, bound).unwrap();
        assert!(origins.contains(&origin("http://localhost:5173")));
        assert!(
            origins.contains(&origin("http://127.0.0.1:51763")),
            "the ephemeral bound loopback origin is added (launcher cannot know :0 port pre-spawn)"
        );
    }

    #[test]
    fn embedded_ipv6_loopback_bind_is_bracketed() {
        let cfg = config(DeploymentProfile::Embedded, &["http://localhost:5173"]);
        let bound: SocketAddr = "[::1]:40001".parse().unwrap();
        let origins = embedded_event_ws_allowed_origins(&cfg, bound).unwrap();
        assert!(origins.contains(&origin("http://[::1]:40001")));
    }

    #[test]
    fn embedded_bound_origin_is_not_duplicated_if_already_present() {
        let cfg = config(DeploymentProfile::Embedded, &["http://127.0.0.1:51763"]);
        let bound: SocketAddr = "127.0.0.1:51763".parse().unwrap();
        let origins = embedded_event_ws_allowed_origins(&cfg, bound).unwrap();
        let count = origins
            .iter()
            .filter(|o| **o == origin("http://127.0.0.1:51763"))
            .count();
        assert_eq!(count, 1, "no duplicate bound origin");
    }

    #[test]
    fn remote_only_bind_adds_nothing() {
        let cfg = config(
            DeploymentProfile::RemoteOnly,
            &["https://silverhand.example"],
        );
        let bound: SocketAddr = "127.0.0.1:51763".parse().unwrap();
        let origins = embedded_event_ws_allowed_origins(&cfg, bound).unwrap();
        assert_eq!(
            origins,
            vec![origin("https://silverhand.example")],
            "standalone/remote declares its own allowlist; the bound origin is not injected"
        );
    }

    #[test]
    fn embedded_non_loopback_bind_adds_nothing() {
        let cfg = config(DeploymentProfile::Embedded, &["http://localhost:5173"]);
        let bound: SocketAddr = "10.0.0.5:51763".parse().unwrap();
        let origins = embedded_event_ws_allowed_origins(&cfg, bound).unwrap();
        assert_eq!(origins, vec![origin("http://localhost:5173")]);
    }
}
