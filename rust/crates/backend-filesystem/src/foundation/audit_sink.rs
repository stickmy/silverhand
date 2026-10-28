//! The production, composition-owned sink that durably records `cli.preflight_failure`
//! audit lines.
//!
//! # Where this sits
//!
//! The domain owns the record and the port ([`silverhand_backend_core::foundation::cli_preflight_audit`]);
//! this module is the ONE concrete implementation of that port — an OS adapter, because
//! its entire job is touching the filesystem and the wall clock. The composition root
//! constructs it, owns its lifecycle, and injects it as the port; a handler never sees
//! this type.
//!
//! # What it preserves
//!
//! The CLI audit sink writes each preflight failure as one JSON object per line to
//! `<log-dir>/cli.YYYY-MM-DD.log`, rotating daily in UTC with 14-file retention.
//! Historical `cli-<utc>.log` files expire after 14 days without writes. Operators
//! query these files with `tail`/`jq`/`grep`. The record contract preserves the same set of
//! independently-addressable top-level keys with the same casing, absent optionals as
//! explicit `null`, an absent flag list as `[]`, and a line that persists regardless of
//! any configured log level (the product force-writes this category past its threshold).
//!
//! # Two layers, one lifecycle
//!
//! [`CategorizedJsonlWriter`] is the reusable primitive: it opens one categorized log
//! file, owns a single background writer task fed by a *bounded* channel, and exposes a
//! non-blocking enqueue plus an idempotent drain/flush/close. It knows nothing about the
//! preflight record — it appends opaque JSON lines. [`CliPreflightFileSink`] is the thin
//! typed facade over one such writer that projects a [`CliPreflightFailure`] into the
//! exact wire line. A second audit surface reuses the SAME primitive with its own prefix
//! and facade — never a second file/lifecycle implementation, and never this file's
//! `cli.*.log`.
//!
//! # Failure policy
//!
//! Recording is best-effort and can never fail a request. Two failure classes are kept
//! distinct, exactly as the review required:
//! - *Transient backpressure*: the bounded queue is full. The line is dropped with an
//!   observable signal (a drop counter and a non-recursive `tracing` warning); the
//!   writer stays **healthy**.
//! - *Durable failure*: an actual open/write/flush error or writer-task termination. The
//!   writer transitions to a terminal **failed/closed** state, emits a non-recursive
//!   warning, and never again pretends a line was written.
//!
//! The diagnostic warnings go to the ordinary process `tracing` subscriber (stdout),
//! never back through this sink — there is no recursion path.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use silverhand_backend_core::foundation::cli_invoke_audit::{CliInvoke, CliInvokeAuditSink};
use silverhand_backend_core::foundation::cli_preflight_audit::{
    CliPreflightAuditSink, CliPreflightFailure,
};
use silverhand_backend_core::identity_access::agent_audit_read::{AgentAuditEvent, AgentAuditSink};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

/// The `tracing` target every non-recursive diagnostic from the reusable writer carries,
/// so an operator (and a test subscriber) can isolate categorized-audit health from
/// application logs. It is category-neutral on purpose: this primitive is shared by more
/// than one audit surface (CLI preflight today, agent-audit next), and each diagnostic
/// already carries the concrete `file` (its `<prefix>.*.log`), so a failing writer
/// is attributed to the right category by its filename rather than by a target hard-wired
/// to one caller.
const DIAGNOSTIC_TARGET: &str = "silverhand::categorized_audit";

/// The categorized-log filename prefix for CLI audit lines: `cli.YYYY-MM-DD.log`.
const CLI_PREFIX: &str = "cli";

/// The categorized-log filename prefix for agent owner-audit evidence lines:
/// `agent-audit.YYYY-MM-DD.log`. A DISTINCT file from `cli.*.log` — the two audit surfaces
/// reuse the same [`CategorizedJsonlWriter`] primitive but never the same file.
const AGENT_AUDIT_PREFIX: &str = "agent-audit";

/// How many enqueued-but-unwritten lines the bounded channel holds before it applies
/// backpressure by dropping. Preflight failures are rare and each line is tiny, so this
/// is generous headroom for a burst while still being a hard ceiling — the queue can
/// never grow without bound and wedge memory behind a stalled disk.
const QUEUE_CAPACITY: usize = 1024;

const HEALTHY: u8 = 0;
const FAILED_IO: u8 = 1;
const CLOSED_SHUTDOWN: u8 = 2;

/// The observable health of a [`CategorizedJsonlWriter`]'s background task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterState {
    /// The task is running and accepting lines.
    Healthy,
    /// The task terminated on an actual open/write/flush error. It writes nothing more.
    Failed,
    /// The task drained and closed cleanly at shutdown. It writes nothing more.
    ClosedShutdown,
}

/// The wall clock this sink stamps each line's `ts` with, and derives the per-boot
/// filename from. Injected rather than read inline so a test can pin a deterministic
/// instant; production supplies the one process clock, never a second wall-clock reader.
pub trait AuditClock: Clone + Send + Sync + 'static {
    /// The current time as signed Unix milliseconds (`Date.now()` parity).
    fn now_unix_millis(&self) -> i64;
}

/// The `app` base field, chosen once by the launcher's deployment profile — never
/// inferred per line. Mirrors the product's per-launcher log base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppTag {
    /// The embedded desktop launcher.
    Desktop,
    /// The remote-only server launcher.
    Server,
}

impl AppTag {
    /// The exact string the product's log base carries for this launcher.
    pub fn as_str(self) -> &'static str {
        match self {
            AppTag::Desktop => "silverhand-desktop",
            AppTag::Server => "silverhand-server",
        }
    }
}

struct CloseState {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

/// A single categorized JSON-lines log file with a background writer task.
///
/// Cloning shares the one channel, the one task, and the one close latch — every clone
/// enqueues to the same file and any clone can close it exactly once. There is never a
/// second task or a second file behind two clones.
#[derive(Clone)]
pub struct CategorizedJsonlWriter {
    tx: mpsc::Sender<Vec<u8>>,
    health: Arc<AtomicU8>,
    dropped_lines: Arc<AtomicU64>,
    filename: Arc<str>,
    close: Arc<Mutex<CloseState>>,
}

impl CategorizedJsonlWriter {
    /// Opens the current UTC daily file and owns rotation and retention thereafter.
    pub async fn open<C: AuditClock>(
        dir: PathBuf,
        prefix: &'static str,
        clock: C,
    ) -> std::io::Result<Self> {
        let now = clock.now_unix_millis();
        let mut rotation = DailyAuditRotation {
            dir,
            prefix,
            clock: Box::new(move || clock.now_unix_millis()),
            next_day: 0,
        };
        let file = rotation.open_day(now).await?;
        Ok(Self::spawn_output(
            Box::new(file),
            format!("{prefix}.*.log"),
            QUEUE_CAPACITY,
            Some(rotation),
        ))
    }

