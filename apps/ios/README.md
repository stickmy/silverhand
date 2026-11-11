# Silverhand iOS

Capacitor 8 hosts the bundled `@silverhand/app-ui`. Swift implements the
`ClientPlatform` adapter; the phone never claims a computer or starts a daemon.
The server is an HTTPS API origin, never a WebView navigation target. No Ionic UI,
React Native, APNs, background execution mode or relaxed TLS policy is included.

## Build

Requirements: Node/pnpm from the repository, Xcode 26 or newer with its license
and first-launch setup complete, and an installed iOS simulator runtime.

```sh
pnpm install
SILVERHAND_DEFAULT_SERVER_URL=https://your-server.example pnpm --filter @silverhand/ios sync
pnpm --filter @silverhand/ios open
```

Choose the App scheme and a simulator. Simulator builds need normal ad-hoc
signing (`CODE_SIGN_IDENTITY=-`), not `CODE_SIGNING_ALLOWED=NO`: Keychain access
requires a signed process. A physical device requires a Development Team and
provisioning choice valid for the configured bundle identifier.

`SILVERHAND_DEFAULT_SERVER_URL` is optional and is captured by Capacitor during
`sync` (or `cap copy ios`), then bundled for the native client. It must be an HTTPS
origin without credentials, query, fragment or a path. An invalid value fails the
sync; an unset/blank value supplies no default. It is not a secret and does not
change the WebView URL. Xcode builds use the last synced value, not the shell or
scheme's runtime environment. Re-run sync with the variable to change it, or
without the variable to remove it. A plain `pnpm build` only rebuilds Web assets.

```sh
xcodebuild -project apps/ios/ios/App/App.xcodeproj -scheme App \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath .cache/ios-derived CODE_SIGN_IDENTITY=- build
```

Swift Package Manager owns the Capacitor dependency. `cap sync ios` regenerates
CapApp-SPM; the application separately references the local `native` package, so
sync never overwrites the session implementation. Commit Package.resolved.
`vite build` generates the native wire contract from the shared API/epoch authority.

## Phone navigation

Compact windows use full-screen list → conversation → thread/attachments/settings
pages, with native Chats / Activity / Tasks tabs and an upward More menu for
Members / Computers / Settings. The shared router resolves semantic destinations;
Desktop keeps its simultaneous panes. Tab return paths and reading positions live
inside the authenticated session; composer drafts and send identities retain their
existing conversation/thread ownership. The four existing themes supply the UI tokens.

Fresh installs use the build's default server if configured; otherwise they ask
for a server address. A saved custom origin always wins. Login exposes Change server before authentication;
Settings → Connection remains available after login. Discovery must succeed before
submitting credentials; network/TLS errors leave retry and address repair available.

## Session and network behavior

- One Keychain item binds server origin and user token atomically. It is available
  after first unlock and restricted to this device; it is never copied into
  JavaScript, cookies in WKWebView, localStorage or the app's ordinary files.
- URLSession uses ephemeral storage, no automatic cookies, no credential cache,
  standard system TLS validation, and refuses redirects. Generic requests can
  address only `/api/` under the active server, with native-owned bearer/epoch
  headers. WS uses the server's existing Origin/session-cookie/epoch admission.
- Generation checks reject late HTTP and login results. Socket identity and
  generation checks suppress frames from retired connections. Native state
  changes commit before retargeting; Keychain errors do not commit a new server.
- Server changes, logout and session invalidation clear the app's browser
  storage and reload the app, dropping all module/query/socket state. Background
  entry closes sockets; foreground entry revalidates the session, reconnects WS
  and refreshes snapshots in the existing React tree, preserving composer drafts
  and pending send identities. Transient network failures retain the mounted
  conversation. Background delivery is not promised.
- PNG export uses the native share sheet. Attachment Download reads original bytes
  through the authenticated backend and opens the system document picker to save a
  copy in Files. Cancellation is silent; completion or dismissal removes the
  temporary source file. Direct PNG save/copy and opening attachments in external
  apps remain unsupported; ordinary file selection uses WebKit. Desktop-only
  capabilities are absent, never replaced with successful stubs.

## Tests

Formatting uses the repository's oxfmt version and Desktop style conventions:

```sh
pnpm --filter @silverhand/ios fmt
pnpm --filter @silverhand/ios fmt:check
```

These commands cover the Web client, build configuration, scripts and docs.
Generated assets and `ios/` / `native/` are excluded; oxfmt does not format Swift.

```sh
pnpm --filter @silverhand/ios typecheck
pnpm --filter @silverhand/ios test
swift test --package-path apps/ios/native
```

The macOS Swift run exercises session races/storage failures and skips the iOS
network acceptance test. For native HTTPS/WSS against the real server:

1. Build `cargo build --manifest-path rust/Cargo.toml -p silverhand-backend-app --bin silverhand-backend`.
2. Create and boot a **dedicated disposable simulator**; never use a personal
   device for the fixture. Run `node apps/ios/scripts/ios-acceptance-server.mjs <simulator-uuid>`.
   It creates a two-day CA and isolated backend under `.cache`, and trusts that
   CA only in the named simulator. TLS validation stays enabled.
3. Run `xcodebuild -project apps/ios/ios/App/App.xcodeproj -scheme App -destination
'platform=iOS Simulator,id=<uuid>' -parallel-testing-enabled NO CODE_SIGN_IDENTITY=- test`.
   Use the hosted AppTests target: hostless iOS SwiftPM tests cannot access Keychain.
4. Stop the fixture with Ctrl-C; delete the dedicated simulator when finished.

The network test covers Keychain cold restore, signup/login, authenticated WS,
channel creation and message exchange, real WKWebView plugin → shared React composer
→ HTTP send → native WS event → React message-list reception, background refusal and foreground snapshot
catch-up, password invalidation and logout. The hosted WebView test also posts
UIKit lifecycle notifications through the real plugin and verifies that the same
composer DOM node, draft and missed messages survive resume. This exercises the
notification-to-page chain, not OS process suspension or termination. It is transport acceptance, not a
claim that a physical iPhone or the entire mobile interface was exercised.

## Remaining acceptance

Connect a development-enabled iPhone, select the user's signing Team, and repeat
login/WS/message/suspend/resume/switch-server checks with an isolated, reachable
HTTPS server. Verify keyboard and message composer behavior on-device. Full-screen navigation and separate parent/thread drafts are covered by the hosted
WebView tests; real keyboard, touch and OS suspension still require device acceptance.
Remaining attachment capabilities and APNs require separate acceptance.

References: [Capacitor local Swift plugins](https://capacitorjs.com/docs/ios/custom-code),
[iOS setup](https://capacitorjs.com/docs/ios),
[configuration](https://capacitorjs.com/docs/config).
