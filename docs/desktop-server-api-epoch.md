# Desktop↔Server API Epoch — contract & upgrade runbook

> TS backend 退役说明：文中的 `packages/backend` / `packages/server` 源码与测试坐标属于迁移期历史，已从当前仓库删除；现行控制面位于 `rust/crates`。迁移权威仍在 `packages/shared/src/db`，独立回归数据在 `rust/test-fixtures`，现行构建与验证见 [testing.md](testing.md)。

> #task:1761. This document is the operating manual for the
> `DESKTOP_SERVER_API_EPOCH` axis defined in
> `packages/shared/src/desktop-server-api-epoch.ts` (the admission-semantics
> root), with shared values in `packages/shared/src/backend-admission.json`.
> Both release-artifact entry points (desktop packaging via
> `scripts/package.sh`, standalone server bundle via the
> `packages/server` build) run `scripts/epoch-runbook-reminder.mjs`, which
> prints this file's path and FAILS THE BUILD if the file is missing.

## 1. What this axis is

One protocol axis per process boundary; an artifact only carries the axes it
participates in:

| Axis                        | Boundary                         | Constant                                                                       |
| --------------------------- | -------------------------------- | ------------------------------------------------------------------------------ |
| Host control wire           | HostClient ↔ local daemon        | `HOST_PROTOCOL_VERSION` (`packages/host/src/protocol.ts`)                      |
| Outbound wire               | remote daemon ↔ server           | `OUTBOUND_PROTOCOL_VERSION` (`packages/host/src/outbound-protocol.ts`)         |
| **Desktop application API** | desktop ↔ server REST + Event WS | `DESKTOP_SERVER_API_EPOCH` (`packages/shared/src/desktop-server-api-epoch.ts`) |
| DB schema version           | server binary ↔ database         | migrations — not a network protocol                                            |

Never reuse one axis for another boundary, and never infer compatibility from
app semver, git commit, or build identity.

**Single numeric authority.** Host and outbound daemon versions remain in
`packages/shared/src/protocol-versions.ts`. The desktop epoch triple and the
transport admission names shared by the TypeScript desktop/client and Rust
server live in `packages/shared/src/backend-admission.json`.
`backend-admission.generated.ts` is a checked projection, not another
write-point. Bump the desktop triple by editing the JSON artifact and regenerating
the projection; Rust consumes the same artifact during its build. The DB schema
version is a separate axis with its own authority.

The contract is **safe version skew (default plan A)**: a compatible pair runs
normally; an incompatible pair is rejected _before any business DTO is read_ —
structured HTTP 426 (JSON `UpgradeRequiredBody`) on REST **and** on the
event-WS upgrade response, plus a typed full-screen gate in the desktop
(`app_update_required` / `server_update_required`). We do NOT promise that old
desktops keep working against new servers.

## 2. Where the epoch flows

- **Advertise**: `/api/server/info` → `desktop_api { current_epoch,
minimum_client_epoch }`.
- **Carry (admission)**: the DEDICATED `x-silverhand-desktop-api-epoch`
  header — a compiled constant rendered to a decimal string, injected by the
  main-process `backendFetch` seam on every REST request and by the WS
  origin-override on the upgrade. Assembling it involves no IO and no schema,
  so an epoch-aware desktop can never fail to send it.
- **Carry (telemetry)**: the fail-open `x-silverhand-client-info` payload
  also carries `desktop_api_epoch` + `minimum_server_api_epoch` for fleet
  observation. It plays NO role in admission; a telemetry assembly failure
  degrades observability only.
- **Enforce (authoritative)**: server-side —
  `installDesktopApiEpochMiddleware` (REST, mounted on `/api/*`) and
  `checkDesktopApiEpochOnUpgrade` (WS), both reading the dedicated header and
  the shared constants.
- **Explain**: desktop `desktopAuthState()` compares ranges right after the
  server-info fetch and returns the typed gate arm.

Historical TS REST exemption list (the retired shared helper has been removed):
`/api/health`, `/api/server/info`, `/api/hosts/connect-token/resolve`
(daemon surface), `/api/hosts/connect` (daemon WS upgrade; defensive pin —
upgrades bypass middleware anyway). `/auth/*`, `/readyz`, and `/internal/*`
(agent CLI) are outside the `/api/*` mount and structurally unaffected.

## 3. When to bump — decision rule