    #[cfg(test)]
    fn spawn<W>(writer: W, filename: String, capacity: usize) -> Self
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::spawn_output(Box::new(writer), filename, capacity, None)
    }

    fn spawn_output(
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        filename: String,
        capacity: usize,
        rotation: Option<DailyAuditRotation>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(capacity);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let health = Arc::new(AtomicU8::new(HEALTHY));
        let filename: Arc<str> = Arc::from(filename);
        let join = tokio::spawn(drive_writer(
            writer,
            rx,
            shutdown_rx,
            filename.clone(),
            health.clone(),
            rotation,
        ));
        Self {
            tx,
            health,
            dropped_lines: Arc::new(AtomicU64::new(0)),
            filename,
            close: Arc::new(Mutex::new(CloseState {
                shutdown_tx: Some(shutdown_tx),
                join: Some(join),
            })),
        }
    }

    /// Enqueues one already-serialized line for the writer task. Never blocks and never
    /// fails the caller. A full queue drops the line as transient backpressure (the
    /// writer stays healthy); a closed queue drops it because the task has terminated.
    /// Both increment the drop counter and emit one non-recursive diagnostic.
    pub fn enqueue(&self, line: Vec<u8>) {
        match self.tx.try_send(line) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped_lines.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: DIAGNOSTIC_TARGET,
                    file = self.filename.as_ref(),
                    "categorized audit writer queue is full; dropping this line as transient backpressure (writer stays healthy)"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.dropped_lines.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    target: DIAGNOSTIC_TARGET,
                    file = self.filename.as_ref(),
                    state = format!("{:?}", self.state()),
                    "categorized audit writer is closed; dropping this line"
                );
            }
        }
    }

    /// The writer task's current health.
    pub fn state(&self) -> WriterState {
        match self.health.load(Ordering::SeqCst) {
            FAILED_IO => WriterState::Failed,
            CLOSED_SHUTDOWN => WriterState::ClosedShutdown,
            _ => WriterState::Healthy,
        }
    }

    /// How many lines have been dropped (full or closed) over this writer's life.
    pub fn dropped_lines(&self) -> u64 {
        self.dropped_lines.load(Ordering::Relaxed)
    }

    /// Signals the writer task to drain the queue, flush, and close its file, then awaits
    /// its termination. Idempotent: the first caller performs the close and every later
    /// caller (including the second shutdown wiring) is a no-op that returns immediately.
    pub async fn close(&self) {
        let mut guard = self.close.lock().await;
        if let Some(shutdown_tx) = guard.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(join) = guard.join.take()
            && let Err(join_error) = join.await
        {
            // The writer task panicked or was cancelled — worker termination, which
            // the failure policy classifies as a durable failure, never a clean
            // close. Fail closed and observably: force `Failed` (never leaving a
            // `Healthy`/`ClosedShutdown` illusion) and emit one non-recursive warning.
            self.health.store(FAILED_IO, Ordering::SeqCst);
            tracing::warn!(
                target: DIAGNOSTIC_TARGET,
                file = self.filename.as_ref(),
                error = join_error.to_string(),
                "categorized audit writer task terminated abnormally; the writer is now failed"
            );
        }
    }
}

async fn drive_writer(
    mut writer: Box<dyn AsyncWrite + Unpin + Send>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    mut shutdown_rx: oneshot::Receiver<()>,
    filename: Arc<str>,
    health: Arc<AtomicU8>,
    mut rotation: Option<DailyAuditRotation>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                // Graceful shutdown is an accepted-line barrier. Closing the receiver
                // FIRST makes every concurrent `try_send` from another clone observe
                // `Closed` (an observable drop) instead of racing this drain: without it,
                // a clone could `try_send == Ok` in the window between "queue looks empty"
                // and "task returns", and that accepted line would never reach disk yet
                // the writer would still report a clean close. After closing intake we
                // drain to channel EOF with `recv().await` — draining EVERY line accepted
                // before the barrier, never stopping on a transient empty moment — then
                // flush+close.
                rx.close();
                while let Some(line) = rx.recv().await {
                    if let Err(error) = write_line(&mut writer, &line, &mut rotation).await {
                        report_durable_failure(&filename, &health, &error);
                        return;
                    }
                }
                finish(&mut writer, &filename, &health).await;
                return;
            }
            received = rx.recv() => match received {
                Some(line) => {
                    if let Err(error) = write_line(&mut writer, &line, &mut rotation).await {
                        report_durable_failure(&filename, &health, &error);
                        return;
                    }
                }
                None => {
                    // Every sender was dropped without an explicit close: flush+close.
                    finish(&mut writer, &filename, &health).await;
                    return;
                }
            }
        }
    }
}

async fn write_line(
    writer: &mut Box<dyn AsyncWrite + Unpin + Send>,
    line: &[u8],
    rotation: &mut Option<DailyAuditRotation>,
) -> std::io::Result<()> {
    if let Some(rotation) = rotation {
        let now = (rotation.clock)();
        if now >= rotation.next_day {
            let next = rotation.open_day(now).await?;
            writer.flush().await?;
            writer.shutdown().await?;
            *writer = Box::new(next);
        }
    }
    writer.write_all(line).await?;
    writer.write_all(b"\n").await?;
    // Flush per line: preflight failures are infrequent and each line must be durable
    // the instant it is recorded, so an operator tailing the file sees it without
    // waiting for a buffer to fill or the process to exit.
    writer.flush().await
}

const DAY_MILLIS: i64 = 86_400_000;
const AUDIT_RETAIN_FILES: usize = 14;

struct DailyAuditRotation {
    dir: PathBuf,
    prefix: &'static str,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
    next_day: i64,
}

