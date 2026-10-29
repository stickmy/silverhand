#!/usr/bin/env bash
# Check formatting, then run the JavaScript suites. SQLite is owned by Rust, so no ABI swap.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
# Suites that touch app data require an isolated root; never the user's ~/.silverhand.
export SILVERHAND_DIR="$ROOT_DIR/.cache/test-artifacts/silverhand-dir"
mkdir -p "$SILVERHAND_DIR"
PACKAGES=(shared host cli client app-ui desktop ios)
scope=all
case "${1:-}" in
  --scope) scope="${2:?--scope requires a package}"; shift 2 ;;
  --scope=*) scope="${1#--scope=}"; shift ;;
esac
if [[ "$scope" != all ]]; then
  for known in "${PACKAGES[@]}"; do
    if [[ "$known" == "$scope" ]]; then
      pnpm --filter "@silverhand/$scope" fmt:check
      exec pnpm --filter "@silverhand/$scope" test "$@"
    fi
  done
  echo "Unknown test scope: $scope (expected: ${PACKAGES[*]} all)" >&2
  exit 2
fi
if [[ "$#" -gt 0 ]]; then
  echo 'Use --scope <package> for targeted test arguments.' >&2
  exit 2
fi
for pkg in "${PACKAGES[@]}"; do
  pnpm --filter "@silverhand/$pkg" fmt:check
  pnpm --filter "@silverhand/$pkg" test
done
