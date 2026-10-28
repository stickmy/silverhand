# Building & Releasing the Desktop App

For hosted arm64 embedded/remote-only DMG builds, repository configuration and
tag/manual triggers, see [Desktop GitHub Actions](desktop-github-actions.md).

> TS backend 退役说明：文中的 `packages/backend` / `packages/server` 源码与测试坐标属于迁移期历史，已从当前仓库删除；现行控制面位于 `rust/crates`。迁移权威仍在 `packages/shared/src/db`，独立回归数据在 `rust/test-fixtures`，现行构建与验证见 [testing.md](testing.md)。

End-to-end recipe for producing a distributable `.dmg` from the
silverhand source tree, plus the **list of footguns we already
stepped on** so the next person doesn't.

> Scope: macOS only. Per `CLAUDE.md` v1 ships Mac-only; nothing
> below has been tested on Win/Linux and at least the
> `process.platform === 'darwin'` PATH-fix branch in `main/index.ts`
> would silently no-op on other OSes.

---

## TL;DR

```fish
# 1. (one-time) make the icon — drop a 1024×1024 PNG at
#    apps/desktop/build/icon.png, then:
cd apps/desktop/build
mkdir -p icon.iconset
# (sips/iconutil recipe — see apps/desktop/build/README.md)
iconutil -c icns icon.iconset
rm -r icon.iconset

# 2. build (from repo root)
pnpm package           # universal off, arm64 dmg + zip in apps/desktop/release/
pnpm package:dir       # faster: skip dmg, just .app for smoke testing; JS stays readable
pnpm package -- --no-minify  # debug dmg/zip with readable JS bundles
```

Artifacts land in `apps/desktop/release/`:

- `Silverhand-<version>-arm64.dmg`
- `Silverhand-<version>-arm64-mac.zip`
- `mac-arm64/Silverhand.app/` (raw bundle)

---

## Development daemon launcher

`pnpm dev` builds `@silverhand-ai/daemon` before starting Desktop. Desktop launches `packages/silverhand-daemon/dist/daemon.js`, whose sibling `cli.js` supplies the daemon-local CLI for `runtime_start`. The host-only `packages/host/dist/daemon.js` bundle does not have that sibling and is not the Desktop dev entry.

Both Desktop layouts pin `ELECTRON_RUN_AS_NODE=1` in the CLI shim, so tool shells that scrub inherited environment can still invoke the Electron binary as Node. Daemon-local launchers retain priority over server-supplied launchers; missing both remains an explicit error. This does not change the packaged Resources layout or remote launcher authority.

## Explicit embedded backend acceptance

`pnpm build` runs package build scripts (`pnpm -r build`). It does not call the release packager or start the embedded backend smoke.

`pnpm package` / `pnpm package:dir` build release artifacts. They retain sidecar target/manifest/hash checks, packaged daemon and version verification, and code-signature verification. They do not start a backend service for acceptance, either before or after packaging.

Run process acceptance explicitly, using one of these commands from the repository root:

```sh
pnpm smoke:embedded-backend --sidecar-stage /absolute/path/to/sidecar-stage
pnpm smoke:embedded-backend /absolute/path/to/Silverhand.app/Contents/Resources
```

The stage must contain the executable and matching `silverhand-backend.manifest.json`. Without an argument, the existing development sidecar resolution is used; the smoke does not build a binary. Use artifacts from the matching source generation.

The smoke creates a fresh repository-local `.cache/embedded-smoke/<uuid>` tree and supplies isolated application data, database, logs, and temporary paths. Its success path supplies allowed origins and checks startup, readiness, API epoch, bootstrap, an authenticated HTTP route, WebSocket admission, and clean shutdown. It then deliberately passes empty allowed origins: the logged `SILVERHAND_ALLOWED_ORIGINS missing` is an expected rejection test, announced before execution. The expected typed rejection and cleanup are asserted; unexpected failures still fail the command.

A successful build/package is not process acceptance. Run the explicit smoke before release when that evidence is required. No existing user instance or database is used.

## How the pieces fit

```
scripts/package.sh
   ├── selected shared / CLI / host / daemon builds
   ├── pnpm --filter @silverhand/desktop build   # electron-vite → out/{main,preload,renderer}
   ├── cargo build --release + sidecar manifest/hash (embedded package only)
   ├── electron-builder --mac --config ...
   └── static daemon / version / signature verification
              ↓
       apps/desktop/release/
```

