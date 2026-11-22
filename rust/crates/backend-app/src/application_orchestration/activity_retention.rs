//! Process-owned retention: sweep at boot and daily, for both deployment shapes.
//! No persisted scheduling state or per-agent coordinator. Each sweep prunes
//! expired activity and deletes human uploads no message picked up.
use chrono::{TimeZone, Utc};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use silverhand_backend_core::conversation_messaging::messaging::collaboration::conversation_agent_activity::ACTIVITY_RETENTION_MS;
use silverhand_backend_server::foundation::{
    clock::ProcessClock,
    scope_event_transport::ScopeEventHub,
};
use silverhand_backend_core::foundation::blob_store::{BlobAliasId, BlobStore};
use silverhand_backend_filesystem::foundation::blob_store::{LocalBlobAliasStore, LocalBlobStore};
use silverhand_backend_sqlite::conversation_messaging::messaging::MessagingStore;

/// An upload no message references after this long is abandoned (a draft that
/// was never sent, or a removed chip whose delete request failed).
const UNSENT_UPLOAD_RETENTION_MS: i64 = 12 * 60 * 60 * 1000;
use tokio::{sync::watch, task::JoinHandle};

pub(crate) struct ActivityRetention {
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ActivityRetention {
    pub(crate) async fn start<C: ProcessClock>(
        store: MessagingStore,
        hub: ScopeEventHub,
        blobs: LocalBlobStore,
        clock: C,
    ) -> Result<Self, crate::StartupError> {
        sweep(&store, &hub, clock.now_unix_millis()).await?;
        sweep_unsent_uploads(&store, &blobs, clock.now_unix_millis()).await;
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut deadline = next_shanghai_six(clock.now_unix_millis());
            loop {
                // Recheck wall time on bounded wakes, including clock adjustments.
                // Only reaching the calendar deadline performs a sweep.
                let now = clock.now_unix_millis();
                let delay = Duration::from_millis(deadline.saturating_sub(now).max(1) as u64)
                    .min(Duration::from_secs(60));
                tokio::select! {
                    biased;
                    _ = stopped.changed() => break,
                    _ = tokio::time::sleep(delay) => {}
                }
                let now = clock.now_unix_millis();
                if now < deadline {
                    continue;
                }
                if let Err(error) = sweep(&store, &hub, now).await {
                    tracing::warn!(%error, "activity retention sweep failed; next daily deadline retries");
                }
                sweep_unsent_uploads(&store, &blobs, now).await;
                deadline = next_shanghai_six(clock.now_unix_millis());
            }
        });
        Ok(Self {
            stop,
            task: Mutex::new(Some(task)),
        })
    }

    pub(crate) async fn close(&self) {
        let _ = self.stop.send(true);
        let task = self.task.lock().expect("retention task lock").take();
        if let Some(task) = task {
            // Let an in-flight SQLite statement finish before releasing its owner.
            let _ = task.await;
        }
    }
}

impl Drop for ActivityRetention {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().expect("retention task lock").take() {
            task.abort();
        }
    }
}

async fn sweep(
    store: &MessagingStore,
    hub: &ScopeEventHub,
    now: i64,
) -> Result<(), crate::StartupError> {
    let started = Instant::now();
    let deleted_count = store.prune_expired_activity(now).await.map_err(|source| {
        crate::StartupError::Composition {
            name: "activity retention",
            source: Box::new(source),
        }
    })?;
    hub.prune_activity_before(now.saturating_sub(ACTIVITY_RETENTION_MS))
        .map_err(|source| crate::StartupError::Composition {
            name: "activity replay retention",
            source: Box::new(source),
        })?;
    tracing::info!(target: "activity_retention",
        deleted_count,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "activity retention sweep completed"
    );
    Ok(())
}

