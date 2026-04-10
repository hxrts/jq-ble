#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

if [ -n "${IN_NIX_SHELL:-}" ] && [ -n "${TOOLKIT_ROOT:-}" ] && command -v toolkit-xtask >/dev/null 2>&1; then
  exec "$@"
fi

# Enter the repo dev shell so toolkit commands see the same pinned toolchain and
# platform-specific system libraries as the rest of CI.
exec nix develop --command "$@"