`pnpm package` minifies shipped JavaScript by default; `pnpm package:dir` does
not, because the raw `.app` path is for smoke/debug iteration. The minified
release path covers the Electron main/preload/renderer output plus the extra
shipped CLI and daemon bundles. The standalone server is built with Cargo. Use `--no-minify` (or `SILVERHAND_MINIFY=false`) when producing a
debug dmg/zip whose JS should remain readable, or `--minify` to force a
minified `--dir` build. This is a size/readability knob, not an obfuscation or
security boundary.

Three things this dance handles that electron-builder alone can't:

1. **Rust sidecar staging.** Embedded builds include the verified Rust binary
   and manifest. Remote-only builds omit both. No JavaScript SQLite binding or
   Electron ABI swap is involved.

2. **Silverhand CLI bundling.** `packages/cli` produces a single
   esbuild bundle at `dist/cli.js` (≈200KB, emitted as ESM).
   electron-builder ships it outside the asar as
   `<app>/Contents/Resources/silverhand-cli/cli.mjs` — the `.mjs`
   rename is important: without a sibling `package.json` type=module
   Node would otherwise parse it as CommonJS and choke on `import`.
   In dev the repo file is loaded directly because
   `packages/cli/package.json` already sets `type: module`. Both paths
   invoke it through Electron's own Node runtime (`process.execPath`
   - `ELECTRON_RUN_AS_NODE=1`) so no extra Node or Bun runtime is
     bundled. The backend exposes it to spawned agents by writing a
     per-spawn `bin/silverhand` shim and prepending that `bin/` to
     `PATH`.