impl DailyAuditRotation {
    async fn open_day(&mut self, now: i64) -> std::io::Result<tokio::fs::File> {
        let name = format!("{}.{}.log", self.prefix, &iso8601_utc_millis(now)[..10]);
        tokio::fs::create_dir_all(&self.dir).await?;
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(&name))
            .await?;
        self.next_day = (now.div_euclid(DAY_MILLIS) + 1) * DAY_MILLIS;
        if let Err(error) = self.prune(&name, now).await {
            tracing::warn!(target: DIAGNOSTIC_TARGET, %error, "Audit log cleanup failed");
        }
        Ok(file)
    }

    async fn prune(&self, current: &str, now: i64) -> std::io::Result<()> {
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        let mut daily = Vec::new();
        let mut expired = Vec::new();
        let daily_prefix = format!("{}.", self.prefix);
        let legacy_prefix = format!("{}-", self.prefix);
        let cutoff = std::time::UNIX_EPOCH
            + std::time::Duration::from_millis((now - 14 * DAY_MILLIS).max(0) as u64);
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches_log_name(&name, &daily_prefix, b"dddd-dd-dd") {
                daily.push(name);
            } else if matches_log_name(&name, &legacy_prefix, b"dddd-dd-ddTdd-dd-dd-dddZ") {
                let modified = entry.metadata().await?.modified()?;
                if modified < cutoff {
                    expired.push(name);
                }
            }
        }
        daily.sort();
        let excess = daily.len().saturating_sub(AUDIT_RETAIN_FILES);
        expired.extend(daily.into_iter().take(excess));
        for name in expired {
            if name == current {
                continue;
            }
            if let Err(error) = tokio::fs::remove_file(self.dir.join(&name)).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(target: DIAGNOSTIC_TARGET, file = name, %error, "Audit log cleanup failed");
            }
        }
        Ok(())
    }
}

fn matches_log_name(name: &str, prefix: &str, pattern: &[u8]) -> bool {
    let Some(stamp) = name
        .strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(".log"))
    else {
        return false;
    };
    stamp.len() == pattern.len()
        && stamp.bytes().zip(pattern).all(|(c, p)| {
            if *p == b'd' {
                c.is_ascii_digit()
            } else {
                c == *p
            }
        })
}

async fn finish<W>(writer: &mut W, filename: &Arc<str>, health: &Arc<AtomicU8>)
where
    W: AsyncWrite + Unpin,
{
    if let Err(error) = writer.flush().await {
        report_durable_failure(filename, health, &error);
        return;
    }
    // A close-time `shutdown` I/O fault is a durable failure like any other write/flush
    // fault, not a clean close: it must NOT leave a `ClosedShutdown` illusion behind a
    // sink that failed to finalize its file.
    if let Err(error) = writer.shutdown().await {
        report_durable_failure(filename, health, &error);
        return;
    }
    health.store(CLOSED_SHUTDOWN, Ordering::SeqCst);
}

fn report_durable_failure(filename: &Arc<str>, health: &Arc<AtomicU8>, error: &std::io::Error) {
    health.store(FAILED_IO, Ordering::SeqCst);
    tracing::warn!(
        target: DIAGNOSTIC_TARGET,
        file = filename.as_ref(),
        error = error.to_string(),
        "categorized audit writer write failed; the writer is now failed and will record nothing more"
    );
}

#[derive(Clone)]
struct CliAuditShared<C: AuditClock> {
    writer: CategorizedJsonlWriter,
    clock: C,
    app: AppTag,
    pid: u32,
}

/// The sole lifecycle owner of the process's daily `cli.*.log` files.
pub struct CliAuditFile<C: AuditClock> {
    shared: CliAuditShared<C>,
}

impl<C: AuditClock> CliAuditFile<C> {
    pub async fn open(dir: PathBuf, app: AppTag, clock: C) -> std::io::Result<Self> {
        let writer = CategorizedJsonlWriter::open(dir, CLI_PREFIX, clock.clone()).await?;
        Ok(Self {
            shared: CliAuditShared {
                writer,
                clock,
                app,
                pid: std::process::id(),
            },
        })
    }

    pub fn preflight_sink(&self) -> CliPreflightFileSink<C> {
        CliPreflightFileSink {
            shared: self.shared.clone(),
        }
    }

    pub fn invoke_sink(&self) -> CliInvokeFileSink<C> {
        CliInvokeFileSink {
            shared: self.shared.clone(),
        }
    }

    pub async fn close(&self) {
        self.shared.writer.close().await;
    }

    pub fn state(&self) -> WriterState {
        self.shared.writer.state()
    }

    pub fn dropped_lines(&self) -> u64 {
        self.shared.writer.dropped_lines()
    }
}

/// Narrow preflight producer facade. It deliberately has no open/close lifecycle.
#[derive(Clone)]
pub struct CliPreflightFileSink<C: AuditClock> {
    shared: CliAuditShared<C>,
}

impl<C: AuditClock> CliPreflightFileSink<C> {
    pub fn state(&self) -> WriterState {
        self.shared.writer.state()
    }

    pub fn dropped_lines(&self) -> u64 {
        self.shared.writer.dropped_lines()
    }

    fn line_bytes(&self, failure: &CliPreflightFailure) -> Vec<u8> {
        use serde_json::{Map, Number, Value};

        let ts = iso8601_utc_millis(self.shared.clock.now_unix_millis());
        let mut line = Map::new();
        // Envelope, mirroring the product's `{ ts, level, msg, ...base }` prefix.
        line.insert("ts".to_owned(), Value::String(ts));
        line.insert("level".to_owned(), Value::String("warn".to_owned()));
        line.insert(
            "msg".to_owned(),
            Value::String("cli.preflight_failure".to_owned()),
        );
        line.insert(
            "app".to_owned(),
            Value::String(self.shared.app.as_str().to_owned()),
        );
        line.insert(
            "pid".to_owned(),
            Value::Number(Number::from(self.shared.pid)),
        );
        // Business fields, each an independently-addressable top-level key with the exact
        // casing the live contract uses. `category`/`status` are stamped constants.
        line.insert(
            "category".to_owned(),
            Value::String("cli.preflight_failure".to_owned()),
        );
        line.insert(
            "agentId".to_owned(),
            Value::String(failure.agent_id.as_str().to_owned()),
        );
        line.insert(
            "conversationId".to_owned(),
            match &failure.conversation_id {
                Some(id) => Value::String(id.as_str().to_owned()),
                None => Value::Null,
            },
        );
        line.insert("noun".to_owned(), opt_string(&failure.noun));
        line.insert("verb".to_owned(), opt_string(&failure.verb));
        line.insert(
            "flags_seen".to_owned(),
            Value::Array(
                failure
                    .flags_seen
                    .iter()
                    .map(|flag| Value::String(flag.clone()))
                    .collect(),
            ),
        );
        line.insert("error_code".to_owned(), opt_string(&failure.error_code));
        line.insert(
            "error_subkind".to_owned(),
            opt_string(&failure.error_subkind),
        );
        line.insert(
            "error_message".to_owned(),
            opt_string(&failure.error_message),
        );
        line.insert(
            "duration_ms".to_owned(),
            match failure.duration_ms {
                // A finite, non-negative f64 always maps to a JSON number; the fallback
                // is unreachable but keeps the projection total rather than panicking.
                Some(duration) => Number::from_f64(duration.get())
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                None => Value::Null,
            },
        );
        line.insert("status".to_owned(), Value::String("failure".to_owned()));

        // Object serialization is infallible for this map (only strings, numbers, null,
        // and a string array); the fallback keeps `record` non-panicking.
        serde_json::to_vec(&Value::Object(line)).unwrap_or_default()
    }
}

