//! The presence feed's disconnect-clock failure path: a clock that cannot seal an
//! instant evicts the gone connection with no offline effect, audits the fault
//! exactly once, and never strands the viewer online — and a later legal
//! reconnect still transitions the viewer back to Online.

use crate::log_capture;

use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use log_capture::LogBuffer;
use silverhand_backend_core::{
    foundation::identity::UserId,
    foundation::persistence::PersistenceFailure,
    viewer_read_model::presence::LastSeenAt,
    viewer_read_model::presence_grace::{GraceWindow, PresenceFact},
    viewer_read_model::user_presence_last_seen::LastSeenSink,
};
use silverhand_backend_server::{
    foundation::scope_event_transport::ConnectionLifecycle,
    host_runtime::host_agent_background::presence_grace_timer::{
        PresenceFactSink, PresenceGraceTimers,
    },
    viewer_read_model::presence::{DisconnectClock, PresenceConnectionFeed},
};

/// A disconnect clock authority that is down: every read fails. Its error carries
/// a message, so the feed's single audit renders a complete, self-describing
/// source chain.
#[derive(Debug)]
struct ClockAuthorityDown;

impl std::fmt::Display for ClockAuthorityDown {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("disconnect wall clock authority offline")
    }
}

impl std::error::Error for ClockAuthorityDown {}

struct FailingDisconnectClock;

impl DisconnectClock for FailingDisconnectClock {
    type Error = ClockAuthorityDown;

    fn read_unix_ms(&self) -> Result<i64, Self::Error> {
        Err(ClockAuthorityDown)
    }
}

/// Records every last-seen persistence and every published presence fact, so the
/// test can assert the failing-clock disconnect produced zero of the former and
/// only the two Online transitions of the latter.
#[derive(Clone, Default)]
struct Recorder {
    facts: Arc<Mutex<Vec<PresenceFact>>>,
    touches: Arc<AtomicUsize>,
}

impl LastSeenSink for Recorder {
    async fn touch_last_seen(
        &self,
        _user_id: UserId,
        _last_seen_at: LastSeenAt,
    ) -> Result<(), PersistenceFailure> {
        self.touches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl PresenceFactSink for Recorder {
    type Error = Infallible;

    fn publish(&self, fact: PresenceFact) -> Result<(), Self::Error> {
        self.facts.lock().unwrap().push(fact);
        Ok(())
    }
}

#[tokio::test]
async fn a_failing_disconnect_clock_evicts_without_effect_and_a_reconnect_re_emits_online() {
    let recorder = Recorder::default();
    let timers = PresenceGraceTimers::new(
        recorder.clone(),
        recorder.clone(),
        GraceWindow::from_millis_or_default(Some(20_000)),
    );
    let feed = PresenceConnectionFeed::new(timers.clone(), FailingDisconnectClock);

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_target(false)
        .with_writer(logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let viewer = UserId::parse("user-1").unwrap();
    feed.on_connect(&viewer, "conn-1");
    assert!(timers.is_online(&viewer));

    // The disconnect clock is down: the connection is evicted with no scheduled
    // window, so the viewer is offline immediately — never stranded online.
    feed.on_disconnect(&viewer, "conn-1");
    assert!(!timers.is_online(&viewer));

    // A legal reconnect after the failure still transitions Online.
    feed.on_connect(&viewer, "conn-2");
    assert!(timers.is_online(&viewer));

    // effect = 0 offline: nothing persisted, no Offline published. The only facts
    // are the two Online transitions (initial connect and reconnect).
    assert_eq!(recorder.touches.load(Ordering::SeqCst), 0);
    let facts = recorder.facts.lock().unwrap().clone();
    assert_eq!(
        facts,
        vec![
            PresenceFact::Online {
                user_id: viewer.clone(),
            },
            PresenceFact::Online {
                user_id: viewer.clone(),
            },
        ]
    );

    // Exactly one audited clock fault — the feed's, since it owns the clock —
    // carrying the complete source chain and no fabricated sentinel instant.
    let errors = logs
        .events()
        .into_iter()
        .filter(|event| event["level"] == "ERROR")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0]["fields"]["message"],
        "presence: disconnect wall clock unavailable; evicting the connection without an offline instant"
    );
    let chain = errors[0]["fields"]["error_chain"].as_str().unwrap();
    assert!(chain.contains("disconnect wall clock authority offline"));
    assert!(!chain.contains(&i64::MIN.to_string()));
}