3. **PATH from login shell.** GUI-launched .app's inherit
   launchd's minimal `/usr/bin:/bin:/usr/sbin:/sbin` — they do NOT
   see `~/.zshrc` / `config.fish` PATH additions where users have
   `claude` / `codex` installed. `apps/desktop/src/main/index.ts`'s
   `patchPathFromLoginShell()` runs `$SHELL -ilc 'printenv PATH'`
   at startup and copies the result into `process.env.PATH` (only
   in packaged darwin builds). `printenv` is used instead of `echo
$PATH` because it's a real binary, so the same incantation
   works in fish (which doesn't accept `${VAR}` syntax) AND
   bash/zsh.

---

## Footguns the easy way (i.e. retrospective)

| #   | symptom                                                                                                                                                                     | cause                                                                                                                                                                                                                                                                                                                                                                                                          | fix                                                                                                                                                                                                                                                                                                                                                                                                   |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `EOF` / `bad file descriptor` downloading `electron-v33.x.x-darwin-x64.zip`                                                                                                 | github.com/electron/electron releases unreliable from CN                                                                                                                                                                                                                                                                                                                                                       | `scripts/package.sh` exports `ELECTRON_MIRROR=https://npmmirror.com/mirrors/electron/`                                                                                                                                                                                                                                                                                                                |
| 2   | `ERR_ELECTRON_BUILDER` "must be under apps/desktop/" walking into `packages/backend/dist/...`                                                                               | electron-builder follows the pnpm symlink for workspace deps                                                                                                                                                                                                                                                                                                                                                   | `@silverhand/backend` + `@silverhand/shared` moved to `devDependencies` (electron-vite bundles them at build time, no runtime require). electron-builder by default only walks production deps                                                                                                                                                                                                        |
| 3   | "runtime 'codex' is not installed (no binary on PATH)" only in packaged build, fine in dev                                                                                  | macOS GUI apps inherit launchd PATH, not the shell PATH                                                                                                                                                                                                                                                                                                                                                        | login-shell PATH probe in `main/index.ts` (see point 3 above)                                                                                                                                                                                                                                                                                                                                         |
| 4   | `silverhand: command not found` inside an agent                                                                                                                             | CLI bundle missing from the packaged resources, or the per-spawn PATH shim was not prepended                                                                                                                                                                                                                                                                                                                   | ensure `pnpm --filter @silverhand/cli build` runs before packaging and `extraResources` copies to `silverhand-cli/cli.mjs` (the `.mjs` rename is required — Node rejects ESM `.js` without a sibling `package.json` type=module)                                                                                                                                                                      |
| 5   | codex spawns OK but can't reach chatgpt.com                                                                                                                                 | proxy URL had `https://` scheme; almost all local proxies (Clash/mihomo) speak plain HTTP CONNECT                                                                                                                                                                                                                                                                                                              | `expandProxyEnv()` in `packages/host/src/proxy.ts` rewrites `https://` → `http://` and derives `socks5://` for `all_proxy`                                                                                                                                                                                                                                                                            |
| 6   | packaged `.app` won't open / "is damaged" after `package.sh`                                                                                                                | `mac.identity: null` does NOT ad-hoc sign — electron-builder _skips_ signing, leaving only Electron's stub sig (Identifier=Electron, no CodeResources) → invalid bundle                                                                                                                                                                                                                                        | `scripts/afterPack.cjs` ad-hoc deep-signs the .app before dmg/zip are built. See "Signing & notarization" below                                                                                                                                                                                                                                                                                       |
| 7   | (electron-builder **26+**) dmg step fails with `404` for `cdn.npmmirror.com/binaries/electron/dmg-builder@.../dmgbuild-bundle-*.tar.gz`                                     | v26's dmg-builder fetches a `dmgbuild` bundle via `@electron/get`, which honors `ELECTRON_MIRROR` over the mirror it passes — so our _electron_ mirror hijacks the _electron-builder-binaries_ URL to a path that 404s                                                                                                                                                                                         | `scripts/package.sh` `stage_dmgbuild_bundle()` pre-downloads the bundle from the electron-builder-binaries mirror (github fallback), checksum-verifies it, and exports `CUSTOM_DMGBUILD_PATH` so dmg-builder skips the network                                                                                                                                                                        |
| 8   | packaged app crashes at launch: `Error: Failed to load native binding` from `app.asar/node_modules/@node-rs/argon2/index.js`                                                | `@node-rs/argon2`'s native binding ships in a platform-specific optional dep (`@node-rs/argon2-darwin-arm64`) that pnpm nests in its virtual store; electron-builder doesn't pull nested optional native deps into the asar (electron-userland/electron-builder#6289)                                                                                                                                          | declare `@node-rs/argon2-darwin-arm64` as a direct `optionalDependencies` entry in `apps/desktop/package.json` (surfaces it so electron-builder includes it) **and** add it to `asarUnpack` so the `.node` lands on disk. Chose this over `node-linker=hoisted` to avoid reshaping the whole monorepo's node_modules (would disrupt the better-sqlite3 ABI swap)                                      |
| 9   | packaged app **AND** `pnpm dev` both fail with `NODE_MODULE_VERSION 137 vs 130` for `better-sqlite3` from the workspace path — even right after a successful `pnpm package` | `_native.sh` `ensure_binding` trusted its `.active-abi` stamp blindly: if a later `pnpm install` re-extracted the Node-ABI prebuild on top of the .node file, the stamp still said "130 (Electron)", so dev/test/package all skipped the swap and shipped the Node-ABI binding. electron-builder can also materialize `app.asar.unpacked` after the workspace-level swap, so the final app must be checked too | `_native.sh` now records the binding's SHA-256 at swap time and re-checks it on every call; hash drift → log a clear "file was replaced — probably by pnpm install" line and re-swap from the per-package-version/per-ABI cache. `package.sh` also stages the Electron-ABI binding for `afterPack`, restores it into `app.asar.unpacked`, and runs an Electron smoke require against the packaged app |

---

## When the version bumps

1. Bump `apps/desktop/package.json` `version` field. electron-builder
   uses this in artifact filenames (`Silverhand-X.Y.Z-arm64.dmg`).
2. `pnpm package`.
3. Smoke-test the .app: drag from dmg to /Applications, right-click
   → Open (Gatekeeper warns because we ad-hoc sign — see below),
   confirm logger first line in `~/.silverhand/logs/desktop.YYYY-MM-DD.log`
   reports `pathPatch.ok=true`.
4. Tag the release commit + upload the dmg + zip wherever.

---

## Adding x64 / Universal back

Currently `electron-builder.yml` has `target.arch: [arm64]`. To
ship for Intel too, add `- x64` (or `- universal`):

```yaml
mac:
  target:
    - target: dmg
      arch:
        - arm64
        - x64
```

Universal needs both arch's Electron zips downloaded. `ELECTRON_MIRROR`
should help; if it still EOFs, manually drop the zips into the
electron-builder cache (`~/Library/Caches/electron/`) then retry.

---

## Signing & notarization

### How we ad-hoc sign (and why it's not via `identity`)

The app is **ad-hoc signed** so macOS will open it on the build machine.
Important gotcha that bit us once: `mac.identity: null` does **NOT** ad-hoc
sign. In electron-builder it means _skip signing entirely_ — the packager logs
`skipped macOS code signing  reason=identity explicitly is set to null` and
returns (see `app-builder-lib` `macPackager.js`: `if (qualifier === null) {
...; return false }`). With signing skipped, the bundle keeps only Electron's
own linker-signed stub (`Identifier=Electron`, `Info.plist` not bound, no
`CodeResources`), which macOS rejects as an invalid bundle — the packaged app
**won't open**, and once the dmg/zip add a quarantine xattr it's a hard
Gatekeeper block.

The actual ad-hoc signature is applied by **`apps/desktop/scripts/afterPack.cjs`**
(wired via `afterPack:` in `electron-builder.yml`). It runs `codesign --deep
--force --sign -` on the assembled `.app` after files/asar/extraResources are
in place but **before** the dmg/zip are built, so the artifacts ship a valid
signature (`Identifier=com.silverhand.desktop`, `Info.plist` bound,
`--verify --deep --strict` passes).

- Why `afterPack`, not `afterSign`: electron-builder **skips the `afterSign`
  hook when no signing occurred** (`skipping "afterSign" hook as no signing
occurred`). Since builtin signing is disabled, only `afterPack` runs.
- We keep `identity: null` so the builtin signer cleanly no-ops; the hook is
  the single source of the signature. (electron-builder 26 additionally prints
  `arm64 requires signing, but identity is set to null and signing is being
skipped` — that warning is expected and harmless; our hook covers it.)
- Ad-hoc apps still fail `spctl -a` (rejected) — that's why
  `gatekeeperAssess: false` is set. They launch fine once the quarantine xattr
  is cleared (right-click → Open the first time, or `xattr -dr
com.apple.quarantine <app>`).

### macOS local-network identity

Both package profiles declare `NSLocalNetworkUsageDescription`. This supplies
the permission prompt's explanation; it does not grant network access.

Before signing, `afterPack` replaces the **packaged main executable's** `LC_UUID`
with a fresh UUID. The development Electron in `node_modules` is not modified.
macOS can confuse apps that reuse Electron's executable UUID even when their
bundle IDs differ; see [Apple TN3178](https://developer.apple.com/documentation/technotes/tn3178-checking-for-and-resolving-build-uuid-problems).
The patch is restricted to the existing thin arm64 target and fails closed for
other formats. No executable code, framework UUIDs, bundle ID or data paths change.
Any future universal/x64 packaging support must extend this validation first.

`Contents/Resources/silverhand-executable-uuid.json` records the upstream and
packaged main-executable UUIDs. Upstream Electron main-executable dSYM/Breakpad
symbols must be remapped using that mapping before automatic symbol lookup;
the mapping alone does not perform symbolication. Framework symbols are unchanged.
The UUID change invalidates the old signature, so signing must always follow it.
The hook verifies the final signature and UUID. This does not guarantee existing
macOS permission records are immediately repaired: test local-network access
from the new app and its children separately. Developer ID signing remains the
recommended distribution identity; ad-hoc signing is not a substitute.

### Developer ID builds and in-app updates

`SILVERHAND_MAC_SIGN_IDENTITY="<name> (<team-id>)" ./scripts/package.sh ...`
signs the sidecar and app with that Developer ID and hardened runtime;
electron-builder notarizes when `APPLE_API_KEY`/`APPLE_API_KEY_ID`/`APPLE_API_ISSUER`
are set. `afterPack` then skips ad-hoc signing.

Only these builds keep `Resources/app-update.yml` (GitHub feed, `channel:
embedded|remote-only`), so only they check for updates. Ad-hoc builds drop it
and show no update entry: Squirrel.Mac rejects an update whose signature does
not satisfy the running app's designated requirement. An ad-hoc install
therefore moves to the first signed release by manual DMG install; in-app
updates (differential when the previous zip is cached, else full) work between
signed releases only.

- For the App Store: completely different config (`mas` target,
  provisioning profiles, App Sandbox entitlements). Out of scope here.

---

## Where the magic lives

| file                                 | role                                                                                                    |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------- |
| `scripts/package.sh`                 | wrapper: build → stage Rust sidecar (embedded only) → electron-builder                                  |
| `apps/desktop/electron-builder.yml`  | files / asarUnpack / extraResources / mac config                                                        |
| `apps/desktop/scripts/afterPack.cjs` | ad-hoc `codesign --deep` of the .app and drops its update feed; skipped signing for Developer ID builds |
| `apps/desktop/build/`                | icon + README with `sips` recipe                                                                        |
| `apps/desktop/build/icon.icns`       | gets baked into `Contents/Resources/icon.icns`                                                          |
| `apps/desktop/src/main/index.ts`     | `patchPathFromLoginShell()` + `resolveSilverhandCliLauncher()` packaged-build path                      |
| `packages/host/src/proxy.ts`         | one-URL → three env-vars expansion                                                                      |
