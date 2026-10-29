# Server Observability

Silverhand standalone exposes Prometheus metrics only when
`SILVERHAND_METRICS_LISTEN` is configured. The metrics listener is a separate
HTTP server from the public API and refuses non-loopback bind addresses.

```bash
SILVERHAND_METRICS_LISTEN=127.0.0.1:9464
```

The endpoint is `GET /metrics`. Do not add it to the public Nginx ingress.

## Current Rust coverage

- HTTP duration/count/status-class histograms, through response-body completion
  or cancellation, using router templates rather than request URLs;
- process CPU, resident/virtual memory, file descriptors, thread count and start
  time (platform-supported process collector fields);
- SQLite readiness write-probe duration/outcome;
- host WebSocket upgrade/ready/close, open connection count, protocol mismatch
  and compatible ready protocol distribution; socket lifetime owns gauge cleanup;
- server-to-daemon command duration/outcome at the actual command transport;
- explicitly named SQLite host lifecycle/list/detail/status/resource and
  credential statements, plus readiness BEGIN/rollback. This is not whole-database
  SQL coverage; uninstrumented query owners do not emit an `unknown` query name.

The `silverhand_*` application metric names, label keys and histogram bucket
boundaries retain the previous Prometheus contract. `deployment_mode` is
`embedded` or `standalone`. Route parameters use `:name` label spelling; an
unmatched route is `unmatched`, and nonstandard HTTP methods collapse to `OTHER`.
SQL outcomes are `ok`, `busy`, `locked`, `error`, using SQLite's primary numeric
error code (including extended BUSY/LOCKED codes). Query names are source-owned
static identifiers; internal queries that have changed shape receive descriptive
names instead of pretending to be the old statement.

The library crates emit through the `metrics` facade. Only the executable owns
and installs a Prometheus recorder, and without configured metrics the facade is
no-op. An upkeep loop drains histogram observations every five seconds even
when Prometheus is unavailable. Library events do not depend on `RUST_LOG` or on
parsing diagnostic log messages.

Node heap, GC and event-loop metrics are retired from the Rust dashboard and
rules. The process memory panel retains `process_resident_memory_bytes` and adds
virtual memory. `silverhand_tokio_scheduling_delay_seconds` measures the lateness
of a five-second Tokio timer; it is not a Node event-loop p99 or utilization
measurement. The corresponding Rust scheduling-delay alert uses its own name.

Metrics never use raw URL paths, host/user/agent/conversation ids, SQL text,
SQL parameters, daemon payloads or arbitrary close-reason strings as labels.
Protocol offers are bounded by the existing 0..100 admission envelope; close codes
and reasons use a fixed classification. A compatible-ready gauge means the wire
compatibility gate passed, not that all downstream hydration has completed.

## Diagnostic logging

The Rust production JSON sink preserves formatted tracing messages, `%Display`
errors and `?Debug` diagnostic context. Formatting alone must never cause an
event's message or error to be discarded: dependency events such as axum's
`accept error: {e}` must retain the actual OS error. This applies to both file
and stderr output and does not require a special wrapper at each call site.

Explicit content/environment and credential fields remain filtered; values whose
formatted representation is valid JSON receive the same recursive filtering.
Arbitrary prose and Rust Debug
structs are retained as text, not treated as reliably parseable secret containers.
Call sites must therefore keep credentials in secret types or explicit filtered
fields, never interpolate tokens into error messages or dump authentication
objects. The formatter does not promise to discover secrets embedded in text.
Normal causes, error codes and diagnostic coordinates must not be hidden behind
redaction counters. Existing log levels, retention and CLI request correlation
are unchanged.

## Request diagnostics and phase spans

The production JSON subscriber always enables the source-owned
`silverhand::diagnostics` target, including under the default ERROR floor.
Each public/internal API request receives a process-local generated HTTP request
ID and a standard `tracing` span. Request headers, bearer tokens, raw URLs/query
strings, SQL text/bindings and message contents are not captured. Routes use the
same normalized templates as metrics. Errors inside the request carry
`http_request_id`; CLI errors retain their existing `request_id` as well.

One `HTTP request diagnostic` event is emitted for requests lasting at least
1 second, 5xx responses, or incomplete/error termination. Fast successful requests
do not emit per-request logs. The event contains:

- `request`: ID, method, route, actual status when available, completion state,
  `handler_ms` through response headers, and `body_ms` through body termination;
- `elapsed_ms`: request lifetime through response-body termination (or handler
  cancellation when no response was produced), in milliseconds;
