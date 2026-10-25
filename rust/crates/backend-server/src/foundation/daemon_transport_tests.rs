#[test]
fn workspace_frame_budget_requires_live_correlated_success_and_complete_envelope() {
    use super::*;
    let (tx, _rx) = oneshot::channel();
    let mut pending = HashMap::from([(
        "read-1".to_owned(),
        PendingCommand {
            kind: IssuedDaemonCommandKind::AgentWorkspaceFileRead,
            capability_refusal: command::rejects_every_capability_refusal,
            response: tx,
            deadline: Instant::now() + Duration::from_secs(30),
        },
    )]);
    let wire = json!({"direction":"daemon_to_server", "type":"command_response", "id":"read-1",
        "kind":"agent_workspace_file_read", "result":{"content":"\u{1}".repeat(512*1024)}})
    .to_string();
    assert!(wire.len() > 3 * 1024 * 1024);
    let frame = parse_daemon_frame(&wire).unwrap();
    assert!(daemon_frame_fits_budget(wire.len(), Some(&frame), &pending));
    assert!(daemon_frame_fits_budget(
        MAX_WORKSPACE_FILE_FRAME_BYTES,
        Some(&frame),
        &pending
    ));
    assert!(!daemon_frame_fits_budget(
        MAX_WORKSPACE_FILE_FRAME_BYTES + 1,
        Some(&frame),
        &pending
    ));
    assert!(!daemon_frame_fits_budget(
        wire.len(),
        Some(&frame),
        &HashMap::new()
    ));
    pending.get_mut("read-1").unwrap().kind = IssuedDaemonCommandKind::HostStatus;
    assert!(!daemon_frame_fits_budget(
        wire.len(),
        Some(&frame),
        &pending
    ));
    pending.get_mut("read-1").unwrap().kind = IssuedDaemonCommandKind::AgentWorkspaceFileRead;
    pending.get_mut("read-1").unwrap().deadline = Instant::now();
    assert!(!daemon_frame_fits_budget(
        wire.len(),
        Some(&frame),
        &pending
    ));
    pending.get_mut("read-1").unwrap().deadline = Instant::now() + Duration::from_secs(30);
    let error = parse_daemon_frame(
        &json!({"direction":"daemon_to_server", "type":"command_response", "id":"read-1",
        "kind":"agent_workspace_file_read", "error":{"code":"internal_error", "message":"x"}})
        .to_string(),
    )
    .unwrap();
    assert!(!daemon_frame_fits_budget(
        MAX_DAEMON_FRAME_BYTES + 1,
        Some(&error),
        &pending
    ));
    assert!(daemon_frame_fits_budget(
        MAX_DAEMON_FRAME_BYTES,
        Some(&error),
        &pending
    ));
    assert!(!daemon_frame_fits_budget(
        MAX_DAEMON_FRAME_BYTES + 1,
        None,
        &pending
    ));
}

use std::ops::Range;

use silverhand_backend_core::{
    conversation_messaging::messaging::{MessageContent, ReferencePositions},
    conversation_messaging::runtime_delivery::{
        ClaimPendingInbox, ClaimedInboxBatch, DeliveryInstruction, DeliveryLeaseOwner,
        InboxDeliveryId, RuntimeDispatchReadPort, RuntimeDispatchTarget, RuntimeInboxCustodyPort,
    },
    foundation::identity::{AgentId, ConversationId},
    foundation::persistence::PersistenceFailure,
    host_runtime::runtime_credential::{
        ClaimRuntimeCustody, RuntimeCustodyPort, RuntimeEventTime, RuntimePrincipal, RuntimeScope,
    },
    host_runtime::{
        aftermath::DaemonConnectionControl, read::ComputerReadModel, rotation::DaemonRotationAck,
    },
};
use silverhand_backend_sqlite::{
    conversation_messaging::runtime_delivery::SqliteRuntimeDeliveryStore,
    host_runtime::SqliteHostRuntimeStore,
    host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

use crate::{
    application_orchestration::daemon_delivery::DaemonDeliveryRuntime,
    current_schema,
    foundation::scope_event_transport::ScopeEventHub,
    host_runtime::daemon_runtime_transport::{
        DeliveryDisposition, compose_delivery_text, delivery_disposition,
    },
    host_runtime::host_agent_background::agent_deletion_resumer::DeletionFenceBinder,
    host_runtime::host_control::management::disconnect::DaemonConnectionControlHandle,
    host_runtime::host_control::{
        status_seed::DaemonCustody, status_snapshot::StatusSnapshotOutcome,
    },
    host_runtime::runtime_credential::RuntimeCredentialAuthority,
    host_runtime::scope_runtime_stop::ProductionScopeRuntimeStop,
    host_runtime::{
        cascade_effects::{DaemonCascadeEffects, DaemonHostConnections},
        daemon_runtime_codecs::runtime_start as runtime_start_codec,
        rotate_token::{parse_rotation_ack, rotation_params},
    },
};

use super::*;

fn attach_for_command_test(
    registry: &DaemonRegistry,
    host: &HostId,
    instance: DaemonInstanceId,
) -> (String, mpsc::Receiver<ConnectionControl>) {
    let (commands, receive) = mpsc::channel(4);
    let (close, _close_receive) = watch::channel(None);
    let connection = registry.reserve(host.as_str(), commands, close).unwrap();
    assert!(
        registry
            .attach(
                host.as_str(),
                &connection,
                AttachedDaemon {
                    daemon_instance: instance,
                    host_name: host.as_str().to_owned(),
                },
            )
            .unwrap()
    );
    (connection, receive)
}

struct ControlledCommandSink {
    entered_first_write: Arc<Notify>,
    release_first_write: Arc<Notify>,
    frames: Arc<Mutex<Vec<Value>>>,
    sends: usize,
}

impl CommandWireSink for ControlledCommandSink {
    async fn send(&mut self, message: Message, _deadline: Instant) -> InterruptibleSend {
        self.sends += 1;
        if self.sends == 1 {
            self.entered_first_write.notify_one();
            self.release_first_write.notified().await;
        }
        let Message::Text(frame) = message else {
            panic!("business command sink received a non-text frame");
        };
        self.frames
            .lock()
            .unwrap()
            .push(serde_json::from_str(&frame).unwrap());
        InterruptibleSend::Sent
    }
}

#[tokio::test]
async fn command_metrics_observe_timeout_success_and_disconnection_once() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new()
        .set_buckets(&[0.001, 0.01])
        .unwrap()
        .build_recorder();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (commands, mut receive) = mpsc::channel(32);
    let attachment = DaemonAttachment {
        host_id: "private-host-id".to_owned(),
        connection_id: "private-connection-id".to_owned(),
        daemon_instance: DaemonInstanceId::parse("instance-test").unwrap(),
        host_name: "private-name".into(),
        commands,
    };
    let encode = || command::host_status(json!({}));
    let timed_out = attachment.command(encode(), Duration::from_millis(5)).await;
    assert!(matches!(timed_out, Err(CommandDispatchError::TimedOut)));
    let _ = receive.recv().await.unwrap();
    let respond = async {
        let ConnectionControl::Command(request) = receive.recv().await.unwrap() else {
            panic!("command expected")
        };
        request.response.send(Ok(json!({"ok":true}))).unwrap();
    };
    let (success, ()) = tokio::join!(
        attachment.command(encode(), Duration::from_secs(1)),
        respond
    );
    assert!(success.is_ok());
    drop(receive);
    assert!(matches!(
        attachment.command(encode(), Duration::from_secs(1)).await,
        Err(CommandDispatchError::ProvenNotDispatched)
    ));
    let text = recorder.handle().render();
    for outcome in ["timeout", "ok", "disconnected"] {
        let line = text
            .lines()
            .find(|line| {
                line.starts_with("silverhand_daemon_command_duration_seconds_count{")
                    && line.contains(&format!("outcome=\"{outcome}\""))
            })
            .unwrap();
        assert!(line.contains("command=\"host_status\""));
        assert!(line.ends_with(" 1"));
    }
    assert!(!text.contains("private-"));
}

