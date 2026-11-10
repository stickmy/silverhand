# Rust Server Environment Configuration

The executable is `silverhand-backend` (`rust/crates/backend-app`). Both Desktop
and standalone deployment launch this binary with explicit paths and a deployment
profile. The former Node `SILVERHAND_LISTEN`, `SILVERHAND_DATA_DIR`,
`SILVERHAND_EVENT_WS_ALLOWED_ORIGINS`, and `SILVERHAND_LOG_LEVEL` variables are
not Rust configuration aliases.

## Variables

| Variable                          | Contract                                                                                                                                                                                                        |
| --------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `SILVERHAND_DEPLOYMENT_PROFILE`   | Required: `embedded` or `remote-only`. The latter advertises deployment mode `standalone`.                                                                                                                      |
| `SILVERHAND_DATABASE_PATH`        | Required, nonempty path to the SQLite database file.                                                                                                                                                            |
| `SILVERHAND_BLOBS_ROOT`           | Required, nonempty blob directory path.                                                                                                                                                                         |
| `SILVERHAND_LOG_DIR`              | Required, writable diagnostic and categorized audit log directory.                                                                                                                                              |
| `SILVERHAND_LISTEN_ADDR`          | Required numeric IP socket address, e.g. `0.0.0.0:8080` or `[::1]:8080`.                                                                                                                                        |
| `SILVERHAND_ALLOWED_ORIGINS`      | Required, nonempty comma-separated event WebSocket origin allowlist. Use the actual client-facing HTTPS origin.                                                                                                 |
| `SILVERHAND_METRICS_LISTEN`       | Optional; unset or empty disables metrics. Numeric loopback IP socket only, e.g. `127.0.0.1:9464` or `[::1]:9464`. Wildcards, LAN addresses and DNS names are rejected. Port 0 is allowed for isolated callers. |
| `SILVERHAND_CONSOLE_TOKEN`        | Optional; enables the token-gated ECS Console on the product listener. Requires `remote-only` and 32–256 visible ASCII characters. Independent of metrics; expose through the existing HTTPS ingress.           |
| `SILVERHAND_DISABLE_SIGNUP`       | Remote-only: unset/empty or `0` enables signup; `1` disables it; other values fail. Embedded preserves the launcher contract: only exact `1` disables signup.                                                   |
| `SILVERHAND_SMTP_URL`             | Optional; remote-only. `smtps://user:pass@host:465` or `smtp://user:pass@host:587?tls=required`; plaintext or opportunistic TLS fails startup. Set with `SILVERHAND_MAIL_FROM` to enable password recovery.     |
| `SILVERHAND_MAIL_FROM`            | Optional; the recovery mail sender, e.g. `Silverhand <no-reply@example.com>`. Setting only one of the two mail variables fails startup.                                                                         |
| `SILVERHAND_MIGRATE`              | `0` or `1`; defaults to `0` for remote-only, `1` for embedded. Fresh databases initialize without an upgrade opt-in.                                                                                            |
| `SILVERHAND_DAEMON_NPM_SPEC`      | Optional explicit daemon package specification.                                                                                                                                                                 |
| `SILVERHAND_BOOTSTRAP_TOKEN_PATH` | Optional launcher-owned token file; armed only for embedded on actual loopback bind.                                                                                                                            |
| `SILVERHAND_STARTUP_GENERATION`   | Optional launcher correlation token. Emits one stdout `bound` frame containing the actual API address; not a readiness claim.                                                                                   |
| `RUST_LOG`                        | Tracing filter; defaults to the ERROR floor, with activity-retention INFO summaries and credential-free Console authorization rejection WARN events.                                                            |

Use absolute paths in service configuration. `SILVERHAND_ALLOWED_ORIGINS` is a
security allowlist, not the backend bind address or a topology advertisement.
Embedded composition also permits its actual bound loopback origin.

## Listener and readiness contract

`GET /api/health` is liveness. `/readyz` checks migrations, the bound API listener,
a real `BEGIN IMMEDIATE`/rollback write probe and, for standalone, outbound fanout
installation. A failed check returns 503.