**Precondition — "one bump per RELEASED generation, not per breaking change."**
The epoch names a _shipped_ pair, so the question is never "does this diff break
something?" in the abstract but **"can a peer at the current epoch already exist
in the field?"** If the current epoch has NOT been published yet, a breaking
change joins that still-unreleased generation and needs no second bump; both
artifacts are built from the tree that already carries it, so no admissible
mismatched pair can be constructed. If it HAS been published, the same change
needs its own triple.

Check it, don't assume it — the current epoch is a working-tree value and says
nothing about what shipped. A release IS a tag: read the LAST PUBLISHED value
from the newest matching release tag with the recipe in §6 (it handles tags cut
before the shared authority existed). An untagged local or dogfood build is
not a release and puts no peer in the field — it carries both sides from one
tree and has zero skew by construction (§4.2). If the current epoch already
appears in a published tag, fall back to the rules below.

- **Do NOT bump** for additive, old-side-safe changes: new optional DTO
  field, new endpoint, new optional event field old clients ignore safely.
- **MUST bump — as an atomic triple** for breaking changes (removing/renaming
  a required field; **adding a REQUIRED response field, or a new endpoint the new
  client hard-depends on**, once the current epoch is released — a new desktop
  would then read `undefined` / 404 deep inside a feature instead of being
  refused at admission, which is exactly the failure mode plan A exists to
  prevent; changing a discriminant, nullability, pagination/cursor
  semantics, permission semantics old clients would misinterpret; changing
  event ordering guarantees). In the SAME commit as the breaking change, edit
  the desktop triple in `packages/shared/src/backend-admission.json` — and ONLY
  there — to the same new value, then run
  `pnpm --filter @silverhand/shared generate:backend-admission`:

  ```
  // packages/shared/src/backend-admission.json
  "desktopApi": N+1
  "desktopMinClient": N+1
  "desktopMinServer": N+1
  ```

  Bumping only `desktopApi` while a minimum stays behind silently re-admits the
  exact incompatible pair plan A exists to reject. The lockstep test in
  `packages/shared/tests/desktop-server-api-epoch.test.ts` enforces this
  structurally: a minimum of 0 is legal ONLY while the current epoch is
  still 1 (the named first-introduction bootstrap, per field); any other
  divergence from `minimum === current` fails the build. A deliberate
  plan-B bridge must change that test together with its adapter and §11
  exit conditions.

Review checklist line: _"Does this diff change any REST DTO / event shape /
behavioral semantics in a way an old desktop would misread? If yes, are ALL
THREE epoch constants bumped to the same value in this same commit?"_

## Desktop 0.4.0 source boundary

Desktop 0.4.0 uses API current/minimum-client/minimum-server **4/4/4**.
This closes the previously deployed epoch-3 compatibility gap for agent activity
subscriptions, scoped permission denials, and human-send correlation/retry
semantics. Epoch-3 peers require an update; no compatibility bridge is provided.
The version source is `apps/desktop/package.json`; packaging derives its version
from that file. Regenerating the pnpm lockfile does not change it for this
workspace package's own version.

Daemon wire versions remain independently **host 12 / outbound 11**. The daemon
package metadata remains **0.2.0** in this source update: a new public daemon
package version must be chosen before publishing a changed daemon package;
Desktop 0.4.0 does not assign that independent package version. This update
performs no packaging, publication, tags, pushes, or deployment.

## 4. Release procedure (plan A — default)

1. Land the breaking change + atomic triple bump (one commit).
2. Ship server and desktop from that tree. The primary shape is a REMOTE
   server with separately-installed desktops, so skew is real there; the
   embedded single-tree build (dogfood) carries both sides and has zero skew
   by construction.
3. Remote deployment order is free, but the skew window is an ACCEPTED
   mandatory upgrade, not a zero-downtime rollout (see §5). Server-first: old
   desktops are rejected server-side immediately (426), and show the
   full-screen upgrade gate **on their next startup or auth-state re-probe**.
   Desktop-first: the new desktop shows `server_update_required` at startup
   against the old server. Either way the user continues only after both
   sides are on the new generation.

