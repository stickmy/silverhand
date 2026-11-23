# Desktop DMG builds on GitHub Actions

`.github/workflows/desktop-dmg.yml` calls the same `scripts/package.sh` used
locally. It builds embedded and remote-only apps in separate standard
`macos-15` ARM64 jobs. It does not build or deploy the standalone ECS server,
publish npm packages, bump versions, or create tags.

## Triggering

- **Manual:** Actions → Desktop DMG → Run workflow. This produces both variants
  as download artifacts, even when manually selecting a tag. It never publishes
  a Release. The workflow must first exist on the default branch.
- **Release:** push `desktop-vX.Y.Z`, exactly matching
  `apps/desktop/package.json.version`. Both builds must succeed before creating
  a Release. Old `desktop-vX.Y.Z-build.*` names and prerelease versions are not
  accepted by this workflow. Use manual runs for development builds.
- Ordinary branch pushes and daemon tags do not trigger packaging.

Version and source SHA come from the checked-out commit, not the latest main.
Node 22, the root `packageManager` pnpm version and Rust 1.99.0 are used; pnpm
and Cargo lockfiles are enforced. Actions are SHA-pinned. Rust compilation is
cached for embedded builds; target/config/toolchain changes invalidate its key.
Each build job has a 60-minute timeout, adjustable after hosted-runner evidence.

## Preparing a Desktop release

1. Find the previous published Desktop release in GitHub Releases. Review
   `git log --oneline desktop-vPREVIOUS..HEAD` and check the diffs for changes
   that affect Desktop users, including changes in shared code.
2. Commit `changelog/desktop/X.Y.Z.md` alongside the Desktop version bump.
   Lead with any required upgrade steps, then describe the features and fixes
   users will notice. Call out remote server or daemon dependencies when they
   matter. Mention platform support, packaging or signing only when it changes
   in this release. Finish with a GitHub comparison link between the two
   Desktop tags. See [0.5.3](../changelog/desktop/0.5.3.md).
3. Tag that commit and push the Desktop tag using the established release
   script. Tag builds check that the matching notes contain text before
   packaging. The release job uses the file from the tagged commit verbatim
   as the GitHub Release body. Manual artifact builds do not require notes.

## Outputs and recovery

Builds are Developer ID signed (hardened runtime) and notarized by
electron-builder. Repository Actions secrets: `MAC_CERT_P12_BASE64`,
`MAC_CERT_PASSWORD`, `APPLE_API_KEY_P8_BASE64`, `APPLE_API_KEY_ID`,
`APPLE_API_ISSUER`. The job fails if `spctl` or `stapler validate` rejects the app.

Each variant uploads only:

- `Silverhand-X.Y.Z-embedded-arm64.dmg` or `Silverhand-X.Y.Z-remote-only-arm64.dmg`
- Its `.dmg.sha256` sidecar (`shasum -a 256 -c FILE.dmg.sha256` on macOS).
- Its in-app update feed: `<variant>-mac.yml`, the zip it names and that zip's
  `.blockmap`.

Unpacked apps and caches are not uploaded as artifacts. Actions artifacts
expire after 7 days, including the temporary handoff artifacts for tag builds.
The tag Release retains both DMGs and checksums independently of that expiry.
Private-repository releases remain private.

The release job verifies both checksums, creates a draft and publishes it only
after all uploads succeed. It never overwrites an existing release or assets.
If uploading/publishing fails, inspect the draft before recovery: complete and
publish that draft manually, or explicitly delete only that failed draft before
rerunning the release job. A published release is not replaced on reruns.
Electron-builder automatic publishing is disabled; this workflow owns publishing.

## Verification

```sh
node --test scripts/ci/desktop-dmg.test.mjs
actionlint .github/workflows/desktop-dmg.yml
bash -n scripts/package.sh
```

Packaging also verifies app signatures, shipped version/source metadata and
daemon bytes using the existing script. Local static checks do not substitute
for the first hosted build: validate both downloaded DMGs on a test Mac before
announcing a release. No running user instance is started by this workflow.