impl<C: AuditClock> CliPreflightAuditSink for CliPreflightFileSink<C> {
    fn record(&self, failure: CliPreflightFailure) {
        let line = self.line_bytes(&failure);
        self.shared.writer.enqueue(line);
    }
}

/// Narrow invocation producer facade. It shares the owner's writer and has no lifecycle.
#[derive(Clone)]
pub struct CliInvokeFileSink<C: AuditClock> {
    shared: CliAuditShared<C>,
}

impl<C: AuditClock> CliInvokeFileSink<C> {
    fn line_bytes(&self, invoke: &CliInvoke) -> Vec<u8> {
        use serde_json::{Map, Number, Value};

        let success = (200..400).contains(&invoke.http_status);
        let level = if success {
            "info"
        } else if invoke.http_status >= 500 || invoke.error_kind.is_some() {
            "error"
        } else {
            "warn"
        };
        let mut line = Map::new();
        line.insert(
            "ts".to_owned(),
            Value::String(iso8601_utc_millis(self.shared.clock.now_unix_millis())),
        );
        line.insert("level".to_owned(), Value::String(level.to_owned()));
        line.insert("msg".to_owned(), Value::String("cli.invoke".to_owned()));
        line.insert(
            "app".to_owned(),
            Value::String(self.shared.app.as_str().to_owned()),
        );
        line.insert(
            "pid".to_owned(),
            Value::Number(Number::from(self.shared.pid)),
        );
        line.insert(
            "category".to_owned(),
            Value::String("cli.invoke".to_owned()),
        );
        line.insert(
            "request_id".to_owned(),
            Value::String(invoke.request_id.clone()),
        );
        line.insert(
            "agentId".to_owned(),
            Value::String(invoke.agent_id.as_str().to_owned()),
        );
        line.insert(
            "noun".to_owned(),
            invoke
                .route
                .map(|route| Value::String(route.noun.to_owned()))
                .unwrap_or(Value::Null),
        );
        line.insert(
            "verb".to_owned(),
            invoke
                .route
                .map(|route| Value::String(route.verb.to_owned()))
                .unwrap_or(Value::Null),
        );
        line.insert(
            "method".to_owned(),
            Value::String(invoke.method.as_str().to_owned()),
        );
        line.insert(
            "route_suffix".to_owned(),
            invoke
                .route
                .map(|route| Value::String(route.route_suffix.to_owned()))
                .unwrap_or(Value::Null),
        );
        line.insert(
            "query_keys".to_owned(),
            Value::Array(
                invoke
                    .query_keys
                    .iter()
                    .map(|key| Value::String((*key).to_owned()))
                    .collect(),
            ),
        );
        line.insert(
            "http_status".to_owned(),
            Value::Number(Number::from(invoke.http_status)),
        );
        line.insert(
            "duration_ms".to_owned(),
            Number::from_f64(invoke.duration_ms.get())
                .map(Value::Number)
                .unwrap_or(Value::Null),
        );
        line.insert(
            "status".to_owned(),
            Value::String(if success { "success" } else { "failure" }.to_owned()),
        );
        if let Some(code) = invoke.error_code {
            line.insert(
                "error_code".to_owned(),
                Value::String(code.as_str().to_owned()),
            );
        }
        if let Some(code) = &invoke.refusal_code {
            line.insert(
                "refusal_code".to_owned(),
                Value::String(code.as_str().to_owned()),
            );
        }
        if let Some(conversation) = &invoke.conversation_id {
            line.insert(
                "conversation_id".to_owned(),
                Value::String(conversation.as_str().to_owned()),
            );
        }
        if !invoke.error_issues.is_empty() {
            line.insert(
                "error_issues".to_owned(),
                Value::Array(
                    invoke
                        .error_issues
                        .as_slice()
                        .iter()
                        .map(|issue| {
                            serde_json::json!({
                                "path": issue.path.as_str(),
                                "code": issue.code.as_str(),
                            })
                        })
                        .collect(),
                ),
            );
        }
        if let Some(reason) = invoke.stale_resolution_reason {
            line.insert(
                "stale_resolution_reason".to_owned(),
                Value::String(reason.as_str().to_owned()),
            );
        }
        if let Some(kind) = invoke.error_kind {
            line.insert(
                "error_kind".to_owned(),
                Value::String(kind.as_str().to_owned()),
            );
        }
        serde_json::to_vec(&Value::Object(line)).unwrap_or_default()
    }
}

impl<C: AuditClock> CliInvokeAuditSink for CliInvokeFileSink<C> {
    fn record(&self, invoke: CliInvoke) {
        let line = self.line_bytes(&invoke);
        self.shared.writer.enqueue(line);
    }
}

fn opt_string(value: &Option<String>) -> serde_json::Value {
    match value {
        Some(text) => serde_json::Value::String(text.clone()),
        None => serde_json::Value::Null,
    }
}