Metrics has an independent listener. Only its `GET /metrics` exports Prometheus
text; `/metrics` on the API listener refuses every method with 403 even when
metrics is disabled. Never expose port 9464 through public ingress.

A configured metrics bind failure aborts startup before database preparation.
The executable owns the exporter and its upkeep; unexpected exporter termination
requests application shutdown and produces a failing process exit. SIGTERM and
SIGINT drain the existing application shutdown path and close both listeners.
See [Server Observability](server-observability.md).
See [ECS Console Dashboard](ecs-console.md) for the separate product-count read model.

## Password recovery

With both mail variables set, a remote-only server advertises
`features.password_recovery_enabled` in `/api/server/info` and serves
`POST /auth/password-recovery/request` and `/reset`; otherwise both answer 503
`password_recovery_disabled`. The configuration is validated at startup only; the
server does not probe SMTP.

- Codes: 6 digits, 15 minutes, one use, at most 5 attempts each. Per account and
  hour: at most 5 mails, 10 attempts, and 60 seconds between mails. Each route also
  admits at most 60 requests per minute per process (429 beyond).
- Codes are stored as an HMAC under a key held only in process memory. A restart
  voids every outstanding code, including ones already delivered; the user requests a
  new one, subject to the same limits.
- Mail is sent from a bounded in-memory queue (16 pending, 4 concurrent, 10 s
  timeout). A full queue answers 503 `password_recovery_busy`. Queued mail is lost on
  restart, and a send failure is logged only as its kind and SMTP status; the user
  requests again after the interval.
- A successful reset replaces the password, consumes the code and revokes every
  session of the account in one transaction; it does not sign in.

## Data and upgrades

SQLite and its WAL/SHM files stay on the same filesystem. There is one server
owner of the data directory. Do not open a running user's database with an
external SQLite client for inspection or backup.

The shared migration plan (`packages/shared/src/backend-migration-plan.json`)
and `backend-sqlite/src/foundation/database_startup.rs` define Rust's upgrade
contract. Startup refuses unknown/newer, too-old or inconsistent ledgers. Existing
standalone databases require `SILVERHAND_MIGRATE=1` to advance. Preparation holds
SQLite exclusive locking across backup and step commits; it does not use the old
Node migration-lock directory. Before applying any upgrade, the SQLite owner uses
`VACUUM INTO` to create a consistent database-only backup in a timestamped sibling
directory. Backup failure prevents migration. It does not copy the database parent:
attachments, logs, old backups and other files are excluded regardless of layout.

Release backups default to the database only. Operators explicitly back up
attachments when requested, separately from this startup step; logs are not part
of a release data backup. File migrations or changes to file references require
an explicit attachment backup decision before deployment. A database backup alone
does not restore deleted attachments. Retention is operator-owned; an older binary
may require the matching pre-upgrade database backup, and restoring it discards
subsequent writes.

Do not run the former Node admin recovery commands against a Rust-owned live
database. Rust's executable currently has no matching admin subcommand contract.

## Standalone example

Create and secure the directories before launch, then supply the deployment's
actual paths and origin:

```bash
SILVERHAND_DEPLOYMENT_PROFILE=remote-only \
SILVERHAND_DATABASE_PATH=/var/lib/silverhand/silverhand.db \
SILVERHAND_BLOBS_ROOT=/var/lib/silverhand/blobs \
SILVERHAND_LOG_DIR=/var/log/silverhand \
SILVERHAND_LISTEN_ADDR=127.0.0.1:8080 \
SILVERHAND_ALLOWED_ORIGINS=https://silverhand.example.com \
SILVERHAND_METRICS_LISTEN=127.0.0.1:9464 \
SILVERHAND_DISABLE_SIGNUP=1 \
/path/to/silverhand-backend
```

Build for the target CPU and Linux ABI; a macOS binary cannot run on ECS. Build,
staging, backup and the authorized service cutover are separate operations. The
[deployment checklist](rust-server-ecs-deployment-checklist.md) points to the ECS
operating manual. This configuration document is not a production deployment record.
