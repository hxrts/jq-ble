#!/usr/bin/env bash

# Publishable crates in release order (leaves first, dependents last).
# Mirrors the production dependency graph — dev-only deps do not appear here.
#
# Dependency order:
#   jq-node-profile, jq-link-profile → jq-client
RELEASE_PACKAGES=(
  "jq-node-profile"
  "jq-link-profile"
  "jq-client"
)

manifest_path() {
  local crate="$1"
  case "${crate}" in
    jq-node-profile) echo "crates/jq-node-profile/Cargo.toml" ;;
    jq-link-profile) echo "crates/jq-link-profile/Cargo.toml" ;;
    jq-client)       echo "crates/jq-client/Cargo.toml" ;;
    *)
      echo "unknown package: ${crate}" >&2
      return 1
      ;;
  esac
}

release_deps_for() {
  local crate="$1"
  case "${crate}" in
    jq-node-profile|jq-link-profile)
      ;;
    jq-client)
      printf '%s\n' "jq-node-profile" "jq-link-profile"
      ;;
    *)
      echo "unknown package: ${crate}" >&2
      return 1
      ;;
  esac
}