/// Formats signed Unix milliseconds as a UTC ISO-8601 instant with millisecond
/// precision and a `Z` suffix — exactly `Date.now()` fed to `new Date(_).toISOString()`.
///
/// Pure and clock-free so the whole edge is pinned by fixed-input tests. Uses Howard
/// Hinnant's `civil_from_days` so it is correct across leap years and, like the product,
/// for pre-1970 instants (a negative millisecond count) as well as positive ones.
fn iso8601_utc_millis(unix_millis: i64) -> String {
    let days = unix_millis.div_euclid(86_400_000);
    let ms_of_day = unix_millis.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hour = ms_of_day / 3_600_000;
    let minute = (ms_of_day / 60_000) % 60;
    let second = (ms_of_day / 1_000) % 60;
    let milli = ms_of_day % 1_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}Z")
}

/// Civil (year, month, day) from a count of days since the Unix epoch (1970-01-01).
/// Howard Hinnant's algorithm, valid for the entire representable range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// The production agent owner-audit sink: a typed facade over one
/// [`CategorizedJsonlWriter`] that projects an [`AgentAuditEvent`] into one flat,
/// per-field-addressable `agent-audit.YYYY-MM-DD.log` line.
///
/// This is the ONE durable backing #task:2256's reusable writer feeds for #task:2254's
/// owner-audit seam: it reuses the exact same primitive (bounded queue, single writer
/// task, idempotent drain/close, best-effort non-blocking record) with its OWN prefix
/// and its OWN projection — never `tracing`, never a generic event bus, never the CLI
/// record, and never a second writer implementation. Cloning shares the one underlying
/// writer, so a clone injected into router state and one kept for shutdown close the
/// SAME file.
#[derive(Clone)]
pub struct AgentAuditFileSink<C: AuditClock> {
    writer: CategorizedJsonlWriter,
    clock: C,
    app: AppTag,
    pid: u32,
}

impl<C: AuditClock> AgentAuditFileSink<C> {
    /// Opens the daily `agent-audit.YYYY-MM-DD.log` sink under `dir`.
    /// An open failure is surfaced so the composition root can fail startup.
    pub async fn open(dir: PathBuf, app: AppTag, clock: C) -> std::io::Result<Self> {
        let writer = CategorizedJsonlWriter::open(dir, AGENT_AUDIT_PREFIX, clock.clone()).await?;
        Ok(Self {
            writer,
            clock,
            app,
            pid: std::process::id(),
        })
    }

    /// Drains, flushes, and closes the underlying file. Idempotent (see
    /// [`CategorizedJsonlWriter::close`]).
    pub async fn close(&self) {
        self.writer.close().await;
    }

    /// The underlying writer's health, for shutdown-close witnessing and diagnostics.
    pub fn state(&self) -> WriterState {
        self.writer.state()
    }

    /// How many lines this sink has dropped over its life.
    pub fn dropped_lines(&self) -> u64 {
        self.writer.dropped_lines()
    }

    fn line_bytes(&self, event: &AgentAuditEvent) -> Vec<u8> {
        use serde_json::{Map, Number, Value};

        let ts = iso8601_utc_millis(self.clock.now_unix_millis());
        let mut line = Map::new();
        // Envelope, mirroring the CLI sink's `{ ts, level, msg, ...base }` prefix. A
        // successful audited read is `info`, not the CLI failure's `warn`.
        line.insert("ts".to_owned(), Value::String(ts));
        line.insert("level".to_owned(), Value::String("info".to_owned()));
        line.insert(
            "msg".to_owned(),
            Value::String("agent.audit_event".to_owned()),
        );
        line.insert(
            "app".to_owned(),
            Value::String(self.app.as_str().to_owned()),
        );
        line.insert("pid".to_owned(), Value::Number(Number::from(self.pid)));
        // Business fields: `category` is the per-variant group-by key an operator greps,
        // and every id/count is its own independently-addressable top-level key. Ids reuse
        // the CLI line's camelCase for the shared identifiers (`agentId`,
        // `conversationId`); this is a new evidence surface with no prior wire reader, so
        // the shape is internally consistent rather than inherited.
        match event {
            AgentAuditEvent::ChatsListed {
                viewer,
                agent_id,
                chat_count,
            } => {
                line.insert(
                    "category".to_owned(),
                    Value::String("agent.chats_listed".to_owned()),
                );
                line.insert(
                    "viewer".to_owned(),
                    Value::String(viewer.as_str().to_owned()),
                );
                line.insert(
                    "agentId".to_owned(),
                    Value::String(agent_id.as_str().to_owned()),
                );
                line.insert(
                    "chatCount".to_owned(),
                    Value::Number(Number::from(*chat_count)),
                );
            }
            AgentAuditEvent::TranscriptRead {
                viewer,
                agent_id,
                conversation_id,
                message_count,
            } => {
                line.insert(
                    "category".to_owned(),
                    Value::String("agent.transcript_read".to_owned()),
                );
                line.insert(
                    "viewer".to_owned(),
                    Value::String(viewer.as_str().to_owned()),
                );
                line.insert(
                    "agentId".to_owned(),
                    Value::String(agent_id.as_str().to_owned()),
                );
                line.insert(
                    "conversationId".to_owned(),
                    Value::String(conversation_id.as_str().to_owned()),
                );
                line.insert(
                    "messageCount".to_owned(),
                    Value::Number(Number::from(*message_count)),
                );
            }
            AgentAuditEvent::AttachmentRead {
                viewer,
                agent_id,
                conversation_id,
                attachment_id,
            } => {
                line.insert(
                    "category".to_owned(),
                    Value::String("agent.attachment_read".to_owned()),
                );
                line.insert(
                    "viewer".to_owned(),
                    Value::String(viewer.as_str().to_owned()),
                );
                line.insert(
                    "agentId".to_owned(),
                    Value::String(agent_id.as_str().to_owned()),
                );
                line.insert(
                    "conversationId".to_owned(),
                    Value::String(conversation_id.as_str().to_owned()),
                );
                line.insert(
                    "attachmentId".to_owned(),
                    Value::String(attachment_id.as_str().to_owned()),
                );
            }
        }
        line.insert("status".to_owned(), Value::String("success".to_owned()));

        // Object serialization is infallible for this map (only strings and numbers); the
        // fallback keeps `record` non-panicking.
        serde_json::to_vec(&Value::Object(line)).unwrap_or_default()
    }
}

impl<C: AuditClock> AgentAuditSink for AgentAuditFileSink<C> {
    fn record(&self, event: AgentAuditEvent) {
        let line = self.line_bytes(&event);
        self.writer.enqueue(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silverhand_backend_core::foundation::cli_invoke_audit::{
        CliInvoke, CliInvokeAuditSink, CliInvokeErrorCode, CliInvokeIssue, CliInvokeIssueCode,
        CliInvokeIssues, CliInvokeMethod, CliInvokeRoute,
    };
    use silverhand_backend_core::foundation::cli_preflight_audit::NonNegativeFiniteF64;
    use silverhand_backend_core::foundation::identity::AgentId;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Clone)]
    struct FixedClock(i64);