#[tokio::test(start_paused = true)]
async fn production_dequeue_to_wire_gate_drops_a_queued_set_after_its_budget_expires() {
    let (commands, mut receive) = mpsc::channel(32);
    let attachment = DaemonAttachment {
        host_id: "host-1".to_owned(),
        connection_id: "connection-1".to_owned(),
        daemon_instance: DaemonInstanceId::parse("instance-1").unwrap(),
        host_name: "host".to_owned(),
        commands,
    };

    let entered_first_write = Arc::new(Notify::new());
    let release_first_write = Arc::new(Notify::new());
    let frames = Arc::new(Mutex::new(Vec::new()));
    let wire = tokio::spawn({
        let entered_first_write = Arc::clone(&entered_first_write);
        let release_first_write = Arc::clone(&release_first_write);
        let frames = Arc::clone(&frames);
        async move {
            let mut sink = ControlledCommandSink {
                entered_first_write,
                release_first_write,
                frames,
                sends: 0,
            };
            let mut next_command = 0;
            let mut pending_responses = Vec::new();
            for _ in 0..2 {
                let command = match receive.recv().await.unwrap() {
                    ConnectionControl::Command(command) => command,
                    ConnectionControl::Hydrated(_) => panic!("expected command"),
                };
                match send_dequeued_command(command, &mut next_command, &mut sink).await {
                    DequeuedCommandSend::Skipped => {}
                    DequeuedCommandSend::Sent { response, .. } => {
                        pending_responses.push(response);
                    }
                    DequeuedCommandSend::Terminate | DequeuedCommandSend::Close(_) => {
                        panic!("controlled sink cannot terminate the command loop")
                    }
                }
            }
            pending_responses
        }
    });

    let first_attachment = attachment.clone();
    let first = tokio::spawn(async move {
        first_attachment
            .command(command::host_status(json!({})), Duration::from_secs(60))
            .await
    });
    entered_first_write.notified().await;

    let second_attachment = attachment.clone();
    let second = tokio::spawn(async move {
        second_attachment
            .command(
                command::proxy_config_set(json!({ "proxy_url": "http://proxy" })),
                Duration::from_secs(10),
            )
            .await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(matches!(
        second.await.unwrap(),
        Err(CommandDispatchError::TimedOut)
    ));

    // The controlled sink is the socket-write seam used by the same production
    // dequeue function. Once the first write resumes, the late SET is dequeued
    // but must be rejected before this sink can observe it. Removing the
    // production expiry/cancellation branch records a second frame and kills the
    // assertion below.
    release_first_write.notify_one();
    let pending_responses = wire.await.unwrap();
    assert_eq!(pending_responses.len(), 1);
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["kind"], "host_status");
    first.abort();
}

#[tokio::test]
async fn production_status_adapter_never_re_resolves_captured_custody_by_host_and_instance() {
    let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
    let host = HostId::parse("host-1").unwrap();
    let instance = DaemonInstanceId::parse("instance-1").unwrap();

    let (a_commands, _a_receive) = mpsc::channel(4);
    let (a_close, _a_close_receive) = watch::channel(None);
    let connection_a = registry
        .reserve(host.as_str(), a_commands, a_close)
        .unwrap();
    assert!(
        registry
            .attach(
                host.as_str(),
                &connection_a,
                AttachedDaemon {
                    daemon_instance: instance.clone(),
                    host_name: "host-a".to_owned(),
                },
            )
            .unwrap()
    );
    let data_plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let captured_a = DaemonCustody::attached_custody(&data_plane, &host)
        .unwrap()
        .expect("connection A is attached");
    assert_eq!(captured_a.connection_id, connection_a);

    registry.remove(host.as_str(), &connection_a);
    let (b_commands, mut b_receive) = mpsc::channel(4);
    let (b_close, _b_close_receive) = watch::channel(None);
    let connection_b = registry
        .reserve(host.as_str(), b_commands, b_close)
        .unwrap();
    assert!(
        registry
            .attach(
                host.as_str(),
                &connection_b,
                AttachedDaemon {
                    daemon_instance: instance,
                    host_name: "host-b".to_owned(),
                },
            )
            .unwrap()
    );

    // This call crosses the production DaemonCustody adapter and the real
    // OwningHostDataPlane pre-send check. An implementation that discards A's
    // connection id and re-resolves by host+same-instance queues host_status on
    // B, then waits for B's answer. Preserve the queue assertion even if the
    // probe times out so that mutation fails on the generation leak itself.
    let outcome = tokio::time::timeout(
        Duration::from_millis(50),
        DaemonCustody::probe_status(&data_plane, host, captured_a),
    )
    .await;
    assert!(matches!(
        b_receive.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        outcome.expect("captured A must settle without waiting on B"),
        StatusSnapshotOutcome::Disconnected
    ));
}

mod scope_runtime_stop_adapter {
    use std::{io, thread};

    use silverhand_backend_core::{
        foundation::identity::{AgentId, ConversationId, UserId},
        foundation::wire_number::JsSafeInteger,
        host_runtime::scope_runtime_stop::{
            ScopeRuntimeStopOutcome, ScopeRuntimeStopPort, ScopeRuntimeStopRefusal,
            ScopeRuntimeStopTarget,
        },
        host_runtime::{
            computer::{ComputerRecord, HostKind, ProvisioningSource},
            read::ComputerReadModel,
        },
    };

    use crate::host_runtime::daemon_runtime_codecs::runtime_stop::RuntimeStopCommand;
    use crate::host_runtime::scope_runtime_stop::{
        ProductionScopeRuntimeStop, ScopeRuntimeStopFailure, stop_captured,
    };

    use super::*;

    #[derive(Clone)]
    struct StopComputerRead {
        record: Option<ComputerRecord>,
        fails: bool,
    }

    impl ComputerReadModel for StopComputerRead {
        async fn list_for_owner(
            &self,
            _owner: UserId,
        ) -> Result<Vec<ComputerRecord>, PersistenceFailure> {
            unreachable!("exact-host runtime stop never lists computers")
        }

        async fn read_computer(
            &self,
            _computer: HostId,
        ) -> Result<Option<ComputerRecord>, PersistenceFailure> {
            if self.fails {
                Err(PersistenceFailure::from_source(io::Error::other(
                    "host read failed",
                )))
            } else {
                Ok(self.record.clone())
            }
        }
    }

    fn computer(
        id: &str,
        activated_at: Option<i64>,
        finalized_at: Option<i64>,
        revoked_at: Option<i64>,
    ) -> ComputerRecord {
        ComputerRecord {
            id: HostId::parse(id).unwrap(),
            name: id.to_owned(),
            kind: HostKind::Remote,
            created_at: 1,
            owner: Some(UserId::parse("owner-1").unwrap()),
            installation_id: None,
            last_seen_at: None,
            activated_at,
            finalized_at,
            revoked_at,
            provisioning_source: ProvisioningSource::UserInvite,
            display_label: None,
        }
    }

    fn target(host: &str, scope: RuntimeScope, generation: i64) -> ScopeRuntimeStopTarget {
        ScopeRuntimeStopTarget::new(
            HostId::parse(host).unwrap(),
            scope,
            silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(
                generation,
            )
            .unwrap(),
        )
    }

    fn attach(
        registry: &DaemonRegistry,
        host: &HostId,
        instance: DaemonInstanceId,
        capacity: usize,
    ) -> (String, mpsc::Receiver<ConnectionControl>) {
        let (commands, receive) = mpsc::channel(capacity);
        let (close, _close_receive) = watch::channel(None);
        let connection = registry.reserve(host.as_str(), commands, close).unwrap();
        assert!(
            registry
                .attach(
                    host.as_str(),
                    &connection,
                    AttachedDaemon {
                        daemon_instance: instance,
                        host_name: host.as_str().to_owned(),
                    },
                )
                .unwrap()
        );
        (connection, receive)
    }

    fn plane(registry: &DaemonRegistry) -> OwningHostDataPlane {
        OwningHostDataPlane {
            registry: registry.clone(),
        }
    }

    fn adapter(
        record: Option<ComputerRecord>,
        registry: &DaemonRegistry,
    ) -> ProductionScopeRuntimeStop<StopComputerRead> {
        ProductionScopeRuntimeStop::new(
            StopComputerRead {
                record,
                fails: false,
            },
            plane(registry),
            Duration::from_secs(10),
        )
    }

    async fn receive_runtime_stop(
        receive: &mut mpsc::Receiver<ConnectionControl>,
    ) -> CommandRequest {
        let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
            panic!("runtime stop must enter the command FIFO")
        };
        assert_eq!(command.kind, IssuedDaemonCommandKind::RuntimeStop);
        command
    }

    fn success() -> Value {
        json!({ "mode": "hard", "stopped": true, "exit_code": 0 })
    }

    #[tokio::test]
    async fn exact_host_scope_and_generation_reach_only_the_captured_hosts_fifo() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let host_a = HostId::parse("host-a").unwrap();
        let host_b = HostId::parse("host-b").unwrap();
        let instance = DaemonInstanceId::parse("same-instance").unwrap();
        let (_connection_a, mut receive_a) = attach(&registry, &host_a, instance.clone(), 4);
        let (_connection_b, mut receive_b) = attach(&registry, &host_b, instance, 4);
        let stop = adapter(Some(computer("host-a", Some(1), Some(2), None)), &registry);
        let exact = target(
            "host-a",
            RuntimeScope::ConversationSlot {
                agent: AgentId::parse("agent-a").unwrap(),
                source: ConversationId::parse("conversation-a").unwrap(),
            },
            17,
        );

        let call = tokio::spawn(async move { stop.hard_stop(&exact).await });
        let command = receive_runtime_stop(&mut receive_a).await;
        assert_eq!(
            command.params,
            json!({
                "runtime_scope": {
                    "kind": "conversation",
                    "conversation_id": "conversation-a",
                    "agent_id": "agent-a",
                },
                "runtime_generation": 17,
                "mode": "hard",
            })
        );
        assert!(matches!(
            receive_b.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        command.response.send(Ok(success())).unwrap();
        assert_eq!(call.await.unwrap().unwrap(), Ok(ScopeRuntimeStopOutcome));
    }

    #[tokio::test]
    async fn agent_slot_keeps_max_safe_generation_exact_on_the_wire() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let host = HostId::parse("host-a").unwrap();
        let (_connection, mut receive) = attach(
            &registry,
            &host,
            DaemonInstanceId::parse("instance-a").unwrap(),
            4,
        );
        let stop = adapter(Some(computer("host-a", Some(1), Some(2), None)), &registry);
        let exact = target(
            "host-a",
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            JsSafeInteger::MAX,
        );

        let call = tokio::spawn(async move { stop.hard_stop(&exact).await });
        let command = receive_runtime_stop(&mut receive).await;
        assert_eq!(
            command.params,
            json!({
                "runtime_scope": { "kind": "agent", "agent_id": "agent-a" },
                "runtime_generation": JsSafeInteger::MAX,
                "mode": "hard",
            })
        );
        command.response.send(Ok(success())).unwrap();
        assert_eq!(call.await.unwrap().unwrap(), Ok(ScopeRuntimeStopOutcome));
    }

    #[tokio::test]
    async fn production_composition_binds_the_sqlite_host_facts_to_its_own_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scope-runtime-stop.sqlite");
        let mut database = crate::current_schema::create_current_schema(&path).await;
        sqlx::query(
            "INSERT INTO users (id, name, display_name, created_at, updated_at)
             VALUES ('owner-1', 'owner-1', 'Owner', 1, 1)",
        )
        .execute(&mut database)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO hosts
                (id, name, kind, created_at, user_id, activated_at, finalized_at,
                 provisioning_source)
             VALUES ('host-a', 'host-a', 'remote', 1, 'owner-1', 2, 3, 'user_invite')",
        )
        .execute(&mut database)
        .await
        .unwrap();
        database.close().await.unwrap();

        let event_hub = ScopeEventHub::production().unwrap();
        let runtime = DaemonDeliveryRuntime::new(
            RuntimeCredentialAuthority::production(
                SqliteRuntimeCustodyStore::open(&path).await.unwrap(),
            ),
            SqliteRuntimeDeliveryStore::open(&path).await.unwrap(),
            SqliteHostRuntimeStore::open(&path).await.unwrap(),
            DaemonDeliveryConfig::production(),
            event_hub.runtime_activity_publisher(),
            event_hub.agent_presence_publisher(),
            event_hub.runtime_status_publisher(),
        )
        .unwrap();
        let host = HostId::parse("host-a").unwrap();
        let (_connection, mut receive) = attach(
            runtime.transport_registry_for_test(),
            &host,
            DaemonInstanceId::parse("instance-a").unwrap(),
            4,
        );
        let stop = runtime.scope_runtime_stop();
        let exact = target(
            "host-a",
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            29,
        );

        let call = tokio::spawn(async move { stop.hard_stop(&exact).await });
        let command = receive_runtime_stop(&mut receive).await;
        assert_eq!(command.params["runtime_generation"], 29);
        command.response.send(Ok(success())).unwrap();
        assert_eq!(call.await.unwrap().unwrap(), Ok(ScopeRuntimeStopOutcome));
    }

    #[tokio::test]
    async fn same_instance_successor_cannot_receive_a_stop_captured_for_its_predecessor() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let host = HostId::parse("host-a").unwrap();
        let instance = DaemonInstanceId::parse("instance-shared").unwrap();
        let (connection_a, _receive_a) = attach(&registry, &host, instance.clone(), 4);
        let data_plane = plane(&registry);
        let captured_a = data_plane.resolve(&host).unwrap().unwrap();
        let command = RuntimeStopCommand::encode(
            &RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(31)
                .unwrap(),
        )
        .unwrap();

        registry.remove(host.as_str(), &connection_a);
        let (_connection_b, mut receive_b) = attach(&registry, &host, instance, 4);
        let outcome =
            stop_captured(&data_plane, captured_a, command, Duration::from_secs(10)).await;

        assert_eq!(
            outcome.unwrap(),
            Err(ScopeRuntimeStopRefusal::HostAttachPending)
        );
        assert!(matches!(
            receive_b.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn persisted_lifecycle_refusals_short_circuit_before_the_registry() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let poisoned = Arc::clone(&registry.state);
        let _ = thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison registry for the discriminator");
        })
        .join();
        let exact = target(
            "host-a",
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            1,
        );

        for (record, expected) in [
            (None, ScopeRuntimeStopRefusal::HostMissing),
            (
                Some(computer("host-a", None, None, None)),
                ScopeRuntimeStopRefusal::HostPendingAttach,
            ),
            (
                Some(computer("host-a", Some(1), None, None)),
                ScopeRuntimeStopRefusal::HostPendingAttach,
            ),
            (
                Some(computer("host-a", Some(1), Some(2), Some(3))),
                ScopeRuntimeStopRefusal::HostRevoked,
            ),
        ] {
            assert_eq!(
                adapter(record, &registry).hard_stop(&exact).await.unwrap(),
                Err(expected)
            );
        }

        assert!(matches!(
            adapter(Some(computer("host-a", Some(1), Some(2), None)), &registry)
                .hard_stop(&exact)
                .await,
            Err(ScopeRuntimeStopFailure::RegistryUnavailable)
        ));
    }

    #[tokio::test]
    async fn active_without_attachment_is_the_only_live_placement_refusal() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let exact = target(
            "host-a",
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            1,
        );
        assert_eq!(
            adapter(Some(computer("host-a", Some(1), Some(2), None)), &registry)
                .hard_stop(&exact)
                .await
                .unwrap(),
            Err(ScopeRuntimeStopRefusal::HostAttachPending)
        );
    }

    #[tokio::test]
    async fn zero_and_max_safe_plus_one_fail_before_registry_and_fifo() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let host = HostId::parse("host-a").unwrap();
        let (_connection, mut receive) = attach(
            &registry,
            &host,
            DaemonInstanceId::parse("instance-a").unwrap(),
            4,
        );
        let poisoned = Arc::clone(&registry.state);
        let _ = thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison registry after attachment for the generation discriminator");
        })
        .join();

        for generation in [0, JsSafeInteger::MAX + 1] {
            let exact = target(
                "host-a",
                RuntimeScope::AgentSlot {
                    agent: AgentId::parse("agent-a").unwrap(),
                },
                generation,
            );
            assert!(matches!(
                adapter(
                    Some(computer("host-a", Some(1), Some(2), None)),
                    &registry
                )
                .hard_stop(&exact)
                .await,
                Err(ScopeRuntimeStopFailure::GenerationOutOfWireRange {
                    generation: actual
                }) if actual == generation
            ));
            assert!(matches!(
                receive.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
    }

    #[tokio::test]
    async fn returned_host_facts_are_bound_to_the_requested_host_before_dispatch() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let host = HostId::parse("host-a").unwrap();
        let (_connection, mut receive) = attach(
            &registry,
            &host,
            DaemonInstanceId::parse("instance-a").unwrap(),
            4,
        );
        let exact = target(
            "host-a",
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            1,
        );
        let outcome = adapter(Some(computer("host-b", Some(1), Some(2), None)), &registry)
            .hard_stop(&exact)
            .await;

        assert!(matches!(
            outcome,
            Err(ScopeRuntimeStopFailure::HostFactsMismatch { expected, actual })
                if expected.as_str() == "host-a" && actual.as_str() == "host-b"
        ));
        assert!(matches!(
            receive.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn queue_saturation_is_a_hard_not_dispatched_failure() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let host = HostId::parse("host-a").unwrap();
        let (_connection, _receive) = attach(
            &registry,
            &host,
            DaemonInstanceId::parse("instance-a").unwrap(),
            1,
        );
        let attachment = registry.attachment(host.as_str()).unwrap().unwrap();
        let (response, _receive_response) = oneshot::channel();
        attachment
            .commands
            .try_send(ConnectionControl::Command(CommandRequest {
                kind: IssuedDaemonCommandKind::HostStatus,
                params: json!({}),
                capability_refusal: command::rejects_every_capability_refusal,
                deadline: Instant::now() + Duration::from_secs(60),
                response,
            }))
            .unwrap();
        let exact = target(
            "host-a",
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            1,
        );

        assert!(matches!(
            adapter(Some(computer("host-a", Some(1), Some(2), None)), &registry)
                .hard_stop(&exact)
                .await,
            Err(ScopeRuntimeStopFailure::NotDispatched)
        ));
    }

    #[tokio::test]
    async fn daemon_refusal_ambiguous_completion_and_bad_response_are_hard_faults() {
        #[derive(Clone, Copy)]
        enum Answer {
            Refused,
            Drop,
            Invalid,
        }

        for (suffix, answer) in [
            ("refused", Answer::Refused),
            ("drop", Answer::Drop),
            ("invalid", Answer::Invalid),
        ] {
            let registry = DaemonRegistry::with_process_epoch(format!("test-epoch-{suffix}"));
            let host = HostId::parse(format!("host-{suffix}")).unwrap();
            let (_connection, mut receive) = attach(
                &registry,
                &host,
                DaemonInstanceId::parse(format!("instance-{suffix}")).unwrap(),
                4,
            );
            let stop = adapter(
                Some(computer(host.as_str(), Some(1), Some(2), None)),
                &registry,
            );
            let exact = target(
                host.as_str(),
                RuntimeScope::AgentSlot {
                    agent: AgentId::parse("agent-a").unwrap(),
                },
                1,
            );
            let call = tokio::spawn(async move { stop.hard_stop(&exact).await });
            let command = receive_runtime_stop(&mut receive).await;
            match answer {
                Answer::Refused => command
                    .response
                    .send(Err(CommandDispatchError::DaemonRefused(
                        CommandRefusal::Response(DaemonRefusalCode::ShuttingDown),
                    )))
                    .unwrap(),
                Answer::Drop => drop(command.response),
                Answer::Invalid => command
                    .response
                    .send(Ok(json!({
                        "mode": "hard",
                        "stopped": true,
                        "exit_code": 0,
                        "extra": true,
                    })))
                    .unwrap(),
            }
            let outcome = call.await.unwrap();
            assert!(matches!(
                (answer, outcome),
                (Answer::Refused, Err(ScopeRuntimeStopFailure::DaemonRefused))
                    | (Answer::Drop, Err(ScopeRuntimeStopFailure::Ambiguous))
                    | (
                        Answer::Invalid,
                        Err(ScopeRuntimeStopFailure::ProtocolCorruption)
                    )
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_is_a_hard_fault_not_a_placement_refusal() {
        let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
        let host = HostId::parse("host-a").unwrap();
        let (_connection, mut receive) = attach(
            &registry,
            &host,
            DaemonInstanceId::parse("instance-a").unwrap(),
            4,
        );
        let stop = ProductionScopeRuntimeStop::new(
            StopComputerRead {
                record: Some(computer("host-a", Some(1), Some(2), None)),
                fails: false,
            },
            plane(&registry),
            Duration::from_secs(5),
        );
        let exact = target(
            "host-a",
            RuntimeScope::AgentSlot {
                agent: AgentId::parse("agent-a").unwrap(),
            },
            1,
        );
        let call = tokio::spawn(async move { stop.hard_stop(&exact).await });
        let _held_response = receive_runtime_stop(&mut receive).await.response;
        tokio::time::advance(Duration::from_secs(5)).await;

        assert!(matches!(
            call.await.unwrap(),
            Err(ScopeRuntimeStopFailure::TimedOut)
        ));
    }
}

#[tokio::test]
async fn disconnect_outranks_a_saturated_command_fifo_and_stays_generation_bound() {
    let registry = DaemonRegistry::with_process_epoch("test-epoch".to_owned());
    let (old_sender, mut old_receiver) = mpsc::channel(1);
    let (old_close, mut old_close_receiver) = watch::channel(None);
    let old_id = registry
        .reserve("host-1", old_sender.clone(), old_close)
        .unwrap();
    let (blocked_response, _blocked_receive) = oneshot::channel();
    old_sender
        .try_send(ConnectionControl::Command(CommandRequest {
            kind: IssuedDaemonCommandKind::HostStatus,
            params: json!({}),
            capability_refusal: command::rejects_every_capability_refusal,
            deadline: Instant::now() + DEFAULT_COMMAND_TIMEOUT,
            response: blocked_response,
        }))
        .unwrap();

    let (other_sender, _) = mpsc::channel(1);
    let (other_close, other_close_receiver) = watch::channel(None);
    registry
        .reserve("host-2", other_sender, other_close)
        .unwrap();

    // This is the real production port. Its completion cannot depend on space in
    // the ordinary FIFO, which is full and deliberately has no active receiver.
    let control = DaemonConnectionControlHandle::new(DaemonConnectionAuthority {
        registry: registry.clone(),
    });
    tokio::time::timeout(
        Duration::from_millis(50),
        control.disconnect(HostId::parse("host-1").unwrap()),
    )
    .await
    .expect("disconnect must not wait for ordinary command capacity")
    .unwrap();

    // The old socket ends and a replacement reserves before the captured close is
    // observed. The close is still on the old generation's private channel.
    registry.remove("host-1", &old_id);
    let (replacement_sender, _) = mpsc::channel(1);
    let (replacement_close, replacement_close_receiver) = watch::channel(None);
    let replacement_id = registry
        .reserve("host-1", replacement_sender, replacement_close)
        .unwrap();

    // Both lanes are ready when the stalled receiver resumes. The production
    // arbitration must select close first and leave the queued command untouched.
    assert!(matches!(
        next_connection_input(&mut old_close_receiver, &old_id, &mut old_receiver).await,
        ConnectionInput::Close(ConnectionClose {
            expected_connection_id: Some(ref expected),
            reason: "host_revoked",
        }) if expected == &old_id
    ));
    assert!(matches!(
        old_receiver.try_recv(),
        Ok(ConnectionControl::Command(_))
    ));
    assert!(replacement_close_receiver.borrow().is_none());
    assert!(other_close_receiver.borrow().is_none());

    // The old task's eventual cleanup is generation-compared as well.
    registry.remove("host-1", &old_id);
    assert_eq!(
        registry.slot_connection_id("host-1").unwrap().as_deref(),
        Some(replacement_id.as_str())
    );
}

/// Wave 3A A2 — load-bearing same-live-owner regression gate for the
/// `/api/channels/{id}/agent-run-states` door, exercised through the
/// production composition seam.
///
/// The composed `prepare()` E2E proves the route is mounted and, against a
/// fresh owner, reads an empty run-state map. That alone cannot show the
/// reader shares the *specific* live owner the daemon writes into. Nor could
/// a component gate that hand-wires the correct `.clone()` into the raw
/// `router` factory: it re-proves the mechanism but never touches the line
/// that chooses the reader owner, so a `prepare()` edit that forked the
/// reader onto a fresh owner would leave it green.
///
/// This gate binds to that line by routing through the seam `prepare()` uses:
/// `DaemonDeliveryRuntime::agent_run_states_router`. The seam takes no owner
/// argument — it binds the reader to the runtime it is called on by
/// construction — so the fork `prepare()` could once commit (a second-arg
/// swap) is now unrepresentable, and the only surviving choice is which
/// runtime the seam is handed. The daemon reconnection hydration path
/// (`events.hydrate`, the sole non-WebSocket writer of the live run-state
/// map) records a non-empty run-state into one runtime's owner; the seam on
/// that runtime reads the precise row back. The seam on an independently
/// constructed runtime is empty — so binding the door to any runtime but the
/// delivery one turns the assertion red.
mod run_states_same_owner {
    use std::{convert::Infallible, path::Path};

    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Number, Value};
    use silverhand_backend_core::{
        conversation_messaging::messaging::{
            collaboration::{
                channel_directory::{ChannelStatus, ChannelVisibility, DirectoryRoster},
                channel_lifecycle::{
                    ChannelDetailFact, ChannelDetailReadPort, ChannelDetailRefusal,
                    ChannelLifecycleRow,
                },
            },
            conversation_title::CanonicalConversationTitle,
        },
        foundation::identity::{AgentId, ConversationId, DaemonInstanceId, UserId, WorkspaceId},
        host_runtime::runtime_credential::{
            RuntimeCustody, RuntimeGeneration, RuntimePrincipal, RuntimeScope,
        },
        identity_access::access::WorkspaceRole,
    };
    use silverhand_backend_sqlite::{
        conversation_messaging::runtime_delivery::SqliteRuntimeDeliveryStore,
        host_runtime::SqliteHostRuntimeStore,
        host_runtime::runtime_credential::SqliteRuntimeCustodyStore,
    };
    use sqlx::Connection;
    use tower::ServiceExt;

    use crate::{
        application_orchestration::daemon_delivery::{DaemonDeliveryConfig, DaemonDeliveryRuntime},
        current_schema,
        foundation::scope_event_transport::ScopeEventHub,
        host_runtime::runtime_credential::RuntimeCredentialAuthority,
        host_runtime::runtime_event_settlement::{
            AgentRunStateWire, AgentRunStatus, HydratedRuntime,
        },
    };

    const CONVERSATION: &str = "conversation0001";
    const AGENT: &str = "agent-1";

    /// Admits the viewer for the requested conversation so the route reaches
    /// its snapshot source; the same-owner claim under test lives entirely in
    /// that source, never in admission.
    #[derive(Clone)]
    struct AdmitViewer;

    impl ChannelDetailReadPort for AdmitViewer {
        type Error = Infallible;

        async fn detail(
            &self,
            viewer: &UserId,
            conversation: &ConversationId,
        ) -> Result<Result<ChannelDetailFact, ChannelDetailRefusal>, Self::Error> {
            Ok(Ok(ChannelDetailFact::projected(
                viewer.clone(),
                ChannelLifecycleRow::workspace_placed(
                    conversation.clone(),
                    WorkspaceId::parse("w").unwrap(),
                    DirectoryRoster::Open,
                    CanonicalConversationTitle::parse("General").unwrap(),
                    ChannelStatus::Active,
                    None,
                    None,
                    None,
                    None,
                    0,
                    1,
                    1,
                    ChannelVisibility::Private,
                )
                .unwrap(),
                true,
                Some(WorkspaceRole::Member),
                1,
            )
            .unwrap()))
        }
    }

    /// A fresh production runtime. Every construction builds its own live
    /// owner (`RuntimeEventSettlement::new`), so two calls against the same
    /// database are two independent owners — the falsifier's second owner.
    async fn build_runtime(
        path: &Path,
    ) -> DaemonDeliveryRuntime<
        SqliteRuntimeCustodyStore,
        SqliteRuntimeDeliveryStore,
        SqliteHostRuntimeStore,
    > {
        let custody = SqliteRuntimeCustodyStore::open(path).await.unwrap();
        let data = SqliteRuntimeDeliveryStore::open(path).await.unwrap();
        let hosts = SqliteHostRuntimeStore::open(path).await.unwrap();
        let event_hub = ScopeEventHub::production().unwrap();
        DaemonDeliveryRuntime::new(
            RuntimeCredentialAuthority::production(custody),
            data,
            hosts,
            DaemonDeliveryConfig::production(),
            event_hub.runtime_activity_publisher(),
            event_hub.agent_presence_publisher(),
            event_hub.runtime_status_publisher(),
        )
        .unwrap()
    }

    async fn read_states(
        runtime: DaemonDeliveryRuntime<
            SqliteRuntimeCustodyStore,
            SqliteRuntimeDeliveryStore,
            SqliteHostRuntimeStore,
        >,
    ) -> (u16, Value) {
        let mut request = Request::builder()
            .uri(format!("/api/channels/{CONVERSATION}/agent-run-states"))
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(UserId::parse("viewer").unwrap());
        // Exercise the PRODUCTION seam `prepare()` mounts. The seam takes no
        // owner argument: it binds the reader to `runtime`'s own owner by
        // construction. So the positive/negative difference below is *which
        // runtime the seam is handed* — the exact choice `prepare()` makes at
        // its `.merge(daemon_runtime.agent_run_states_router(..))` line. There
        // is no second-parameter fork for that line to get wrong.
        let response = runtime
            .agent_run_states_router(AdmitViewer)
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn the_door_reads_the_delivery_owner_and_a_second_owner_is_red() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run-states-same-owner.sqlite");
        current_schema::create_current_schema(&path)
            .await
            .close()
            .await
            .unwrap();

        let delivery_owner = build_runtime(&path).await;
        // The daemon reconnection hydration path writes a live run-state into
        // this owner's `Arc<Mutex<..>>` map — the very map delivery effects
        // mutate. No storage row backs it; run-state is process memory.
        let principal = RuntimePrincipal::for_scope(
            RuntimeScope::ConversationSlot {
                agent: AgentId::parse(AGENT).unwrap(),
                source: ConversationId::parse(CONVERSATION).unwrap(),
            },
            RuntimeCustody::new(
                RuntimeGeneration::new(1).unwrap(),
                DaemonInstanceId::parse("daemon-1").unwrap(),
            ),
        );
        let wire = AgentRunStateWire {
            run_id: "run-seeded".to_owned(),
            agent_id: AGENT.to_owned(),
            conversation_id: CONVERSATION.to_owned(),
            status: AgentRunStatus::Thinking,
            started_at: Number::from(1),
            updated_at: Number::from(2),
            last_activity_at: Number::from(2),
            terminal_reason: None,
            activity_snippet: None,
            activity_snippet_kind: None,
            tool_call_counts: None,
            compacting: None,
            compacting_since: None,
            compaction_stale: None,
            detail: None,
        };
        delivery_owner
            .hydrate_run_states_for_test(
                "host-1",
                principal.clone().custody().daemon_instance(),
                "connection-test",
                [principal.agent().clone()].into_iter().collect(),
                vec![HydratedRuntime {
                    principal,
                    run_state: Some(wire),
                }],
            )
            .await
            .unwrap();

        // Positive: hand the seeded runtime to the same seam `prepare()`
        // uses. The seam binds this runtime's owner (a share of the seeded
        // `Arc<Mutex<..>>` map) as the reader, so the route reads the exact
        // seeded row — the same-live-owner contract, proven at the seam.
        let (status, body) = read_states(delivery_owner.clone()).await;
        assert_eq!(status, 200);
        let states = body["states"].as_array().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0]["runId"], "run-seeded");
        assert_eq!(states[0]["agentId"], AGENT);
        assert_eq!(states[0]["conversationId"], CONVERSATION);

        // Falsifier: hand the seam an independently constructed runtime — a
        // different live owner. Same admission, same seam, same request; the
        // seeded row is gone. This is the only fork the seam still permits
        // (a whole second runtime, not a silent second-arg swap), and it is
        // red — so binding the door to any runtime but delivery's fails here.
        let (status, body) = read_states(build_runtime(&path).await).await;
        assert_eq!(status, 200);
        assert!(body["states"].as_array().unwrap().is_empty());
    }
}

struct NoReferences;

impl ReferencePositions for NoReferences {
    fn autolink_spans(&self, _content: &MessageContent) -> Vec<Range<usize>> {
        Vec::new()
    }
}

fn ready() -> Value {
    json!({
        "direction": "daemon_to_server",
        "type": "event",
        "kind": "ready",
        "protocol_version": OUTBOUND_PROTOCOL_VERSION,
        "daemon_version": "test",
        "daemon_id": "daemon-test",
        "instance_id": "instance-test",
        "host_descriptor": { "name": "Test host", "kind": "remote" },
        "capabilities": [],
        "daemon_pid": 42,
        "started_at": 100,
    })
}

fn daemon_ingress_is_valid(raw: &str) -> bool {
    match parse_daemon_frame(raw) {
        Ok(DaemonFrame::Runtime { kind, value }) => {
            crate::host_runtime::daemon_runtime_owner::runtime_inbound_is_valid_for_test(
                &kind, &value,
            )
        }
        Ok(_) => true,
        Err(_) => false,
    }
}

#[test]
fn ready_and_command_response_frames_are_strict() {
    assert!(matches!(
        parse_daemon_frame(&ready().to_string()),
        Ok(DaemonFrame::Ready(_))
    ));
    let mut widened = ready();
    widened["unexpected"] = json!(true);
    assert!(parse_daemon_frame(&widened.to_string()).is_err());

    let ambiguous_response = json!({
        "direction": "daemon_to_server",
        "type": "command_response",
        "id": "one",
        "kind": "runtime_status",
        "result": { "runtimes": [] },
        "error": { "code": "internal_error", "message": "hidden" },
    });
    assert!(parse_daemon_frame(&ambiguous_response.to_string()).is_err());
}

#[test]
fn the_offered_version_is_read_from_the_envelope_the_strict_schema_rejects() {
    // The population this probe exists for: a daemon old enough that its `ready`
    // frame predates `instance_id`. The strict schema refuses it, so a mismatch
    // decided after the parse would never see the build a human must upgrade.
    let mut old_schema = ready();
    old_schema["protocol_version"] = json!(OUTBOUND_PROTOCOL_VERSION - 1);
    old_schema
        .as_object_mut()
        .unwrap()
        .remove("instance_id")
        .unwrap();
    let old_schema = old_schema.to_string();
    assert!(parse_daemon_frame(&old_schema).is_err());
    assert_eq!(
        offered_ready_protocol_version(&old_schema),
        Some(OfferedReadyVersion::Readable(OUTBOUND_PROTOCOL_VERSION - 1))
    );

    // Our own version is the strict schema's business, not this probe's.
    assert_eq!(
        offered_ready_protocol_version(&ready().to_string()),
        Some(OfferedReadyVersion::Readable(OUTBOUND_PROTOCOL_VERSION))
    );

    // Out-of-range versions remain observable as invalid without admission state.
    let mut unreadable = ready();
    unreadable["protocol_version"] = json!(MAX_READABLE_PROTOCOL_VERSION + 1);
    assert_eq!(
        offered_ready_protocol_version(&unreadable.to_string()),
        Some(OfferedReadyVersion::Invalid)
    );
    assert!(matches!(
        parse_daemon_frame(&unreadable.to_string()),
        Ok(DaemonFrame::Ready(_))
    ));

    for unreadable_version in [json!(-1), json!(10.5), json!("10"), json!(null)] {
        let mut frame = ready();
        frame["protocol_version"] = unreadable_version.clone();
        assert_eq!(
            offered_ready_protocol_version(&frame.to_string()),
            Some(OfferedReadyVersion::Invalid),
            "{unreadable_version} is not a readable protocol version"
        );
    }

    let mut absent = ready();
    absent.as_object_mut().unwrap().remove("protocol_version");
    assert_eq!(
        offered_ready_protocol_version(&absent.to_string()),
        Some(OfferedReadyVersion::Invalid)
    );

    // Not a `ready` envelope at all, and not JSON at all.
    let mut other_kind = ready();
    other_kind["kind"] = json!("shutting_down");
    assert_eq!(
        offered_ready_protocol_version(&other_kind.to_string()),
        None
    );
    let mut other_direction = ready();
    other_direction["direction"] = json!("server_to_daemon");
    assert_eq!(
        offered_ready_protocol_version(&other_direction.to_string()),
        None
    );
    assert_eq!(offered_ready_protocol_version("not json"), None);
}

#[test]
fn refusal_families_admit_exact_literal_codes_and_strict_error_objects() {
    let families: [(CapabilityRefusalAdmission, &[&str]); 9] = [
        (command::rejects_every_capability_refusal, &[]),
        (crate::host_runtime::runtime_control::admits_runtime_refusal,
            &["agent_deleting", "workspace_not_provisioned", "workspace_bundle_missing", "registry_unavailable",
              "runtime_binary_missing", "runtime_version_unsupported"]),
        (crate::host_runtime::agent_materialize::admits_agent_materialize_refusal,
            &["agent_deleting", "fs_quota_exceeded", "fs_unavailable", "agent_dir_collision", "unsupported_template", "unsupported_runtime"]),
        (crate::identity_access::agent_profile_mcp_data_plane::admits_profile_mcp_read_refusal,
            &["fs_unavailable"]),
        (crate::identity_access::agent_profile_mcp_data_plane::admits_profile_mcp_write_refusal,
            &["fs_unavailable", "invalid_config", "agent_deleting"]),
        (crate::identity_access::agent_profile_mcp_data_plane::admits_mcp_probe_refusal,
            &["fs_unavailable", "agent_deleting"]),
        (crate::workspace_data_plane::admits_workspace_fs_refusal,
            &["fs_unavailable", "registry_unavailable", "forbidden_path", "not_found"]),
        (crate::identity_access::agent_workspace_index_probe::admits_agent_workspace_index_refusal,
            &["registry_unavailable"]),
        (crate::host_runtime::cascade_effects::admits_agent_files_purge_refusal,
            &["quiesce_failed", "partial_purge"]),
    ];
    let capability_universe = families
        .iter()
        .flat_map(|(_, codes)| codes.iter().copied())
        .chain(["banana"])
        .collect::<std::collections::BTreeSet<_>>();
    let error = |code: &str| json!({ "code": code, "message": "why" });
    for (admission, expected) in families {
        for (raw, code) in [
            ("unknown_intent", DaemonRefusalCode::UnknownIntent),
            ("invalid_params", DaemonRefusalCode::InvalidParams),
            ("internal_error", DaemonRefusalCode::InternalError),
            ("shutting_down", DaemonRefusalCode::ShuttingDown),
            ("request_too_large", DaemonRefusalCode::RequestTooLarge),
        ] {
            assert_eq!(
                CommandRefusal::decode(admission, &error(raw)),
                Some(CommandRefusal::Response(code))
            );
        }
        for raw in &capability_universe {
            let decoded = CommandRefusal::decode(admission, &error(raw));
            if expected.contains(raw) {
                let Some(CommandRefusal::Capability(refusal)) = decoded else {
                    panic!("legitimate capability code rejected: {raw}");
                };
                assert_eq!(refusal.code(), *raw);
                assert_eq!(refusal.message(), "why");
            } else {
                assert_eq!(decoded, None, "foreign code admitted: {raw}");
            }
        }
    }
    for malformed in [
        json!({"code":"internal_error"}),
        json!({"code":"internal_error", "message":7}),
        json!({"code":"internal_error", "message":"why", "extra":true}),
        json!("why"),
    ] {
        assert_eq!(
            CommandRefusal::decode(
                crate::host_runtime::runtime_control::admits_runtime_refusal,
                &malformed
            ),
            None
        );
    }
}

#[test]
fn runtime_stop_refusal_vocabulary_stays_on_the_hard_fault_side() {
    let error = |code: &str| json!({ "code": code, "message": "why" });
    let response = CommandRefusal::decode(
        command::rejects_every_capability_refusal,
        &error("shutting_down"),
    )
    .expect("response-level refusal is admitted for runtime_stop");
    assert!(matches!(
        HostDataPlaneFailure::from_dispatch(CommandDispatchError::DaemonRefused(response)),
        HostDataPlaneFailure::Refused(CommandRefusal::Response(DaemonRefusalCode::ShuttingDown,))
    ));

    // runtime_stop has no operation-scoped refusal arm. The real connection
    // loop turns this failed decode into InvalidResponse; the shared data plane
    // then preserves it as protocol corruption, never a placement refusal.
    assert_eq!(
        CommandRefusal::decode(
            command::rejects_every_capability_refusal,
            &error("agent_deleting"),
        ),
        None
    );
    assert!(matches!(
        HostDataPlaneFailure::from_dispatch(CommandDispatchError::InvalidResponse),
        HostDataPlaneFailure::ProtocolCorruption
    ));
}

#[test]
fn an_answer_opens_only_for_the_coordinate_that_awaited_it() {
    let answer = || HostDataPlaneAnswer {
        host: HostId::parse("host-1").unwrap(),
        instance: DaemonInstanceId::parse("instance-shared").unwrap(),
        connection_id: "connection-current".to_owned(),
        result: json!({ "runtimes": [] }),
    };
    let custody = |host: &str, instance: &str, connection: &str| HostDataPlaneCustody {
        host: HostId::parse(host).unwrap(),
        instance: DaemonInstanceId::parse(instance).unwrap(),
        connection_id: connection.to_owned(),
    };

    assert_eq!(
        answer().into_result_for(&custody("host-1", "instance-shared", "connection-current")),
        Ok(json!({ "runtimes": [] }))
    );
    // Both machines spell the custody the same way, so an instance-only bind
    // would file this answer under the wrong host.
    assert_eq!(
        answer().into_result_for(&custody("host-2", "instance-shared", "connection-current")),
        Err(HostDataPlaneAnswerMismatch::Host)
    );
    assert_eq!(
        answer().into_result_for(&custody("host-1", "instance-shared", "connection-old")),
        Err(HostDataPlaneAnswerMismatch::ConnectionGeneration)
    );
    assert_eq!(
        answer().into_result_for(&custody("host-1", "instance-old", "connection-current")),
        Err(HostDataPlaneAnswerMismatch::DaemonInstance)
    );
    assert_eq!(
        answer().into_result_for(&custody("host-2", "instance-old", "connection-old")),
        Err(HostDataPlaneAnswerMismatch::Host)
    );
}

#[test]
fn every_issued_word_is_admitted_on_the_response_epoch() {
    for kind in IssuedDaemonCommandKind::ALL {
        let wire = kind.as_wire();
        assert!(valid_command_response_envelope(&json!({
            "id": "r1",
            "kind": wire,
            "result": { "ok": true },
        })));
    }
}

#[tokio::test]
async fn materialize_command_stays_on_its_captured_host_and_connection_generation() {
    use silverhand_backend_core::{
        foundation::identity::WorkspaceId,
        host_runtime::agent_materialize::{
            AgentMaterializePort, AgentMaterializeTarget, AgentMaterializeTemplate,
        },
        identity_access::agents::runtime::RuntimeKind,
    };

    let registry = DaemonRegistry::with_process_epoch("materialize-test".to_owned());
    let host_a = HostId::parse("host-a").unwrap();
    let host_b = HostId::parse("host-b").unwrap();
    let instance = DaemonInstanceId::parse("same-instance").unwrap();
    let (connection_a, mut receive_a) =
        attach_for_command_test(&registry, &host_a, instance.clone());
    let (_connection_b, mut receive_b) =
        attach_for_command_test(&registry, &host_b, instance.clone());
    let plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let adapter = crate::host_runtime::agent_materialize::ProductionAgentMaterialize::new(
        plane.clone(),
        Duration::from_secs(10),
    );
    let target = AgentMaterializeTarget::reserved(
        host_a.clone(),
        AgentId::parse("agent-a").unwrap(),
        WorkspaceId::parse("wsah_agent-a").unwrap(),
        "Agent A".to_owned(),
        RuntimeKind::Codex,
        None,
        None,
        AgentMaterializeTemplate::Default,
    );
    let call_target = target.clone();
    let captured = adapter
        .capture(&host_a)
        .await
        .unwrap()
        .expect("host A is attached at capture time");
    let call = tokio::spawn(async move { adapter.materialize(captured, &call_target).await });
    let ConnectionControl::Command(command) = receive_a.recv().await.unwrap() else {
        panic!("materialize must enter host A FIFO")
    };
    assert_eq!(command.kind, IssuedDaemonCommandKind::AgentMaterialize);
    assert_eq!(command.params["agent_id"], "agent-a");
    assert!(matches!(
        receive_b.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    command
        .response
        .send(Ok(json!({
            "ok":true,"agent_dir":"/agent-a",
            "agent_home_workspace_path":"/agent-a","materialized_at":1
        })))
        .unwrap();
    assert!(call.await.unwrap().unwrap().is_ok());

    let captured_a = plane.resolve(&host_a).unwrap().unwrap();
    registry.remove(host_a.as_str(), &connection_a);
    let (_successor_connection, mut successor_receive) =
        attach_for_command_test(&registry, &host_a, instance);
    let params =
        crate::application_orchestration::daemon_materialize_codec::encode(&target).unwrap();
    assert!(matches!(
        crate::host_runtime::agent_materialize::materialize_captured(
            &plane,
            captured_a,
            params,
            Duration::from_secs(10),
        )
        .await
        .unwrap(),
        Err(silverhand_backend_core::host_runtime::agent_materialize::AgentMaterializeRefusal::HostAttachPending)
    ));
    assert!(matches!(
        successor_receive.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[derive(Clone)]
struct McpPlacementRead {
    fact: Option<
        silverhand_backend_core::identity_access::agent_profile_mcp::AgentProfileMcpPlacement,
    >,
}

impl silverhand_backend_core::identity_access::agent_profile_mcp::AgentProfileMcpPlacementRead
    for McpPlacementRead
{
    async fn read_mcp_placement(
        &self,
        _agent: &AgentId,
    ) -> Result<
        Option<
            silverhand_backend_core::identity_access::agent_profile_mcp::AgentProfileMcpPlacement,
        >,
        PersistenceFailure,
    > {
        Ok(self.fact.clone())
    }
}

fn mcp_placement(
    source: &str,
    host: &str,
    deletion_pending: bool,
) -> silverhand_backend_core::identity_access::agent_profile_mcp::AgentProfileMcpPlacement {
    silverhand_backend_core::identity_access::agent_profile_mcp::AgentProfileMcpPlacement::new(
        AgentId::parse(source).unwrap(),
        HostId::parse(host).unwrap(),
        deletion_pending,
    )
}

#[tokio::test]
async fn profile_mcp_commands_are_typed_host_bound_and_generation_bound() {
    use silverhand_backend_core::identity_access::agent_profile_mcp::{
        AgentProfileMcpDataPlane, McpProbeCommand, McpProbePhase,
    };

    let registry = DaemonRegistry::with_process_epoch("profile-mcp-test".to_owned());
    let host_a = HostId::parse("host-a").unwrap();
    let host_b = HostId::parse("host-b").unwrap();
    let instance = DaemonInstanceId::parse("same-instance").unwrap();
    let (connection_a, mut receive_a) =
        attach_for_command_test(&registry, &host_a, instance.clone());
    let (_connection_b, mut receive_b) =
        attach_for_command_test(&registry, &host_b, instance.clone());
    let plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let adapter = crate::identity_access::agent_profile_mcp_data_plane::ProductionAgentProfileMcpDataPlane::new(
        McpPlacementRead {
            fact: Some(mcp_placement("agent-a", "host-a", false)),
        },
        plane.clone(),
    );
    let agent = AgentId::parse("agent-a").unwrap();

    let read_adapter = adapter.clone();
    let read_agent = agent.clone();
    let read = tokio::spawn(async move { read_adapter.read_config(&read_agent).await });
    let ConnectionControl::Command(command) = receive_a.recv().await.unwrap() else {
        panic!("MCP read must enter host A FIFO")
    };
    assert_eq!(command.kind, IssuedDaemonCommandKind::AgentProfileMcpRead);
    assert_eq!(command.params, json!({ "agent_id": "agent-a" }));
    assert!(matches!(
        receive_b.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    command
        .response
        .send(Ok(json!({ "ok": true, "raw": null })))
        .unwrap();
    assert_eq!(read.await.unwrap().unwrap(), None);

    let raw = "{\n  \"mcpServers\": {\"sky😀\": {\"command\": \"x\"}}\n}\n";
    let write_adapter = adapter.clone();
    let write_agent = agent.clone();
    let write_raw = raw.to_owned();
    let write =
        tokio::spawn(async move { write_adapter.write_config(&write_agent, &write_raw).await });
    let ConnectionControl::Command(command) = receive_a.recv().await.unwrap() else {
        panic!("MCP write must enter host A FIFO")
    };
    assert_eq!(command.kind, IssuedDaemonCommandKind::AgentProfileMcpWrite);
    assert_eq!(command.params["agent_id"], "agent-a");
    assert_eq!(command.params["raw"].as_str(), Some(raw));
    command
        .response
        .send(Ok(json!({ "ok": true, "bytes_written": 61 })))
        .unwrap();
    assert_eq!(write.await.unwrap().unwrap().get(), 61);

    let probe_adapter = adapter.clone();
    let probe_agent = agent.clone();
    let probe = tokio::spawn(async move {
        probe_adapter
            .probe(
                &probe_agent,
                &McpProbeCommand {
                    server_name: "sky".to_owned(),
                    include_tools: true,
                },
            )
            .await
    });
    let ConnectionControl::Command(command) = receive_a.recv().await.unwrap() else {
        panic!("MCP probe must enter host A FIFO")
    };
    assert_eq!(command.kind, IssuedDaemonCommandKind::McpProbe);
    assert_eq!(
        command.params,
        json!({
            "source": { "kind": "profile", "agent_id": "agent-a" },
            "server_name": "sky",
            "include_tools": true,
        })
    );
    command
        .response
        .send(Ok(json!({
            "ok": false,
            "phase": "connect",
            "message": "connection failed"
        })))
        .unwrap();
    let outcome = probe.await.unwrap().unwrap();
    assert!(!outcome.ok);
    assert_eq!(outcome.phase, McpProbePhase::Connect);

    // Admission captured A. A same-instance successor has a different
    // connection generation and must receive none of the old attempt.
    let captured_a = plane.resolve(&host_a).unwrap().unwrap();
    registry.remove(host_a.as_str(), &connection_a);
    let (_successor, mut successor_receive) = attach_for_command_test(&registry, &host_a, instance);
    let failure = crate::identity_access::agent_profile_mcp_data_plane::read_captured(
        &plane,
        captured_a,
        &agent,
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert_eq!(
        failure.code,
        silverhand_backend_core::identity_access::agent_profile_mcp::McpDataPlaneFaultCode::HostAttachPending
    );
    assert!(matches!(
        successor_receive.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn profile_mcp_write_decoder_closes_every_unsafe_byte_count() {
    use silverhand_backend_core::{
        foundation::wire_number::JsSafeInteger,
        identity_access::agent_profile_mcp::{AgentProfileMcpDataPlane, McpDataPlaneFaultCode},
    };

    let registry = DaemonRegistry::with_process_epoch("profile-mcp-number".to_owned());
    let host = HostId::parse("host-a").unwrap();
    let (_connection, mut receive) = attach_for_command_test(
        &registry,
        &host,
        DaemonInstanceId::parse("instance-a").unwrap(),
    );
    let adapter = crate::identity_access::agent_profile_mcp_data_plane::ProductionAgentProfileMcpDataPlane::new(
        McpPlacementRead {
            fact: Some(mcp_placement("agent-a", "host-a", false)),
        },
        OwningHostDataPlane { registry },
    );
    let agent = AgentId::parse("agent-a").unwrap();

    for unsafe_count in [json!(-1), json!(1.5), json!(JsSafeInteger::MAX + 1)] {
        let call_adapter = adapter.clone();
        let call_agent = agent.clone();
        let call = tokio::spawn(async move { call_adapter.write_config(&call_agent, "{}").await });
        let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
            panic!("write must enter FIFO")
        };
        command
            .response
            .send(Ok(json!({ "ok": true, "bytes_written": unsafe_count })))
            .unwrap();
        assert_eq!(
            call.await.unwrap().unwrap_err().code,
            McpDataPlaneFaultCode::DaemonInternalError
        );
    }

    let call_adapter = adapter.clone();
    let call_agent = agent.clone();
    let call = tokio::spawn(async move { call_adapter.write_config(&call_agent, "{}").await });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("write must enter FIFO")
    };
    command
        .response
        .send(Ok(json!({
            "ok": true,
            "bytes_written": JsSafeInteger::MAX
        })))
        .unwrap();
    assert_eq!(call.await.unwrap().unwrap().get(), JsSafeInteger::MAX);
}

#[tokio::test]
async fn profile_mcp_persisted_refusals_bind_source_before_registry_or_deletion() {
    use silverhand_backend_core::identity_access::agent_profile_mcp::{
        AgentProfileMcpDataPlane, McpDataPlaneFaultCode,
    };
    use std::thread;

    let registry = DaemonRegistry::with_process_epoch("profile-mcp-poison".to_owned());
    let poisoned = Arc::clone(&registry.state);
    let _ = thread::spawn(move || {
        let _guard = poisoned.lock().unwrap();
        panic!("poison registry for the pre-transport discriminator");
    })
    .join();
    let plane = OwningHostDataPlane { registry };
    let requested = AgentId::parse("agent-a").unwrap();

    for (fact, expected) in [
        (
            Some(mcp_placement("agent-a", "host-a", true)),
            McpDataPlaneFaultCode::AgentDeleting,
        ),
        (
            Some(mcp_placement("agent-b", "host-a", true)),
            McpDataPlaneFaultCode::DaemonInternalError,
        ),
        (None, McpDataPlaneFaultCode::DaemonInternalError),
    ] {
        let adapter =
            crate::identity_access::agent_profile_mcp_data_plane::ProductionAgentProfileMcpDataPlane::new(
                McpPlacementRead { fact },
                plane.clone(),
            );
        assert_eq!(
            adapter.read_config(&requested).await.unwrap_err().code,
            expected
        );
    }
}

#[tokio::test]
async fn materialize_refuses_a_target_that_names_a_different_host_than_the_capture() {
    use silverhand_backend_core::{
        foundation::identity::WorkspaceId,
        host_runtime::agent_materialize::{
            AgentMaterializePort, AgentMaterializeTarget, AgentMaterializeTemplate,
        },
        identity_access::agents::runtime::RuntimeKind,
    };

    // Both hosts are legitimately attached: A's capture and B's reserved target are each
    // valid on their own. The single custody authority must still refuse the *pairing*
    // before any send, so neither FIFO ever sees a byte.
    let registry = DaemonRegistry::with_process_epoch("materialize-cross-host".to_owned());
    let host_a = HostId::parse("host-a").unwrap();
    let host_b = HostId::parse("host-b").unwrap();
    let instance = DaemonInstanceId::parse("same-instance").unwrap();
    let (_connection_a, mut receive_a) =
        attach_for_command_test(&registry, &host_a, instance.clone());
    let (_connection_b, mut receive_b) =
        attach_for_command_test(&registry, &host_b, instance.clone());
    let plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let adapter = crate::host_runtime::agent_materialize::ProductionAgentMaterialize::new(
        plane.clone(),
        Duration::from_secs(10),
    );
    let captured_a = adapter
        .capture(&host_a)
        .await
        .unwrap()
        .expect("host A is attached at capture time");
    let target_b = AgentMaterializeTarget::reserved(
        host_b.clone(),
        AgentId::parse("agent-b").unwrap(),
        WorkspaceId::parse("wsah_agent-b").unwrap(),
        "Agent B".to_owned(),
        RuntimeKind::Codex,
        None,
        None,
        AgentMaterializeTemplate::Default,
    );
    assert!(matches!(
        adapter.materialize(captured_a, &target_b).await,
        Err(crate::host_runtime::agent_materialize::AgentMaterializeFailure::TargetCrossedCustody)
    ));
    assert!(matches!(
        receive_a.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        receive_b.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn clear_command_replacement_is_attach_pending_and_successor_fifo_stays_empty() {
    let registry = DaemonRegistry::with_process_epoch("clear-test".to_owned());
    let host = HostId::parse("host-a").unwrap();
    let instance = DaemonInstanceId::parse("same-instance").unwrap();
    let (connection_a, _receive_a) = attach_for_command_test(&registry, &host, instance.clone());
    let plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let captured_a = plane.resolve(&host).unwrap().unwrap();
    registry.remove(host.as_str(), &connection_a);
    let (_connection_b, mut receive_b) = attach_for_command_test(&registry, &host, instance);
    let result = crate::host_runtime::runtime_control::clear_captured(
        &plane,
        captured_a,
        &RuntimeScope::AgentSlot {
            agent: AgentId::parse("agent-a").unwrap(),
        },
        silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(1)
            .unwrap(),
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        crate::host_runtime::runtime_control::ClearDispatchOutcome::AttachPending
    ));
    assert!(matches!(
        receive_b.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[derive(Clone)]
struct ClearCustody;

impl silverhand_backend_core::host_runtime::runtime_credential::RuntimeCustodyPort
    for ClearCustody
{
    async fn claim_or_current(
        &self,
        _claim: silverhand_backend_core::host_runtime::runtime_credential::ClaimRuntimeCustody,
    ) -> Result<
        Option<silverhand_backend_core::host_runtime::runtime_credential::ClaimedRuntime>,
        PersistenceFailure,
    > {
        unreachable!("clear never claims custody")
    }

    async fn current(
        &self,
        _scope: RuntimeScope,
    ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
        unreachable!("clear never asks current custody")
    }

    async fn is_current(&self, _principal: RuntimePrincipal) -> Result<bool, PersistenceFailure> {
        unreachable!("clear never authenticates custody")
    }

    async fn finalize(
        &self,
        _finalization: silverhand_backend_core::host_runtime::runtime_credential::FinalizeRuntimeCustody,
    ) -> Result<bool, PersistenceFailure> {
        Ok(true)
    }
}

#[derive(Clone)]
struct ClearSeat {
    snapshot: silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlSeatSnapshot,
    settles: Arc<std::sync::atomic::AtomicUsize>,
}

impl RuntimeDispatchReadPort for ClearSeat {
    async fn resolve_dispatch_target(
        &self,
        _conversation: ConversationId,
        _agent: AgentId,
    ) -> Result<Option<RuntimeDispatchTarget>, PersistenceFailure> {
        unreachable!("clear never resolves a start target")
    }

    async fn resolve_start_dispatch(
        &self,
        _agent: AgentId,
        _candidate_origin: ConversationId,
    ) -> Result<Option<RuntimeDispatchTarget>, PersistenceFailure> {
        unreachable!("clear never resolves a start target")
    }

    async fn current_custodies_for_host(
        &self,
        _host_id: String,
    ) -> Result<Vec<RuntimePrincipal>, PersistenceFailure> {
        unreachable!("clear never hydrates")
    }
}

impl silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlSeatPort for ClearSeat {
    type Error = std::io::Error;

    async fn snapshot(
        &self,
        _scope: &silverhand_backend_core::host_runtime::runtime_credential::RuntimeScope,
    ) -> Result<
        Option<silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlSeatSnapshot>,
        Self::Error,
    > {
        Ok(Some(self.snapshot.clone()))
    }

    async fn settle_clear(
        &self,
        _target: &silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeSeatTarget,
        before: &silverhand_backend_core::conversation_messaging::messaging::collaboration::BoundRuntimeControlSeat,
    ) -> Result<
        Option<silverhand_backend_core::conversation_messaging::messaging::collaboration::ClearSessionCommitReceipt>,
        Self::Error,
    > {
        self.settles
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(
            silverhand_backend_core::conversation_messaging::messaging::collaboration::ClearSessionCommitReceipt::decoded(
                before.scope().clone(),
                before.generation(),
                before.previous_cli_session().cloned(),
                before.was_running(),
                None,
                silverhand_backend_core::conversation_messaging::messaging::collaboration::ClearSessionAuditWitness::Agent(
                    before.agent().clone(),
                ),
            ),
        ))
    }
}

#[derive(Clone)]
struct ClearComputer(HostId);

impl ComputerReadModel for ClearComputer {
    async fn list_for_owner(
        &self,
        _owner: UserId,
    ) -> Result<
        Vec<silverhand_backend_core::host_runtime::computer::ComputerRecord>,
        PersistenceFailure,
    > {
        unreachable!("exact clear never lists hosts")
    }

    async fn read_computer(
        &self,
        computer: HostId,
    ) -> Result<
        Option<silverhand_backend_core::host_runtime::computer::ComputerRecord>,
        PersistenceFailure,
    > {
        use silverhand_backend_core::host_runtime::computer::{
            ComputerRecord, HostKind, ProvisioningSource,
        };
        assert_eq!(computer, self.0);
        Ok(Some(ComputerRecord {
            id: self.0.clone(),
            name: "host".to_owned(),
            kind: HostKind::Remote,
            created_at: 0,
            owner: Some(UserId::parse("user-1").unwrap()),
            installation_id: None,
            last_seen_at: None,
            activated_at: Some(1),
            finalized_at: Some(1),
            revoked_at: None,
            provisioning_source: ProvisioningSource::UserInvite,
            display_label: None,
        }))
    }
}

struct ProductionClearFixture {
    control: crate::host_runtime::runtime_control::ProductionRuntimeControl<
        ClearCustody,
        ClearSeat,
        ClearComputer,
    >,
    receive: mpsc::Receiver<ConnectionControl>,
    waiters: crate::host_runtime::runtime_control::RuntimeClearSessionWaiters,
    principal: RuntimePrincipal,
    actor: UserId,
    agent: AgentId,
    settles: Arc<std::sync::atomic::AtomicUsize>,
}

/// A deterministic agent-scope admission fake. The seat target is minted only
/// by the core `clear_agent_session` use case (never a cross-crate
/// `RuntimeSeatTarget::new`), so these production-provider tests drive the same
/// entry as production: agent ladder (existence + edit), then the row-owned
/// `agent` session scope. Conversation reads are `unreachable!` — an agent-scope
/// clear touches no conversation.
#[derive(Clone)]
struct AgentSeatAdmission {
    agent: AgentId,
}

struct AgentSeatAdmissionScope {
    agent: AgentId,
}

impl silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlAdmissionScope
    for AgentSeatAdmissionScope
{
    type Error = std::io::Error;

    async fn conversation_exists(
        &mut self,
        _: &silverhand_backend_core::foundation::identity::ConversationId,
    ) -> Result<bool, Self::Error> {
        unreachable!("agent-scope clear reads no conversation")
    }

    async fn agent_exists(&mut self, agent: &AgentId) -> Result<bool, Self::Error> {
        Ok(agent == &self.agent)
    }

    async fn agent_is_member(
        &mut self,
        _: &silverhand_backend_core::foundation::identity::ConversationId,
        _: &AgentId,
    ) -> Result<bool, Self::Error> {
        unreachable!("agent-scope clear checks no membership")
    }

    async fn actor_may_post(
        &mut self,
        _: &silverhand_backend_core::foundation::identity::ConversationId,
        _: &UserId,
    ) -> Result<bool, Self::Error> {
        unreachable!("agent-scope clear runs no post gate")
    }

    async fn actor_may_edit_agent(
        &mut self,
        _: &UserId,
        agent: &AgentId,
    ) -> Result<bool, Self::Error> {
        Ok(agent == &self.agent)
    }

    async fn agent_session_scope(
        &mut self,
        agent: &AgentId,
    ) -> Result<
        Option<
            silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope,
        >,
        Self::Error,
    > {
        Ok((agent == &self.agent).then_some(
            silverhand_backend_core::identity_access::agents::session_scope::AgentSessionScope::Agent,
        ))
    }

    async fn finish(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlAdmission
    for AgentSeatAdmission
{
    type Error = std::io::Error;
    type Scope = AgentSeatAdmissionScope;

    async fn begin_snapshot(&self) -> Result<Self::Scope, Self::Error> {
        Ok(AgentSeatAdmissionScope {
            agent: self.agent.clone(),
        })
    }
}

fn production_clear_fixture() -> ProductionClearFixture {
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::{
        CliSessionId, RuntimeControlObservedStatus, RuntimeControlSeatSnapshot,
    };
    let registry = DaemonRegistry::with_process_epoch("clear-production".to_owned());
    let host = HostId::parse("host-a").unwrap();
    let daemon = DaemonInstanceId::parse("daemon-a").unwrap();
    let (_connection, receive) = attach_for_command_test(&registry, &host, daemon.clone());
    let scope = RuntimeScope::AgentSlot {
        agent: AgentId::parse("agent-a").unwrap(),
    };
    let generation =
        silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(7)
            .unwrap();
    let principal = RuntimePrincipal::for_scope(
        scope.clone(),
        silverhand_backend_core::host_runtime::runtime_credential::RuntimeCustody::new(
            generation,
            daemon.clone(),
        ),
    );
    let settles = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seats = ClearSeat {
        snapshot: RuntimeControlSeatSnapshot::decoded(
            AgentId::parse("agent-a").unwrap(),
            host.clone(),
            scope,
            generation,
            Some(daemon),
            Some(CliSessionId::parse("session-a").unwrap()),
            RuntimeControlObservedStatus::Active,
            "agent-a".to_owned(),
        ),
        settles: Arc::clone(&settles),
    };
    let plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let computers = ClearComputer(host);
    let waiters = crate::host_runtime::runtime_control::RuntimeClearSessionWaiters::default();
    let stop =
        ProductionScopeRuntimeStop::new(computers.clone(), plane.clone(), Duration::from_secs(30));
    let control = crate::host_runtime::runtime_control::ProductionRuntimeControl::new(
        RuntimeCredentialAuthority::production(ClearCustody),
        seats,
        computers,
        plane,
        stop,
        waiters.clone(),
        Duration::from_secs(30),
    );
    ProductionClearFixture {
        control,
        receive,
        waiters,
        principal,
        actor: UserId::parse("user-1").unwrap(),
        agent: AgentId::parse("agent-a").unwrap(),
        settles,
    }
}

#[tokio::test(start_paused = true)]
async fn clear_ack_alone_times_out_without_settlement_and_finally_releases_single_flight() {
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::{
        ClearSessionProviderRefusal, ClearSessionRefusal, clear_agent_session,
    };
    let ProductionClearFixture {
        control,
        mut receive,
        waiters,
        principal,
        actor,
        agent,
        settles,
    } = production_clear_fixture();
    let admission = AgentSeatAdmission {
        agent: agent.clone(),
    };
    let first_control = control.clone();
    let first_admission = admission.clone();
    let first_actor = actor.clone();
    let first_agent = agent.clone();
    let first = tokio::spawn(async move {
        clear_agent_session(&first_admission, &first_control, first_actor, first_agent).await
    });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("clear must enter the production FIFO")
    };
    assert_eq!(command.kind, IssuedDaemonCommandKind::RuntimeClearSession);
    assert_eq!(command.params["runtime_generation"], 7);
    command.response.send(Ok(json!({"cleared":true}))).unwrap();

    let concurrent = clear_agent_session(&admission, &control, actor.clone(), agent.clone())
        .await
        .unwrap();
    assert_eq!(
        concurrent,
        Err(ClearSessionRefusal::Provider(
            ClearSessionProviderRefusal::InProgress
        ))
    );
    waiters.notify_exit(&RuntimePrincipal::for_scope(
        principal.scope(),
        silverhand_backend_core::host_runtime::runtime_credential::RuntimeCustody::new(
            silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(8)
                .unwrap(),
            principal.custody().daemon_instance().clone(),
        ),
    ));
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(
        first.await.unwrap().unwrap(),
        Err(ClearSessionRefusal::Provider(
            ClearSessionProviderRefusal::Timeout
        ))
    );
    assert_eq!(settles.load(std::sync::atomic::Ordering::SeqCst), 0);

    let retry_control = control.clone();
    let retry_admission = admission.clone();
    let retry_actor = actor.clone();
    let retry_agent = agent.clone();
    let retry = tokio::spawn(async move {
        clear_agent_session(&retry_admission, &retry_control, retry_actor, retry_agent).await
    });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("finally cleanup must admit a retry")
    };
    command.response.send(Ok(json!({"cleared":false}))).unwrap();
    assert!(retry.await.unwrap().unwrap().is_ok());
    assert_eq!(settles.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exact_exit_before_clear_response_is_latched_then_settled() {
    use silverhand_backend_core::conversation_messaging::messaging::collaboration::clear_agent_session;
    let ProductionClearFixture {
        control,
        mut receive,
        waiters,
        principal,
        actor,
        agent,
        settles,
    } = production_clear_fixture();
    let admission = AgentSeatAdmission {
        agent: agent.clone(),
    };
    let call =
        tokio::spawn(async move { clear_agent_session(&admission, &control, actor, agent).await });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("clear must enter the production FIFO")
    };
    waiters.notify_exit(&principal);
    command.response.send(Ok(json!({"cleared":true}))).unwrap();
    assert!(call.await.unwrap().unwrap().is_ok());
    assert_eq!(settles.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[derive(Clone)]
struct StartCustody;

impl silverhand_backend_core::host_runtime::runtime_credential::RuntimeCustodyPort
    for StartCustody
{
    async fn claim_or_current(
        &self,
        claim: silverhand_backend_core::host_runtime::runtime_credential::ClaimRuntimeCustody,
    ) -> Result<
        Option<silverhand_backend_core::host_runtime::runtime_credential::ClaimedRuntime>,
        PersistenceFailure,
    > {
        Ok(Some(
            silverhand_backend_core::host_runtime::runtime_credential::ClaimedRuntime::new(
                RuntimePrincipal::for_scope(
                    claim.scope().clone(),
                    silverhand_backend_core::host_runtime::runtime_credential::RuntimeCustody::new(
                        silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(1)
                            .unwrap(),
                        claim.daemon_instance().clone(),
                    ),
                ),
                true,
            ),
        ))
    }

    async fn current(
        &self,
        _scope: RuntimeScope,
    ) -> Result<Option<RuntimePrincipal>, PersistenceFailure> {
        Ok(None)
    }

    async fn is_current(&self, _principal: RuntimePrincipal) -> Result<bool, PersistenceFailure> {
        Ok(true)
    }

    async fn finalize(
        &self,
        _finalization: silverhand_backend_core::host_runtime::runtime_credential::FinalizeRuntimeCustody,
    ) -> Result<bool, PersistenceFailure> {
        Ok(true)
    }
}

#[derive(Clone)]
struct StartSeat(RuntimeDispatchTarget);

impl RuntimeDispatchReadPort for StartSeat {
    async fn resolve_dispatch_target(
        &self,
        _conversation: ConversationId,
        _agent: AgentId,
    ) -> Result<Option<RuntimeDispatchTarget>, PersistenceFailure> {
        unreachable!("start resolves through resolve_start_dispatch")
    }

    async fn resolve_start_dispatch(
        &self,
        agent: AgentId,
        candidate_origin: ConversationId,
    ) -> Result<Option<RuntimeDispatchTarget>, PersistenceFailure> {
        assert_eq!(candidate_origin, self.0.conversation);
        assert_eq!(agent, *self.0.plan.scope.agent());
        Ok(Some(self.0.clone()))
    }

    async fn current_custodies_for_host(
        &self,
        _host_id: String,
    ) -> Result<Vec<RuntimePrincipal>, PersistenceFailure> {
        unreachable!("runtime control start never hydrates")
    }
}

impl silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlSeatPort for StartSeat {
    type Error = std::io::Error;

    async fn snapshot(
        &self,
        _scope: &silverhand_backend_core::host_runtime::runtime_credential::RuntimeScope,
    ) -> Result<
        Option<silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeControlSeatSnapshot>,
        Self::Error,
    > {
        unreachable!("start resolves the spawn plan")
    }

    async fn settle_clear(
        &self,
        _target: &silverhand_backend_core::conversation_messaging::messaging::collaboration::RuntimeSeatTarget,
        _before: &silverhand_backend_core::conversation_messaging::messaging::collaboration::BoundRuntimeControlSeat,
    ) -> Result<
        Option<silverhand_backend_core::conversation_messaging::messaging::collaboration::ClearSessionCommitReceipt>,
        Self::Error,
    > {
        unreachable!("start never settles clear")
    }
}

#[tokio::test]
async fn runtime_control_start_traverses_typed_codec_and_production_fifo() {
    use silverhand_backend_core::{
        conversation_messaging::messaging::collaboration::{
            RuntimeControlPort, RuntimeStartTarget,
        },
        conversation_messaging::runtime_delivery::{RuntimeAgentSpec, RuntimeSpawnPlan},
        host_runtime::runtime_credential::{AgentSlotPresentationFocusPort, RuntimeCustody},
        identity_access::agents::runtime::RuntimeKind,
    };
    let registry = DaemonRegistry::with_process_epoch("start-production".to_owned());
    let host = HostId::parse("host-a").unwrap();
    let (_connection, mut receive) = attach_for_command_test(
        &registry,
        &host,
        DaemonInstanceId::parse("daemon-a").unwrap(),
    );
    let conversation = ConversationId::parse("conversation-a").unwrap();
    let agent = AgentId::parse("agent-a").unwrap();
    let seats = StartSeat(RuntimeDispatchTarget {
        host_id: host.as_str().to_owned(),
        conversation: conversation.clone(),
        plan: RuntimeSpawnPlan {
            scope: RuntimeScope::AgentSlot {
                agent: agent.clone(),
            },
            agent: RuntimeAgentSpec {
                id: agent.clone(),
                name: "agent-a".to_owned(),
                runtime: RuntimeKind::Codex,
                model: None,
                reasoning_effort: None,
                fast_mode: None,
                cli_session_id: None,
            },
            workspace: None,
            workspace_memory_index: Vec::new(),
            session: None,
        },
    });
    let plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let computers = ClearComputer(host);
    let stop =
        ProductionScopeRuntimeStop::new(computers.clone(), plane.clone(), Duration::from_secs(30));
    let credentials = RuntimeCredentialAuthority::production(StartCustody);
    let focus = credentials.clone();
    let expected_principal = RuntimePrincipal::for_scope(
        RuntimeScope::AgentSlot {
            agent: agent.clone(),
        },
        RuntimeCustody::new(
            silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(1)
                .unwrap(),
            DaemonInstanceId::parse("daemon-a").unwrap(),
        ),
    );
    let control = crate::host_runtime::runtime_control::ProductionRuntimeControl::new(
        credentials,
        seats,
        computers,
        plane,
        stop,
        crate::host_runtime::runtime_control::RuntimeClearSessionWaiters::default(),
        Duration::from_secs(30),
    );
    let target = RuntimeStartTarget::new(UserId::parse("user-1").unwrap(), agent, conversation);

    let invalid_control = control.clone();
    let invalid_target = target.clone();
    let invalid = tokio::spawn(async move { invalid_control.start(&invalid_target).await });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("invalid runtime_start must enter the production FIFO")
    };
    command.response.send(Ok(json!({"started":true}))).unwrap();
    assert!(invalid.await.unwrap().is_err());
    assert_eq!(
        focus
            .agent_slot_presentation_focus(expected_principal.clone())
            .await
            .unwrap(),
        None
    );

    let cooldown_control = control.clone();
    let cooldown_target = target.clone();
    let cooldown = tokio::spawn(async move { cooldown_control.start(&cooldown_target).await });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("cooldown runtime_start must enter the production FIFO")
    };
    command
        .response
        .send(Ok(json!({
            "started":false,
            "reason":"init_cooldown",
            "cooldown_until":7
        })))
        .unwrap();
    assert!(cooldown.await.unwrap().unwrap().is_ok());
    assert_eq!(
        focus
            .agent_slot_presentation_focus(expected_principal.clone())
            .await
            .unwrap(),
        None
    );

    let call = tokio::spawn(async move { control.start(&target).await });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("runtime_start must enter the production FIFO")
    };
    assert_eq!(command.kind, IssuedDaemonCommandKind::RuntimeStart);
    assert_eq!(command.params["conversation_id"], "conversation-a");
    assert_eq!(command.params["agent_id"], "agent-a");
    assert_eq!(command.params["runtime_generation"], 1);
    assert_eq!(
        command.params["options"]["runtime_scope"],
        json!({"kind":"agent","agent_id":"agent-a"})
    );
    command
        .response
        .send(Ok(json!({"started":true,"pid":42,"started_at":7})))
        .unwrap();
    assert!(call.await.unwrap().unwrap().is_ok());
    assert_eq!(
        focus
            .agent_slot_presentation_focus(expected_principal)
            .await
            .unwrap(),
        Some(ConversationId::parse("conversation-a").unwrap())
    );
}

#[test]
fn the_rotate_token_frame_is_keyed_as_the_daemon_expects() {
    assert_eq!(
        rotation_params("host-1", "cleartext-token".to_owned(), 100, 61),
        json!({
            "host_id": "host-1",
            "host_token": "cleartext-token",
            "issued_at": 100,
            "grace_until": 61,
        })
    );
}

#[test]
fn a_stored_ack_carries_the_computer_and_the_refresh_instant() {
    assert_eq!(
        parse_rotation_ack(&json!({
            "rotated": true,
            "host_id": "host-1",
            "config_refreshed_at": 42,
        })),
        Some(DaemonRotationAck::Stored {
            computer: HostId::parse("host-1").unwrap(),
            config_refreshed_at: 42,
        })
    );
}

#[test]
fn false_or_absent_rotated_preserves_the_daemon_refusal_reason() {
    assert_eq!(
        parse_rotation_ack(&json!({ "rotated": false, "reason": "daemon_busy" })),
        Some(DaemonRotationAck::Refused {
            reason: "daemon_busy".to_owned(),
        })
    );
    // A daemon that answered without saying it rotated has refused, matching the TS
    // coordinator's `!rotated` branch; with no reason to carry the field is empty.
    assert_eq!(
        parse_rotation_ack(&json!({ "reason": "nope" })),
        Some(DaemonRotationAck::Refused {
            reason: "nope".to_owned(),
        })
    );
    assert_eq!(
        parse_rotation_ack(&json!({ "rotated": false })),
        Some(DaemonRotationAck::Refused {
            reason: String::new(),
        })
    );
}

#[test]
fn a_broken_success_frame_is_not_an_acknowledgement() {
    // `rotated: true` with no computer, or no refresh instant, or not an object at
    // all: none stores anything. The caller reads the `None` as unreachable (502),
    // never as a silent success.
    assert_eq!(
        parse_rotation_ack(&json!({ "rotated": true, "config_refreshed_at": 42 })),
        None
    );
    assert_eq!(
        parse_rotation_ack(&json!({ "rotated": true, "host_id": "host-1" })),
        None
    );
    assert_eq!(parse_rotation_ack(&json!("rotated")), None);
}

#[test]
fn rust_consumes_the_ts_first_exact_daemon_event_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../packages/host/tests/fixtures/outbound-daemon-events.json"
    ))
    .unwrap();
    let valid = fixture["valid"].as_array().unwrap();
    let valid_kinds = valid
        .iter()
        .map(|frame| frame["kind"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        valid_kinds,
        OUTBOUND_DAEMON_EVENT_KINDS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    );
    for frame in valid {
        assert!(
            daemon_ingress_is_valid(&frame.to_string()),
            "valid fixture rejected: {}",
            frame["kind"]
        );
    }
    let parsed_envelope = valid
        .iter()
        .find(|frame| frame["kind"] == "runtime_parsed_event")
        .unwrap();
    let parsed_kinds = fixture["parsedValid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|parsed| parsed["kind"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        parsed_kinds,
        PARSED_EVENT_KINDS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(
        crate::host_runtime::runtime_event_settlement::ParsedEventWire::IMPLEMENTED_KINDS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        PARSED_EVENT_KINDS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    );
    for parsed in fixture["parsedValid"].as_array().unwrap() {
        let mut frame = parsed_envelope.clone();
        frame["parsed"] = parsed.clone();
        let parsed_frame = parse_daemon_frame(&frame.to_string())
            .unwrap_or_else(|_| panic!("valid parsed fixture rejected: {}", parsed["kind"]));
        let DaemonFrame::Runtime { kind, value } = parsed_frame else {
            panic!("parsed fixture reached the wrong owner: {}", parsed["kind"]);
        };
        let crate::host_runtime::runtime_event_settlement::RuntimeProductEvent::Parsed(event) =
            crate::host_runtime::runtime_event_settlement::decode_product_event(&kind, &value)
                .unwrap()
        else {
            panic!("parsed fixture reached the wrong owner: {}", parsed["kind"]);
        };
        assert_eq!(event.parsed.kind(), parsed["kind"].as_str().unwrap());
    }
    let state_envelope = valid
        .iter()
        .find(|frame| frame["kind"] == "runtime_state")
        .unwrap();
    for test_case in fixture["runState"]["valid"].as_array().unwrap() {
        let state = serde_json::from_value::<
            crate::host_runtime::runtime_event_settlement::AgentRunStateWire,
        >(test_case["state"].clone())
        .unwrap_or_else(|_| panic!("valid run state rejected: {}", test_case["name"]));
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            test_case["canonical"]
        );
        let mut frame = state_envelope.clone();
        frame["runtime_scope"] = json!({
            "kind": "conversation",
            "conversation_id": test_case["state"]["conversationId"],
            "agent_id": test_case["state"]["agentId"],
        });
        frame["conversation_id"] = test_case["state"]["conversationId"].clone();
        frame["agent_id"] = test_case["state"]["agentId"].clone();
        frame["state"] = test_case["state"].clone();
        assert!(
            parse_daemon_frame(&frame.to_string()).is_ok(),
            "valid ingress run state rejected: {}",
            test_case["name"]
        );
    }
    for test_case in fixture["runState"]["invalid"].as_array().unwrap() {
        assert!(
            serde_json::from_value::<
                crate::host_runtime::runtime_event_settlement::AgentRunStateWire,
            >(test_case["state"].clone(),)
            .is_err(),
            "invalid hydrate/live run state accepted: {}",
            test_case["name"]
        );
        let mut frame = state_envelope.clone();
        frame["runtime_scope"] = json!({
            "kind": "conversation",
            "conversation_id": test_case["state"]["conversationId"],
            "agent_id": test_case["state"]["agentId"],
        });
        frame["conversation_id"] = test_case["state"]["conversationId"].clone();
        frame["agent_id"] = test_case["state"]["agentId"].clone();
        frame["state"] = test_case["state"].clone();
        assert!(
            !daemon_ingress_is_valid(&frame.to_string()),
            "invalid ingress run state accepted: {}",
            test_case["name"]
        );
    }
    for test_case in fixture["invalid"].as_array().unwrap() {
        assert!(
            !daemon_ingress_is_valid(&test_case["frame"].to_string()),
            "invalid fixture accepted: {}",
            test_case["name"]
        );
    }
}

#[test]
fn application_heartbeat_timestamps_are_nonnegative() {
    let ping = json!({
        "direction": "daemon_to_server",
        "type": "heartbeat",
        "kind": "ping",
        "ts": 1,
    });
    assert!(matches!(
        parse_daemon_frame(&ping.to_string()),
        Ok(DaemonFrame::HeartbeatPing(_))
    ));
    let mut negative = ping;
    negative["ts"] = json!(-1);
    assert!(parse_daemon_frame(&negative.to_string()).is_err());
}

#[test]
fn response_result_shapes_do_not_accept_extra_fields() {
    assert_eq!(
        runtime_start_codec::decode_result(&json!({ "started": true, "pid": 1, "started_at": 2 })),
        Some(runtime_start_codec::RuntimeStartResult::Started)
    );
    assert_eq!(
        runtime_start_codec::decode_result(&json!({
            "started": true,
            "pid": 1,
            "started_at": 2,
            "extra": true,
        })),
        None
    );
    assert!(matches!(
        delivery_disposition(&json!({ "disposition": "refused", "reason": "startup_pending" })),
        DeliveryDisposition::Refused
    ));
    assert!(matches!(
        delivery_disposition(&json!({
            "disposition": "refused",
            "reason": "startup_pending",
            "cooldown_until": 4,
        })),
        DeliveryDisposition::Invalid
    ));
}

#[test]
fn lease_owner_coordinates_do_not_repeat_across_process_epochs_or_host_order() {
    let first_process = DaemonRegistry::with_process_epoch("process-a".into());
    let second_process = DaemonRegistry::with_process_epoch("process-b".into());
    let (first_a, _) = mpsc::channel(1);
    let (first_b, _) = mpsc::channel(1);
    let (second_b, _) = mpsc::channel(1);
    let (second_a, _) = mpsc::channel(1);

    let old_host_a = first_process
        .reserve("host-a", first_a, watch::channel(None).0)
        .unwrap();
    let old_host_b = first_process
        .reserve("host-b", first_b, watch::channel(None).0)
        .unwrap();
    let new_host_b = second_process
        .reserve("host-b", second_b, watch::channel(None).0)
        .unwrap();
    let new_host_a = second_process
        .reserve("host-a", second_a, watch::channel(None).0)
        .unwrap();

    assert_ne!(old_host_a, new_host_a);
    assert_ne!(old_host_b, new_host_b);
    assert_ne!(old_host_a, new_host_b);
    assert_ne!(old_host_b, new_host_a);
    assert!(old_host_a.starts_with("daemon-connection-process-a-"));
    assert!(new_host_b.starts_with("daemon-connection-process-b-"));
}

async fn start_archive_fixture(path: &std::path::Path) -> sqlx::SqliteConnection {
    let mut db = current_schema::create_current_schema(path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-a', 'host-a', 'remote', 1, 'user-1'),
                ('host-b', 'host-b', 'remote', 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope
         ) VALUES ('agent-lease', 'agent-lease', 'codex', 'host-b', 1, 1,
             'Lease Agent', 'user-1', 'conversation')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (
            id, name, name_key, created_at, updated_at, owner_user_id
         ) VALUES ('workspace-1', 'Workspace One', 'workspace one', 1, 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
         VALUES ('workspace-1', 'agent-lease', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (
            id, workspace_id, placement_kind, roster_policy, title, title_key,
            status, created_at, updated_at
         ) VALUES ('conversation0001', 'workspace-1', 'workspace', 'open',
             'Conversation One', 'conversation one', 'active', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES ('conversation0001', 'agent-lease', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db
}

#[tokio::test]
async fn automatic_start_rechecks_archive_before_entering_production_fifo() {
    use crate::host_runtime::daemon_runtime_transport::StartTransport;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("start-archive.sqlite");
    let mut db = start_archive_fixture(&path).await;
    let store = SqliteRuntimeDeliveryStore::open(&path).await.unwrap();
    let conversation = ConversationId::parse("conversation0001").unwrap();
    let agent = AgentId::parse("agent-lease").unwrap();
    let target = store
        .resolve_dispatch_target(conversation.clone(), agent)
        .await
        .unwrap()
        .expect("active target is projected before archive");

    let registry = DaemonRegistry::with_process_epoch("start-archive".into());
    let host = HostId::parse("host-b").unwrap();
    let instance = DaemonInstanceId::parse("daemon-start-archive").unwrap();
    let (_connection, mut receive) = attach_for_command_test(&registry, &host, instance.clone());
    let plane = OwningHostDataPlane { registry };
    let attached = plane.capture_attached(host.as_str()).unwrap().unwrap();
    let transport = StartTransport::new(attached, target.clone(), Duration::from_secs(1), store);
    let credentials = RuntimeCredentialAuthority::production(StartCustody);

    // Archive commits after projection but before start dispatch admission.
    // No inbox batch is involved in this path.
    sqlx::query("UPDATE conversations SET archived_at = 2 WHERE id = ?")
        .bind(conversation.as_str())
        .execute(&mut db)
        .await
        .unwrap();
    let result = credentials
        .dispatch_start(
            target.plan.scope.clone(),
            instance.clone(),
            conversation.clone(),
            &transport,
        )
        .await;
    assert!(matches!(
        result,
        Err(crate::host_runtime::runtime_credential::RuntimeStartFailure::ProvenNotDispatched(
            crate::host_runtime::daemon_runtime_transport::StartTransportError::TargetUnavailable
        ))
    ));
    assert!(matches!(
        receive.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    // The same production transport still dispatches for an active channel.
    sqlx::query("UPDATE conversations SET archived_at = NULL WHERE id = ?")
        .bind(conversation.as_str())
        .execute(&mut db)
        .await
        .unwrap();
    let call = tokio::spawn(async move {
        credentials
            .dispatch_start(target.plan.scope, instance, conversation, &transport)
            .await
    });
    let ConnectionControl::Command(command) = receive.recv().await.unwrap() else {
        panic!("active automatic start must enter FIFO")
    };
    command
        .response
        .send(Ok(json!({"started":true,"pid":123,"started_at":100})))
        .unwrap();
    assert!(call.await.unwrap().is_ok());
}

#[tokio::test]
async fn a_restarted_process_cannot_release_an_old_hosts_real_inbox_lease() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lease-owner-process-epoch.sqlite");
    let mut db = current_schema::create_current_schema(&path).await;
    sqlx::query(
        "INSERT INTO users (id, name, display_name, created_at, updated_at)
         VALUES ('user-1', 'user-1', 'User One', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO hosts (id, name, kind, created_at, user_id)
         VALUES ('host-a', 'host-a', 'remote', 1, 'user-1'),
                ('host-b', 'host-b', 'remote', 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, runtime, host_id, created_at, updated_at,
            display_name, created_by_user_id, session_scope
         ) VALUES ('agent-lease', 'agent-lease', 'codex', 'host-b', 1, 1,
             'Lease Agent', 'user-1', 'conversation')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (
            id, name, name_key, created_at, updated_at, owner_user_id
         ) VALUES ('workspace-1', 'Workspace One', 'workspace one', 1, 1, 'user-1')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, agent_id, joined_at)
         VALUES ('workspace-1', 'agent-lease', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (
            id, workspace_id, placement_kind, roster_policy, title, title_key,
            status, created_at, updated_at
         ) VALUES ('conversation0001', 'workspace-1', 'workspace', 'open',
             'Conversation One', 'conversation one', 'active', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_agents (conversation_id, agent_id, joined_at)
         VALUES ('conversation0001', 'agent-lease', 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (
            id, conversation_id, from_kind, content, seq, created_at
         ) VALUES ('message-lease', 'conversation0001', 'system', 'lease', 1, 1)",
    )
    .execute(&mut db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_inboxes (
            conversation_id, agent_id, message_id, created_at, delivery_kind
         ) VALUES ('conversation0001', 'agent-lease', 'message-lease', 1, 'turn')",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();

    let first_process = DaemonRegistry::with_process_epoch("process-a".into());
    let second_process = DaemonRegistry::with_process_epoch("process-b".into());
    let (old_a_tx, _) = mpsc::channel(1);
    let (old_b_tx, _) = mpsc::channel(1);
    let (new_b_tx, _) = mpsc::channel(1);
    let (new_a_tx, _) = mpsc::channel(1);
    let _old_host_a = first_process
        .reserve("host-a", old_a_tx, watch::channel(None).0)
        .unwrap();
    let old_host_b = first_process
        .reserve("host-b", old_b_tx, watch::channel(None).0)
        .unwrap();
    let _new_host_b = second_process
        .reserve("host-b", new_b_tx, watch::channel(None).0)
        .unwrap();
    let new_host_a = second_process
        .reserve("host-a", new_a_tx, watch::channel(None).0)
        .unwrap();
    let old_owner = DeliveryLeaseOwner::parse(old_host_b).unwrap();
    let new_owner = DeliveryLeaseOwner::parse(new_host_a).unwrap();

    let custody = SqliteRuntimeCustodyStore::open(&path).await.unwrap();
    let scope = RuntimeScope::ConversationSlot {
        agent: AgentId::parse("agent-lease").unwrap(),
        source: ConversationId::parse("conversation0001").unwrap(),
    };
    let principal = custody
        .claim_or_current(ClaimRuntimeCustody::new(
            scope,
            DaemonInstanceId::parse("daemon-old").unwrap(),
            RuntimeEventTime::from_unix_millis(1),
        ))
        .await
        .unwrap()
        .unwrap()
        .into_principal();
    let store = SqliteRuntimeDeliveryStore::open(&path).await.unwrap();
    let claim = store
        .claim_pending(
            "host-b".into(),
            principal.clone(),
            ConversationId::parse("conversation0001").unwrap(),
            old_owner.clone(),
            100,
            1_000,
            &NoReferences,
        )
        .await
        .unwrap();
    assert!(matches!(claim, ClaimPendingInbox::Claimed(_)));

    assert_eq!(store.release_owner(new_owner).await.unwrap(), 0);
    let mut check = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    let persisted_owner: Option<String> = sqlx::query_scalar(
        "SELECT in_flight_owner FROM conversation_inboxes WHERE message_id = 'message-lease'",
    )
    .fetch_one(&mut check)
    .await
    .unwrap();
    assert_eq!(persisted_owner.as_deref(), Some(old_owner.as_str()));
    assert_eq!(
        store
            .claim_pending(
                "host-b".into(),
                principal,
                ConversationId::parse("conversation0001").unwrap(),
                DeliveryLeaseOwner::parse("third-owner").unwrap(),
                101,
                1_000,
                &NoReferences,
            )
            .await
            .unwrap(),
        ClaimPendingInbox::Empty
    );
    assert_eq!(store.release_owner(old_owner).await.unwrap(), 1);
    let released: Option<String> = sqlx::query_scalar(
        "SELECT in_flight_owner FROM conversation_inboxes WHERE message_id = 'message-lease'",
    )
    .fetch_one(&mut check)
    .await
    .unwrap();
    assert!(released.is_none());
}

#[test]
fn rust_batch_and_novel_remainder_consume_the_shared_presentation_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../packages/shared/tests/fixtures/runtime-delivery-presentation.json"
    ))
    .unwrap();
    let deliveries = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, case)| {
            let instruction = match case["expectedInstruction"].as_str().unwrap() {
                "user_for_recipient" => DeliveryInstruction::UserForRecipient,
                "user_for_other" => DeliveryInstruction::UserForOther,
                "non_system_mention" => DeliveryInstruction::NonSystemMention,
                "system" => DeliveryInstruction::System,
                "other" => DeliveryInstruction::Other,
                other => panic!("unexpected fixture instruction {other}"),
            };
            silverhand_backend_core::conversation_messaging::runtime_delivery::ClaimedInboxDelivery::decoded(
                InboxDeliveryId::parse(i64::try_from(index + 1).unwrap()).unwrap(),
                i64::try_from(index).unwrap(),
                case["expectedBody"].as_str().unwrap().to_owned(),
                instruction,
            )
        })
        .collect::<Vec<_>>();
    let principal = RuntimePrincipal::for_scope(
        RuntimeScope::AgentSlot {
            agent: AgentId::parse("fixture-agent").unwrap(),
        },
        silverhand_backend_core::host_runtime::runtime_credential::RuntimeCustody::new(
            silverhand_backend_core::host_runtime::runtime_credential::RuntimeGeneration::new(1)
                .unwrap(),
            DaemonInstanceId::parse("fixture-daemon").unwrap(),
        ),
    );
    let batch = ClaimedInboxBatch::claimed(
        principal,
        ConversationId::parse("aBcDeFgHiJkLmN_P").unwrap(),
        DeliveryLeaseOwner::parse("fixture-owner").unwrap(),
        deliveries,
    )
    .unwrap();

    assert_eq!(
        compose_delivery_text(&batch),
        fixture["expectedBatch"].as_str().unwrap()
    );
    let remainder = ClaimedInboxBatch::claimed(
        batch.principal().clone(),
        batch.conversation().clone(),
        batch.owner().clone(),
        batch.deliveries()[1..].to_vec(),
    )
    .unwrap();
    let recomposed = compose_delivery_text(&remainder);
    assert!(
        recomposed
            .starts_with("You have 2 messages waiting since you were last active (oldest first):")
    );
    assert!(recomposed.contains("msg_id=msg-agent"));
    assert!(!recomposed.contains("msg_id=msg-human"));
}

fn purge_test_registry() -> DaemonRegistry {
    DaemonRegistry::with_process_epoch("purge-epoch".to_owned())
}

/// Reserve and attach a fresh generation for `host`, returning its captured
/// (connection id, instance). The command channel is retained by the slot but no
/// consumer drains it: every generation-mismatch test must refuse BEFORE any send.
fn attach_generation(
    registry: &DaemonRegistry,
    host: &str,
    instance: &str,
) -> (String, DaemonInstanceId) {
    let (commands, _commands_rx) = mpsc::channel(4);
    let (close, _close_rx) = watch::channel(None);
    let connection_id = registry.reserve(host, commands, close).expect("reserve");
    let instance = DaemonInstanceId::parse(instance).expect("instance");
    registry
        .attach(
            host,
            &connection_id,
            AttachedDaemon {
                daemon_instance: instance.clone(),
                host_name: host.to_owned(),
            },
        )
        .expect("attach");
    (connection_id, instance)
}

#[tokio::test]
async fn a_generation_pinned_purge_refuses_a_replaced_connection() {
    let registry = purge_test_registry();
    let host = HostId::parse("host-1").expect("host");
    let agent = AgentId::parse("agent-1").expect("agent");
    let (captured_connection, _captured_instance) =
        attach_generation(&registry, "host-1", "instance-1");
    let data_plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let custody = data_plane.resolve(&host).unwrap().unwrap();

    // The daemon reconnects on a fresh generation before the captured purge is sent.
    registry.remove("host-1", &captured_connection);
    let (live_connection, _live_instance) = attach_generation(&registry, "host-1", "instance-1");
    assert_ne!(live_connection, captured_connection);

    let effects = DaemonCascadeEffects::new(data_plane);
    let failure = effects
        .purge_host_local_files_on_generation(custody, &agent)
        .await
        .expect_err("a superseded connection generation must not purge");
    assert!(
        error::describe(&failure).contains("connection generation"),
        "unexpected failure chain: {}",
        error::describe(&failure)
    );
}

#[tokio::test]
async fn a_generation_pinned_purge_refuses_a_reused_connection_with_a_new_instance() {
    let registry = purge_test_registry();
    let host = HostId::parse("host-1").expect("host");
    let agent = AgentId::parse("agent-1").expect("agent");
    let (connection, _captured_instance) = attach_generation(&registry, "host-1", "instance-1");
    let data_plane = OwningHostDataPlane {
        registry: registry.clone(),
    };
    let custody = data_plane.resolve(&host).unwrap().unwrap();

    // The same connection slot re-hydrates under a different daemon instance.
    registry
        .attach(
            "host-1",
            &connection,
            AttachedDaemon {
                daemon_instance: DaemonInstanceId::parse("instance-2").expect("instance"),
                host_name: "host-1".to_owned(),
            },
        )
        .expect("re-attach");

    let effects = DaemonCascadeEffects::new(data_plane);
    let failure = effects
        .purge_host_local_files_on_generation(custody, &agent)
        .await
        .expect_err("a reused connection under a new instance must not purge");
    assert!(
        error::describe(&failure).contains("daemon custody changed"),
        "unexpected failure chain: {}",
        error::describe(&failure)
    );
}

#[tokio::test]
async fn a_fence_binder_declines_a_stale_generation_and_binds_a_live_one() {
    let registry = purge_test_registry();
    let host = HostId::parse("host-1").expect("host");
    let (connection, _instance) = attach_generation(&registry, "host-1", "instance-1");

    let binder = DaemonHostConnections::new(OwningHostDataPlane {
        registry: registry.clone(),
    });
    assert!(
        binder.bind(&host, &connection).is_some(),
        "the live generation must bind a fence"
    );
    assert!(
        binder
            .bind(&host, "daemon-connection-purge-epoch-999")
            .is_none(),
        "a stale generation must not bind"
    );
    assert_eq!(
        binder.current_generation(&host).as_deref(),
        Some(connection.as_str())
    );
}