- `phases`: source-named child spans, each with `count`, `total_ms`, `max_ms`,
  and bounded `outcomes` counts (`ok`, `error`, `cancelled`).

Phase timing is elapsed wall time, not CPU time. Nested phases overlap; do not
sum them into a critical path or subtract all of them from total duration.
The subscriber aggregates by static operation names, never by entity IDs, and
holds no global request map or persistent state. Fast child operations still
contribute to a slow parent's summary. Cancellation records partial operation duration; it never reports success.
`ok` means the observed Rust Result was Ok, not that a containing transaction
committed or a nested domain result accepted the request. An unpolled, already-empty
body is complete, not a cancellation. Response body errors retain the actual
Display error in a correlated error event. An unfinished handler has no invented
HTTP status. Existing response bytes, trailers, errors and HTTP metric semantics
are unchanged.

Current phase coverage:

- human-session authentication: separate pool acquisition and session/user read;
  renewal BEGIN, recheck, conditional touch and commit only when renewal is due;
- selected read adapters: Activity, favorites, DirectPairs, attention badges/modes,
  channel directories, message windows and unread projections;
- named SQL statements and selected read queries;
- connection acquisition separately from transaction BEGIN on these transaction
  paths (BEGIN/query duration can include SQLite lock wait);
- the actual daemon command round trip, with the closed command kind in its key.

Read-adapter spans include projection/decoding and their nested queries: they are
not synonymous with SQL execution time. This is targeted coverage, not automatic
whole-database tracing. Some single-statement pool executions and downstream
helpers remain inclusive of acquisition. Missing phases must not be interpreted
as zero database work. Child spans use `skip_all` and `Instrument`/instrumented
async functions, not an entered thread-local guard held across `.await`.
Spawned background jobs require explicit context propagation when they belong to
the request; this change does not claim end-to-end tracing inside remote daemons.
Unparented diagnostic operations lasting at least 1 second emit a separate
`slow background operation` event.

This first stage requires no collector or additional ECS service. Existing local
logs and Prometheus remain authoritative. The standard spans can later feed
`tracing-opentelemetry` and Tempo; no OTLP exporter, Collector, Tempo deployment or
sampling policy is enabled by this change. Trace retention and deployment resource
budgets must be configured when that infrastructure is introduced.

## Operation boundaries and tracing context

`silverhand-backend-observability` supplies the shared adapter-level timer; core
business types do not depend on it. `Observe::observe` delegates context
propagation to standard `Instrument`. Functions with `#[instrument(skip_all)]`
use an `Operation` guard and finish it with their Result. Span fields are bounded;
no arguments, result values, SQL, or error contents enter the timer.

Timing starts when the operation is polled/entered, ends at result or cancellation,
and is recorded before a terminal `operation_finished` field. The application
collector consumes that record exactly once. It does not wait for `on_close`:
SQLx workers or other downstream code may still own span clones. Do not hold an
entered thread-local guard across `.await`; synchronous `in_scope` is appropriate
for assembly. Unpolled futures produce no observation.

HTTP completion has its own authority: handler cancellation before headers, or
response-body completion/error/drop afterwards. The final completion record
emits the summary once, regardless of remaining span handles. Operations that
finish after the HTTP boundary do not reopen its summary or extend request time.
Statement histograms retain their existing Result-based samples: cancellation
has a diagnostic outcome but does not fabricate a successful SQL metric sample.

Production installs one global subscriber. Observability composition tests run
in independent processes with the same global subscriber and assert diagnostics
while SQLite pools remain open. This covers worker threads without assuming a
scoped test subscriber propagates into them. Tests also retain span clones across
completed requests, and cancel a real writer-blocked BEGIN before releasing the
lock, then verify rollback/reuse with a subsequent request.