    impl AuditClock for FixedClock {
        fn now_unix_millis(&self) -> i64 {
            self.0
        }
    }

    #[derive(Clone)]
    struct MovingClock(Arc<std::sync::atomic::AtomicI64>);

    impl AuditClock for MovingClock {
        fn now_unix_millis(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn audit_daily_rotation_appends_restarts_expires_old_files_and_preserves_other_logs() {
        for prefix in [CLI_PREFIX, AGENT_AUDIT_PREFIX] {
            let dir = tempfile::tempdir().unwrap();
            let day = ANCHOR_MS.div_euclid(DAY_MILLIS) * DAY_MILLIS;
            let clock = MovingClock(Arc::new(std::sync::atomic::AtomicI64::new(day)));
            let name = |now| format!("{prefix}.{}.log", &iso8601_utc_millis(now)[..10]);
            for age in 1..20 {
                std::fs::write(dir.path().join(name(day - age * DAY_MILLIS)), "old\n").unwrap();
            }
            let old = dir
                .path()
                .join(format!("{prefix}-2023-01-01T00-00-00-000Z.log"));
            std::fs::write(&old, "expired").unwrap();
            let recent = dir
                .path()
                .join(format!("{prefix}-2023-01-02T00-00-00-000Z.log"));
            std::fs::write(&recent, "recent write").unwrap();
            for (path, age) in [(&old, 15), (&recent, 1)] {
                std::fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_modified(
                        std::time::UNIX_EPOCH
                            + std::time::Duration::from_millis((day - age * DAY_MILLIS) as u64),
                    )
                    .unwrap();
            }
            let untouched = [
                "rust-backend.2023-01-01.log",
                "daemon-2023-01-01T00-00-00-000Z.log",
                "cli-not-a-timestamp.log",
            ];
            for file in untouched {
                std::fs::write(dir.path().join(file), "keep").unwrap();
            }
            for text in [b"first".to_vec(), b"restart".to_vec()] {
                let owner =
                    CategorizedJsonlWriter::open(dir.path().to_path_buf(), prefix, clock.clone())
                        .await
                        .unwrap();
                owner.enqueue(text);
                owner.close().await;
            }
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name(day))).unwrap(),
                "first\nrestart\n"
            );
            assert!(!old.exists());
            assert!(recent.exists());
            assert!(!dir.path().join(name(day - 14 * DAY_MILLIS)).exists());
            let owner =
                CategorizedJsonlWriter::open(dir.path().to_path_buf(), prefix, clock.clone())
                    .await
                    .unwrap();
            clock.0.store(day + DAY_MILLIS, Ordering::SeqCst);
            owner.enqueue(b"next day".to_vec());
            owner.close().await;
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name(day + DAY_MILLIS))).unwrap(),
                "next day\n"
            );
            let daily = std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .filter(|e| {
                    matches_log_name(
                        &e.file_name().to_string_lossy(),
                        &format!("{prefix}."),
                        b"dddd-dd-dd",
                    )
                })
                .count();
            assert_eq!(daily, 14);
            for file in untouched {
                assert_eq!(
                    std::fs::read_to_string(dir.path().join(file)).unwrap(),
                    "keep"
                );
            }
        }
    }

    #[tokio::test]
    async fn cli_rotation_open_failure_preserves_the_existing_durable_failure_contract() {
        let dir = tempfile::tempdir().unwrap();
        let clock = MovingClock(Arc::new(std::sync::atomic::AtomicI64::new(ANCHOR_MS)));
        let owner = CliAuditFile::open(dir.path().to_path_buf(), AppTag::Desktop, clock.clone())
            .await
            .unwrap();
        std::fs::create_dir(dir.path().join("cli.2023-11-15.log")).unwrap();
        clock.0.store(ANCHOR_MS + DAY_MILLIS, Ordering::SeqCst);
        owner.shared.writer.enqueue(b"cannot persist".to_vec());
        owner.close().await;
        assert_eq!(owner.shared.writer.state(), WriterState::Failed);
        owner.shared.writer.enqueue(b"after failure".to_vec());
        assert_eq!(owner.shared.writer.dropped_lines(), 1);
    }

    // 2023-11-14T22:13:20.123Z in milliseconds.
    const ANCHOR_MS: i64 = 1_700_000_123_123;

    #[test]
    fn iso_matches_date_now_to_iso_string_at_known_instants() {
        assert_eq!(iso8601_utc_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            iso8601_utc_millis(1_700_000_000_123),
            "2023-11-14T22:13:20.123Z"
        );
        // A leap day, to exercise civil_from_days across a leap year.
        // 2024-02-29T00:00:00.000Z = 1709164800000 ms.
        assert_eq!(
            iso8601_utc_millis(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
    }

    /// An `AsyncWrite` that parks forever on its first write, signalling once it has been
    /// polled so a test knows the writer task is wedged mid-write with the queue empty.
    struct BlockingWriter {
        started: Option<oneshot::Sender<()>>,
    }

    impl AsyncWrite for BlockingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    /// An `AsyncWrite` whose every write fails, to drive the durable-failure transition.
    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other("disk gone")))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("disk gone")))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// An `AsyncWrite` whose writes and flushes succeed but whose `shutdown` fails, to
    /// prove a close-time I/O fault is a durable failure and not a faked clean close.
    struct FlushOkShutdownFailsWriter;

    impl AsyncWrite for FlushOkShutdownFailsWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("close syscall failed")))
        }
    }

    /// An `AsyncWrite` whose first write panics, so the spawned writer task terminates
    /// abnormally and `close()` observes a `JoinHandle` error.
    struct PanickingWriter;

    impl AsyncWrite for PanickingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            panic!("writer task blew up mid-write")
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// An `AsyncWrite` that records every written byte into a shared buffer, and whose
    /// `shutdown` parks (signalling once it has begun) until the test releases it. Letting
    /// the writer task sit inside `finish`'s `shutdown` gives the test a deterministic
    /// point — provably after the drain loop and after the receiver has been closed — at
    /// which to attempt a post-barrier send and inspect where it went.
    struct RecordingGatedShutdownWriter {
        written: Arc<std::sync::Mutex<Vec<u8>>>,
        began_shutdown: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<()>,
    }

    impl AsyncWrite for RecordingGatedShutdownWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.written.lock().unwrap().extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if let Some(began) = self.began_shutdown.take() {
                let _ = began.send(());
            }
            match Pin::new(&mut self.release).poll(cx) {
                Poll::Ready(_) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_line_but_keeps_the_writer_healthy() {
        let (started_tx, started_rx) = oneshot::channel();
        let writer = CategorizedJsonlWriter::spawn(
            BlockingWriter {
                started: Some(started_tx),
            },
            "cli-test.log".to_owned(),
            1,
        );
        // The task receives this line and parks in the (never-completing) write.
        writer.enqueue(b"one".to_vec());
        started_rx.await.unwrap();
        // The queue (capacity 1) is now empty; this fills it, the next overflows it.
        writer.enqueue(b"two".to_vec());
        writer.enqueue(b"three".to_vec());
        assert_eq!(writer.dropped_lines(), 1, "the overflowing line is dropped");
        assert_eq!(
            writer.state(),
            WriterState::Healthy,
            "a full queue is transient backpressure, never a failed writer"
        );
    }

    #[tokio::test]
    async fn a_write_error_terminates_the_writer_as_failed_and_is_observable() {
        let writer = CategorizedJsonlWriter::spawn(FailingWriter, "cli-test.log".to_owned(), 8);
        writer.enqueue(b"doomed".to_vec());
        // close() awaits the task's join, so on return the failure transition has settled.
        writer.close().await;
        assert_eq!(writer.state(), WriterState::Failed);
        // A closed/failed writer keeps dropping observably rather than faking success.
        writer.enqueue(b"after".to_vec());
        assert!(writer.dropped_lines() >= 1);
    }

    #[tokio::test]
    async fn close_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let writer =
            CategorizedJsonlWriter::open(dir.path().to_path_buf(), "cli", FixedClock(ANCHOR_MS))
                .await
                .unwrap();
        writer.close().await;
        writer.close().await; // second close is a no-op, never a panic or a hang
        assert_eq!(writer.state(), WriterState::ClosedShutdown);
    }

    /// The close barrier: every line whose `try_send` returned `Ok` before shutdown lands
    /// on disk, and any send after the barrier is an OBSERVABLE drop — never a silent
    /// `Ok` for a line that will never be written. A writer that ended its shutdown drain
    /// on a transient empty (the pre-fix `try_recv` loop) would accept the post-barrier
    /// line with `Ok` (drop counter stays 0) and lose it: this test fails against that.
    #[tokio::test]
    async fn close_is_an_accepted_line_barrier() {
        let written = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let (began_tx, began_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let writer = CategorizedJsonlWriter::spawn(
            RecordingGatedShutdownWriter {
                written: written.clone(),
                began_shutdown: Some(began_tx),
                release: release_rx,
            },
            "cli-test.log".to_owned(),
            64,
        );

        // Two lines accepted before the barrier.
        writer.enqueue(b"A".to_vec());
        writer.enqueue(b"B".to_vec());

        // Close on another task: it signals shutdown, drains A+B to the writer, then parks
        // inside `finish`'s gated `shutdown`, having already closed intake.
        let closing = {
            let writer = writer.clone();
            tokio::spawn(async move { writer.close().await })
        };

        // The writer has reached `shutdown`, i.e. it is strictly past the drain loop and
        // the receiver is closed. A send now must be an observable drop, not a fake `Ok`.
        began_rx.await.unwrap();
        writer.enqueue(b"C-after-barrier".to_vec());
        assert_eq!(
            writer.dropped_lines(),
            1,
            "a send after the close barrier is an observable drop, never a silent accept-and-lose"
        );

        // Let the gated shutdown complete and the close finish.
        let _ = release_tx.send(());
        closing.await.unwrap();

        assert_eq!(writer.state(), WriterState::ClosedShutdown);
        let on_disk = String::from_utf8(written.lock().unwrap().clone()).unwrap();
        assert_eq!(
            on_disk, "A\nB\n",
            "both lines accepted before the barrier landed; the post-barrier line did not"
        );
    }

    /// A close-time `shutdown` I/O fault is a durable failure, not a clean close: the
    /// writer must not report `ClosedShutdown` when it failed to finalize its file.
    #[tokio::test]
    async fn a_shutdown_error_at_close_is_a_durable_failure_not_a_clean_close() {
        let writer =
            CategorizedJsonlWriter::spawn(FlushOkShutdownFailsWriter, "cli-test.log".to_owned(), 8);
        writer.enqueue(b"one".to_vec());
        writer.close().await;
        assert_eq!(
            writer.state(),
            WriterState::Failed,
            "a failed close syscall must not be reported as a clean shutdown"
        );
    }

    /// If the writer task terminates abnormally (a panic), `close()` observes the
    /// `JoinHandle` error and fails closed — it never leaves a `Healthy`/`ClosedShutdown`
    /// illusion for a worker that died.
    #[tokio::test]
    async fn a_writer_task_panic_is_observed_by_close_as_failed() {
        let writer = CategorizedJsonlWriter::spawn(PanickingWriter, "cli-test.log".to_owned(), 8);
        writer.enqueue(b"boom".to_vec());
        writer.close().await;
        assert_eq!(
            writer.state(),
            WriterState::Failed,
            "an abnormal worker termination is a durable failure, never a clean close"
        );
    }

    #[tokio::test]
    async fn the_facade_projects_the_exact_wire_line_and_persists_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = CliAuditFile::open(
            dir.path().to_path_buf(),
            AppTag::Desktop,
            FixedClock(ANCHOR_MS),
        )
        .await
        .unwrap();
        let sink = owner.preflight_sink();

        // A record exercising every arm: present + absent optionals, empty flags, and a
        // fractional duration that must survive verbatim.
        sink.record(CliPreflightFailure {
            agent_id: AgentId::parse("agent-7").unwrap(),
            conversation_id: None,
            noun: Some("message".to_owned()),
            verb: None,
            flags_seen: Vec::new(),
            error_code: Some("E_STRICT_FLAG".to_owned()),
            error_subkind: None,
            error_message: Some("unknown flag --nope".to_owned()),
            duration_ms: Some(NonNegativeFiniteF64::new(12.5).unwrap()),
        });
        owner.close().await;

        // CLI logs use the UTC day; the JSON timestamp still has millisecond precision.
        let expected_name = "cli.2023-11-14.log";
        let path = dir.path().join(expected_name);
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines,
            vec![serde_json::json!({
                "ts": "2023-11-14T22:15:23.123Z",
                "level": "warn",
                "msg": "cli.preflight_failure",
                "app": "silverhand-desktop",
                "pid": std::process::id(),
                "category": "cli.preflight_failure",
                "agentId": "agent-7",
                "conversationId": null,
                "noun": "message",
                "verb": null,
                "flags_seen": [],
                "error_code": "E_STRICT_FLAG",
                "error_subkind": null,
                "error_message": "unknown flag --nope",
                "duration_ms": 12.5,
                "status": "failure"
            })]
        );

        assert_eq!(owner.state(), WriterState::ClosedShutdown);
    }

    #[tokio::test]
    async fn one_cli_owner_persists_preflight_and_invoke_in_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let owner = CliAuditFile::open(
            dir.path().to_path_buf(),
            AppTag::Server,
            FixedClock(ANCHOR_MS),
        )
        .await
        .unwrap();
        owner.preflight_sink().record(CliPreflightFailure {
            agent_id: AgentId::parse("agent-7").unwrap(),
            conversation_id: None,
            noun: None,
            verb: None,
            flags_seen: Vec::new(),
            error_code: None,
            error_subkind: None,
            error_message: None,
            duration_ms: None,
        });
        owner.invoke_sink().record(CliInvoke {
            request_id: "rust-7-1".to_owned(),
            agent_id: AgentId::parse("agent-7").unwrap(),
            route: Some(CliInvokeRoute {
                noun: "message",
                verb: "send",
                route_suffix: "send-message",
            }),
            method: CliInvokeMethod::Post,
            query_keys: Vec::new(),
            http_status: 422,
            duration_ms: NonNegativeFiniteF64::new(0.5).unwrap(),
            error_code: Some(CliInvokeErrorCode::InvalidInput),
            refusal_code:
                silverhand_backend_core::foundation::cli_invoke_audit::CliRefusalCode::parse(
                    "target_unresolved",
                ),
            error_issues: CliInvokeIssues::new(vec![CliInvokeIssue::body(
                CliInvokeIssueCode::InvalidStructure,
            )]),
            conversation_id: Some(
                silverhand_backend_core::foundation::identity::ConversationId::parse("conv-7")
                    .unwrap(),
            ),
            stale_resolution_reason: None,
            error_kind: None,
        });
        owner.close().await;

        let expected_name = "cli.2023-11-14.log";
        let contents = tokio::fs::read_to_string(dir.path().join(expected_name))
            .await
            .unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["category"], "cli.preflight_failure");
        assert_eq!(lines[1]["category"], "cli.invoke");
        assert_eq!(lines[1]["duration_ms"], serde_json::json!(0.5));
        assert_eq!(lines[1]["refusal_code"], "target_unresolved");
        assert_eq!(lines[1]["conversation_id"], "conv-7");
        assert_eq!(
            lines[1]["error_issues"],
            serde_json::json!([{ "path": "body", "code": "invalid_structure" }])
        );
        assert_eq!(owner.state(), WriterState::ClosedShutdown);
    }

    #[tokio::test]
    async fn the_agent_audit_facade_projects_each_variant_addressably_and_persists() {
        use silverhand_backend_core::conversation_messaging::messaging::AttachmentId;
        use silverhand_backend_core::foundation::identity::{AgentId, ConversationId, UserId};

        let dir = tempfile::tempdir().unwrap();
        let sink = AgentAuditFileSink::open(
            dir.path().to_path_buf(),
            AppTag::Server,
            FixedClock(ANCHOR_MS),
        )
        .await
        .unwrap();

        sink.record(AgentAuditEvent::ChatsListed {
            viewer: UserId::parse("viewer-1").unwrap(),
            agent_id: AgentId::parse("agent-7").unwrap(),
            chat_count: 3,
        });
        sink.record(AgentAuditEvent::TranscriptRead {
            viewer: UserId::parse("viewer-1").unwrap(),
            agent_id: AgentId::parse("agent-7").unwrap(),
            conversation_id: ConversationId::parse("conv-9").unwrap(),
            message_count: 42,
        });
        sink.record(AgentAuditEvent::AttachmentRead {
            viewer: UserId::parse("viewer-1").unwrap(),
            agent_id: AgentId::parse("agent-7").unwrap(),
            conversation_id: ConversationId::parse("conv-9").unwrap(),
            attachment_id: AttachmentId::parse("att-2").unwrap(),
        });
        sink.close().await;

        let expected_name = "agent-audit.2023-11-14.log";
        let path = dir.path().join(expected_name);
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3, "one line per recorded event");

        // Shared envelope on every line: info level, the audit msg, the server app tag.
        for line in &lines {
            let o = line.as_object().unwrap();
            assert_eq!(o["ts"], "2023-11-14T22:15:23.123Z");
            assert_eq!(o["level"], "info");
            assert_eq!(o["msg"], "agent.audit_event");
            assert_eq!(o["app"], "silverhand-server");
            assert_eq!(o["pid"], std::process::id());
            assert_eq!(o["viewer"], "viewer-1");
            assert_eq!(o["agentId"], "agent-7");
            assert_eq!(o["status"], "success");
        }

        // ChatsListed: category + chatCount are addressable; no conversation/attachment.
        let listed = lines[0].as_object().unwrap();
        assert_eq!(listed["category"], "agent.chats_listed");
        assert_eq!(listed["chatCount"], serde_json::json!(3));
        assert!(listed.get("conversationId").is_none());

        // TranscriptRead: conversationId + messageCount addressable.
        let read = lines[1].as_object().unwrap();
        assert_eq!(read["category"], "agent.transcript_read");
        assert_eq!(read["conversationId"], "conv-9");
        assert_eq!(read["messageCount"], serde_json::json!(42));

        // AttachmentRead: conversationId + attachmentId addressable.
        let att = lines[2].as_object().unwrap();
        assert_eq!(att["category"], "agent.attachment_read");
        assert_eq!(att["conversationId"], "conv-9");
        assert_eq!(att["attachmentId"], "att-2");

        assert_eq!(sink.state(), WriterState::ClosedShutdown);
    }
}