**Runtime promotion (#task:1763):** server-side fail closed is immediate and
authoritative; a RUNNING desktop additionally promotes into the full-screen
gate without waiting for a restart. The path keeps one authority:

1. Two inputs reach the main-process runtime-upgrade monitor
   (`apps/desktop/src/main/runtime-upgrade-monitor.ts`): a REST proxy 426
   whose body parses as `UpgradeRequiredBody` (suspect — never trusted
   directly), and the `desktop_api` advertisement main fetches inside the
   backend:info handler (authoritative — classified directly). That handler
   is hit by the ws-client's reconnect discovery — `openSocket()` re-runs
   `getBackendInfo()` before EVERY reconnect attempt (first backoff tier
   500ms) — so a rejected WS upgrade promotes within the next reconnect
   cycle; the renderer's 30s backend-info poll is the idle fallback, not
   the primary path.
   There is deliberately NO event-WS arm: spike-verified, Electron's
   webRequest layer cannot observe a failed WS upgrade's status/headers/
   body (only net::ERR_FAILED), so a dead socket carries zero epoch
   semantics and never promotes by itself — promotion comes from the
   discovery that precedes the reconnect, not from the failure.
2. A REST suspect triggers ONE single-flight server-info probe,
   re-classified with the same shared classifier the startup precheck
   uses. Only a confirmed `client_too_old` (from probe or advertisement)
   pushes `epoch:update-required` to the renderer (latched — at most once
   per incompatibility period; the latch is a notification dedupe, not a
   second compatibility state).
3. The renderer invalidates its auth-state query; `getAuthState()` re-runs
   the startup precheck, which returns the `app_update_required` arm and
   unmounts the registered shell through the normal gate order.

Recovery: when a later auth-state probe observes a compatible pair again
(app updated, or server rolled back), the monitor re-arms automatically.
Unrelated failures (network errors, 401/403, foreign 426 bodies, malformed
payloads, unreachable probe) never promote.

**Plan B (one-off N-1 bridge)** is NOT built by default. Only when a release
explicitly requires zero interruption: keep `MINIMUM_CLIENT_API_EPOCH` at N
while the server serves both generations behind a request-epoch switch, ship
desktops, then raise the minimum and delete the adapter. Any such bridge must
carry the §11 exit-condition triple (named contract / observable removal
trigger / owner) inline and adjust the lockstep test in the same commit.

## 5. Observation bootstrap — CLOSED at epoch 2

The first introduction shipped the observation bridge: `MINIMUM_CLIENT_API_EPOCH
= 0` and `MINIMUM_SERVER_API_EPOCH = 0`, so a pre-epoch desktop (no dedicated
header, classified epoch 0) and a pre-epoch server (no `desktop_api` in
server-info) were both still admitted, while every event-WS session logged
`admission_epoch` in `desktop_client_connected` for the cutoff decision.

**The bridge is now closed.** relocation-v1 §6's atomic triple bump moved all
three constants to epoch 2, which puts the pre-epoch generation BELOW the
minimum on both sides:

- server side: `/api/*` and the event-WS upgrade fail closed with a structured
  426 `upgrade_required` / `client_too_old` for an absent or malformed header;
- client side: the desktop refuses a server that advertises below its own
  minimum, and promotes into the full-screen upgrade gate.

**Cutoff basis: an explicit breaking cutoff, ruled by the product owner —
NOT an observation that the old generation is gone.** The primary deployment
shape is a REMOTE server with separately-installed desktops (the embedded
single-tree build is dogfood only), so raising the minimum is a real,
user-visible break, and it was accepted as one on 2026-08-05:

- during any client/server version skew a remote user hits the full-screen
  upgrade gate and **must upgrade both sides** to continue — mandatory,
  interrupting, and expected;
- neither direction can deep-decode the other's payloads: both are refused at
  admission (426 server-side / startup gate client-side), so the failure mode
  is "blocked until upgraded", never a partial or corrupt read;
- no plan-B dual-DTO bridge is built for that window; the decision was that a
  bounded interruption is cheaper than carrying two generations of the shape.

Cutoff date: 2026-08 (relocation-v1 §6/§7 landing). The §3 minimum raise is
therefore a product decision recorded here, not a wait on a distribution
metric — and re-raising the minimum later carries the same cost, so it needs
the same explicit ruling.

`admission_epoch` logging stays — it is now the evidence channel for the NEXT
raise, not for this one. The telemetry `client_info_status` buckets (`missing` /
`invalid`) remain monitored for observability health and were never cutoff
conditions: they describe the optional telemetry channel only, and gating
admission on them would re-couple optional telemetry to the admission decision.

Nothing here is a permanent fail-open, and there is no remaining bridge to
exit; this section is the §11 record of its closure.

## 6. Release-time version-step check (all three wire axes) — MANUAL

The single authorities remove _duplicate_ version numbers; they do NOT prove
the numbers move correctly across releases. That is a
release-time discipline, done by a human against the release tags — deliberately
NOT code and NOT a test. The rule for **every** wire axis
(`HOST_PROTOCOL_VERSION`, `OUTBOUND_PROTOCOL_VERSION`, `DESKTOP_SERVER_API_EPOCH`
and its two minima):

> Each axis, versus the LAST PUBLISHED release of the artifact that carries it,
> is either **unchanged** or **exactly +1**. Never +2 or more.

**The tag is the only release proof — judge against it, never against
`origin/main`.** `origin/main` can already carry an unreleased bump, so it is not
"what shipped". Take the newest tag for the artifact that carries the axis, then
read the last shipped value FROM THAT TAG:

| Axis (`PROTOCOL_VERSIONS` field)                       | Tag family   | Location inside a pre-authority tag                                           |
| ------------------------------------------------------ | ------------ | ----------------------------------------------------------------------------- |
| `host`                                                 | `daemon-v*`  | `packages/host/src/protocol.ts` → `HOST_PROTOCOL_VERSION`                     |
| `outbound`                                             | `daemon-v*`  | `packages/host/src/outbound-protocol.ts` → `OUTBOUND_PROTOCOL_VERSION`        |
| `desktopApi` / `desktopMinClient` / `desktopMinServer` | `desktop-v*` | `packages/shared/src/desktop-server-api-epoch.ts` → the three epoch constants |

```
# Daemon axes (host, outbound) — newest daemon-v* tag:
TAG=$(git tag --list 'daemon-v*' --sort=-v:refname | head -1)
git show "$TAG:packages/shared/src/protocol-versions.ts"    # authority (host, outbound)
# ...or, for a tag cut before that file existed (e.g. daemon-v0.1.9), read BOTH
# old per-axis files — never a single path that could match the wrong axis:
git show "$TAG:packages/host/src/protocol.ts"           # HOST_PROTOCOL_VERSION
git show "$TAG:packages/host/src/outbound-protocol.ts"  # OUTBOUND_PROTOCOL_VERSION

# Desktop triple — newest desktop-v* tag:
TAG=$(git tag --list 'desktop-v*' --sort=-v:refname | head -1)
git show "$TAG:packages/shared/src/backend-admission.json"        # authority (the triple)
# ...or, for a tag cut before that file existed, read protocol-versions.ts;
# for an even older tag (e.g. desktop-v0.2.3), read the
# old desktop-epoch file — NOT host/protocol.ts, which exists in that tag and
# would silently return the wrong axis:
git show "$TAG:packages/shared/src/protocol-versions.ts"
git show "$TAG:packages/shared/src/desktop-server-api-epoch.ts"   # the three epoch constants
```

Read the axis from the file the table names for its OWN tag family; do not use a
generic `||` fallback, because the daemon files exist in a `desktop-v*` tag (and
vice versa) and would succeed on the wrong axis. Do NOT transcribe the historical
number into this doc — read it from the tag each time.

The desktop also bundles and spawns the daemon, so a `desktop-v*` tag ships the
daemon axes too. For `host` / `outbound`, the last shipped value is the newest of
the two: read `packages/shared/src/daemon-admission.json` from the newest
`desktop-v*` tag as well, and compare against the higher of the two values.

Compare each field being released to the value that tag shows and confirm the
step is 0 or +1.

**A +2 is not "two breaking changes" — it is a skipped generation, and a bug.**
Multiple breaking changes that never shipped separately fold into ONE generation
(two peers built from the same tree are never a mixed pair), so they still move
the axis by a single +1. A step of +2 means a value was minted for a generation
no artifact ever carried — delete the phantom step and collapse to +1 before
tagging. (This is exactly the drift #task:2110 corrected: the branch had jumped
host/outbound/desktop by +3/+3/+2 over their published tags.)

An unchanged axis needs no ceremony; only a CHANGED axis is checked. The daemon
side records the same rule at the outbound gate in
`docs/daemon-outbound-protocol.md`.

## 7. Known residuals

- The desktop-side gate is advisory UX; enforcement authority stays
  server-side (a modified client can skip its own gate but not the 426).
- Race window: if the server bumps between the desktop's server-info
  precheck and a business request, that single request 426s before the next
  re-probe corrects the classification.
