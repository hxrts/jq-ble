#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

resolve_pinned_toolkit_root() {
  if [ ! -f "$repo_root/flake.lock" ]; then
    return 1
  fi

  local toolkit_rev archive_path metadata_json cache_root cache_tmp
  toolkit_rev="$(
    perl -0ne '
      if (/"toolkit"\s*:\s*\{.*?"locked"\s*:\s*\{.*?"rev"\s*:\s*"([0-9a-f]+)"/s) {
        print $1;
      }
    ' "$repo_root/flake.lock"
  )"
  if [ -z "$toolkit_rev" ]; then
    return 1
  fi

  archive_path="${TOOLKIT_ROOT:-}"
  if [ -n "$archive_path" ] && [ ! -f "$archive_path/xtask/Cargo.toml" ]; then
    archive_path=""
  fi

  if [ -z "$archive_path" ]; then
    if ! command -v nix >/dev/null 2>&1; then
      return 1
    fi

    metadata_json="$(nix flake metadata --json "github:hxrts/toolkit/$toolkit_rev")" || return 1
    archive_path="$(
      printf '%s' "$metadata_json" | perl -0ne '
        while (/"path"\s*:\s*"([^"]+)"/g) {
          $path = $1;
        }
        END {
          print $path if defined $path;
        }
      '
    )"
    if [ -z "$archive_path" ]; then
      return 1
    fi
  fi

  cache_root="${XDG_CACHE_HOME:-$HOME/.cache}/toolkit/source-cache/$toolkit_rev"
  if [ ! -f "$cache_root/xtask/Cargo.toml" ]; then
    cache_tmp="${cache_root}.tmp.$$"
    mkdir -p "$(dirname "$cache_root")"
    rm -rf "$cache_tmp"
    cp -R "$archive_path" "$cache_tmp"
    chmod -R u+w "$cache_tmp" || true
    rm -rf "$cache_root"
    mv "$cache_tmp" "$cache_root"
  fi

  printf '%s\n' "$cache_root"
}

sanitize_path() {
  perl -e '
    my $path = $ENV{PATH} // q();
    my $home = $ENV{HOME} // q();
    my $cargo_home = $ENV{CARGO_HOME} // ($home eq q() ? q() : "$home/.cargo");
    my @drop = grep { $_ ne q() } (
      $home eq q() ? q() : "$home/.cargo/bin",
      $cargo_home eq q() ? q() : "$cargo_home/bin",
    );
    my %drop = map { $_ => 1 } @drop;
    my @parts = grep { $_ ne q() && !$drop{$_} } split(/:/, $path, -1);
    print join(":", @parts);
  '
}

run_sanitized() {
  local sanitized_path toolkit_root
  sanitized_path="$(sanitize_path)"
  toolkit_root="${TOOLKIT_ROOT:-}"
  if pinned_toolkit_root="$(resolve_pinned_toolkit_root)"; then
    toolkit_root="$pinned_toolkit_root"
  fi
  env \
    -u CARGO \
    -u RUSTC \
    -u RUSTDOC \
    -u RUSTUP_TOOLCHAIN \
    TOOLKIT_ROOT="$toolkit_root" \
    PATH="$sanitized_path" \
    "$@"
}

if [ "${1:-}" = "--inside-nix" ]; then
  shift
  if [ -z "${IN_NIX_SHELL:-}" ] || [ -z "${TOOLKIT_ROOT:-}" ] || ! command -v toolkit-xtask >/dev/null 2>&1; then
    echo "toolkit-shell.sh: --inside-nix requires the toolkit nix shell" >&2
    exit 1
  fi
  run_sanitized "$@"
  exit $?
fi

if [ -n "${IN_NIX_SHELL:-}" ] && [ -n "${TOOLKIT_ROOT:-}" ] && command -v toolkit-xtask >/dev/null 2>&1; then
  run_sanitized "$@"
  exit $?
fi

run_sanitized nix develop --command \
  "$repo_root/scripts/toolkit-shell.sh" --inside-nix "$@"