The locked tracing-subscriber 0.3.23 already contains the local Registry::exit
reference handling from [upstream PR #3331](https://github.com/tokio-rs/tracing/pull/3331).
A scoped-subscriber test failure is not evidence of a production span leak; its
specific cause remains unestablished. Standard asynchronous instrumentation follows
[tracing's documentation](https://docs.rs/tracing/latest/tracing/struct.Span.html#in-asynchronous-code).

## Named SQLite statement metrics

Adapters wrap the actual SQLx execution future with
`foundation::statement_metrics::statement(stable_name, operation, future)`.
`get`, `all` and `run` preserve the old operation vocabulary. Duration includes
SQLite lock wait and fetching, but not business-row decoding. Pool acquisition is
included only when the passed future executes against a pool; where acquisition
is measured separately, statement timing starts with the acquired connection.
The wrapper returns the original result unchanged. Readiness instruments its
own BEGIN/rollback as statements as well as the complete probe, so the contention
alert can detect a writer lock even when user traffic is absent.

## Temporary single-ECS deployment

The checked-in starting configuration is under `ops/observability/`:

- `prometheus.yml` scrapes Silverhand and node_exporter on loopback;
- `silverhand-alerts.yml` contains the initial service alerts;
- `grafana/provisioning/` provisions the local Prometheus datasource and the
  Silverhand overview dashboard;
- `logrotate-silverhand` caps logs in the configured `SILVERHAND_LOG_DIR` without
  requiring the running server to reopen file descriptors.

For a temporary same-host deployment:

1. Run Prometheus, Grafana, and node_exporter as native/systemd services so
   `127.0.0.1` consistently means the ECS host. A container needs explicit
   host networking; do not change the app metrics bind to `0.0.0.0` merely to
   accommodate container networking.
2. Start Prometheus with both a time and size bound, for example
   `--storage.tsdb.retention.time=7d` and
   `--storage.tsdb.retention.size=5GB`.
3. Bind Prometheus and Grafana themselves to loopback. Reach Grafana through an
   SSH tunnel while this is an owner-only deployment; do not add an unauthenticated
   public proxy route.
4. Put Prometheus/Grafana data outside `SILVERHAND_DATA_DIR` and set systemd
   `MemoryMax`, `CPUQuota`, and log rotation. Monitoring must not compete with
   SQLite's data/WAL path for an unbounded disk budget.
5. Keep one external uptime probe against public `/readyz`. Same-ECS Prometheus
   cannot report an ECS-wide outage after the machine disappears.

The checked-in Prometheus rules are local evaluations only. Notifications still
require an Alertmanager receiver; do not call alerting complete until a real
owner channel and a test notification are configured.

This is incident-local visibility, not durable monitoring. When a remote sink
is available, keep the app's Prometheus/OTLP contracts and move retention off
the application host rather than redesigning instrumentation.

## Rust log retention

Desktop and daemon diagnostic files follow the same UTC daily convention:
`<prefix>.YYYY-MM-DD.log`, appending across same-day restarts and retaining the
latest 14 files per prefix. Each process manages its own configured directory;
the standalone daemon does not depend on desktop for cleanup. Cleanup runs at
logger startup and on the first write after a UTC day boundary, with no timer.
Ordinary writes only compare their timestamp with the cached rotation deadline.
There is no per-file size cap or total disk quota. Desktop main-process logs use
the `desktop` prefix (formerly `backend`). At the same cleanup points, desktop
expires its old `backend` and `agent` files (boot-timestamp and interim daily), and daemon
expires its boot-timestamp `daemon` and retired `agent-activity` files, after
14 days without writes. Last modification time is used because old files may
span days after their boot timestamp. This includes the interim daily `backend`
names. Rust diagnostic logging keeps its existing implementation.

The server-owned audit writers use `cli.YYYY-MM-DD.log` and
`agent-audit.YYYY-MM-DD.log`, appending
within the UTC day and retaining 14 daily files. It prunes at startup and on the
first line after midnight; old `cli-<boot-timestamp>.log` and
`agent-audit-<boot-timestamp>.log` files expire after 14
days without writes. These sinks are the only owners of `cli.*` and
`agent-audit.*` files. A day file that cannot be opened does not fail the sink:
it keeps appending to the current file and retries at the next boundary. Record fields, bounded-queue behavior, shutdown draining,
and observable I/O-failure handling are unchanged. This applies to embedded and
standalone servers; it does not depend on a desktop process doing cleanup.

Detached `daemon connect` disables the stderr mirror: normal runtime diagnostics
go only to the daemon's daily files. Its `connect-launcher.log` captures raw
stdout/stderr (startup failures, uncaught errors) and is overwritten on each
new background launch, after the existing-process check. It is not daily-rotated
or size-capped. Foreground connect still mirrors logs to the terminal by default.

Diagnostics are `rust-backend.YYYY-MM-DD.log` under `SILVERHAND_LOG_DIR`; the
process appender rolls daily and retains at most 14 matching files. Categorized
CLI/agent audit logs share the directory and have separate retention needs.
The checked-in logrotate template is only a 100 MiB size fallback using
copytruncate. Replace its example path with the real log root and run the check
frequently enough for the intended disk budget; it is not a hard disk quota.
Do not carry the old seven-day cleanup into the Rust diagnostic directory
without explicitly reconciling it with the appender's retention. Rotated
compressed copies are retained by logrotate, not counted as the appender's daily
files. See [Rust server logging](rust-server-logging.md).