/// Rows go first, in one writer transaction that rechecks references; blobs are
/// removed only after that commit. A failed blob delete leaves an orphaned file,
/// never a row pointing at a missing one.
async fn sweep_unsent_uploads(store: &MessagingStore, blobs: &LocalBlobStore, now: i64) {
    let deleted = match store
        .attachment_writes()
        .delete_unreferenced_uploads(now.saturating_sub(UNSENT_UPLOAD_RETENTION_MS))
        .await
    {
        Ok(deleted) => deleted,
        Err(error) => {
            tracing::warn!(%error, "unsent upload sweep failed; next daily deadline retries");
            return;
        }
    };
    let mut blob_failures = 0usize;
    for (id, blob) in &deleted {
        if blobs.delete(blob).await.is_err() {
            blob_failures += 1;
        }
        if let Ok(alias) = BlobAliasId::from_semantic_id(id.as_str()) {
            let _ = blobs.delete_alias(blob, &alias).await;
        }
    }
    tracing::info!(target: "activity_retention",
        deleted_count = deleted.len(),
        blob_failures,
        "unsent upload sweep completed"
    );
}

/// Strictly future local 06:00, independent of startup and sweep duration.
fn next_shanghai_six(now: i64) -> i64 {
    let local = chrono::DateTime::<Utc>::from_timestamp_millis(now)
        .expect("process clock is a representable timestamp")
        .with_timezone(&chrono_tz::Asia::Shanghai);
    let mut date = local.date_naive();
    loop {
        let next = chrono_tz::Asia::Shanghai
            .from_local_datetime(&date.and_hms_opt(6, 0, 0).unwrap())
            .single()
            .expect("Shanghai 06:00 is unambiguous")
            .timestamp_millis();
        if next > now {
            return next;
        }
        date = date.succ_opt().expect("next calendar day");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn successful_sweeps_log_actual_counts_and_failure_does_not() {
        use silverhand_backend_server::{
            conversation_messaging::messaging::collaboration::conversation_log_debug::ProductionConversationLogDebugControl,
            foundation::clock::SystemProcessClock,
        };
        use sqlx::{Connection, Executor};
        use tracing::instrument::WithSubscriber;
        use tracing_subscriber::{EnvFilter, layer::SubscriberExt};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("retention.sqlite");
        let mut db = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        db.execute("CREATE TABLE conversation_events(id INTEGER PRIMARY KEY,created_at INTEGER,payload TEXT); INSERT INTO conversation_events VALUES(1,9,'private-activity-marker'),(2,10,'private-activity-marker'),(3,11,'private-activity-marker')").await.unwrap();
        let store = MessagingStore::open(&path).await.unwrap();
        let hub = ScopeEventHub::production().unwrap();
        let log = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let file = std::sync::Arc::new(log.reopen().unwrap());
        let subscriber = tracing_subscriber::registry().with(crate::production_json_layer(
            EnvFilter::new("error,activity_retention=info"),
            ProductionConversationLogDebugControl::process(),
            SystemProcessClock,
            move || file.try_clone().unwrap(),
        ));
        async {
            sweep(&store, &hub, ACTIVITY_RETENTION_MS + 10)
                .await
                .unwrap();
            sweep(&store, &hub, ACTIVITY_RETENTION_MS + 10)
                .await
                .unwrap();
            db.execute("DROP TABLE conversation_events").await.unwrap();
            assert!(
                sweep(&store, &hub, ACTIVITY_RETENTION_MS + 10)
                    .await
                    .is_err()
            );
        }
        .with_subscriber(subscriber)
        .await;
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(!text.contains("private-activity-marker"));
        assert!(!text.contains("next_run"));
        let events: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2, "no success for failed deletion");
        for (event, count) in events.iter().zip([1, 0]) {
            assert_eq!(event["target"], "activity_retention");
            assert!(event["timestamp"].is_string());
            assert_eq!(event["fields"]["deleted_count"], count);
            assert!(event["fields"]["elapsed_ms"].as_f64().unwrap() >= 0.0);
            assert_eq!(event["fields"].as_object().unwrap().len(), 3);
        }
    }

    #[test]
    fn daily_deadline_is_strictly_next_shanghai_six() {
        let six = chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2026, 9, 7, 6, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(next_shanghai_six(six - 1), six);
        assert_eq!(next_shanghai_six(six), six + 86_400_000);
        assert_eq!(next_shanghai_six(six + 1), six + 86_400_000);
        assert_eq!(next_shanghai_six(six + 20 * 3_600_000), six + 86_400_000);
    }
}
