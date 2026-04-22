# jq-ble

## What This Repo Is

This repository contains Jacquard adapters that let Jacquard use BLE through [`blew`](https://github.com/mcginty/blew).

Jacquard is a mesh-routing system with a synchronous routing core. It models links, observations, routes, and topology explicitly, and expects hosts to own time, ingress batching, and round advancement.

`blew` is a cross-platform Rust BLE library that provides central and
peripheral roles plus GATT and optional L2CAP support. It is asynchronous and owns live BLE runtime state.

The main job of this repo is to bridge those two worlds cleanly:

- Jacquard stays synchronous
- BLE runtime ownership stays asynchronous
- the bridge between them is explicit and testable

## Workspace Layout

- `crates/jq-link-profile` - Jacquard BLE link profile. Owns the BLE runtime task and maps `blew` discovery, identity resolution, sessions, and payload ingress/egress into Jacquard transport observations and outbound commands.

- `crates/jq-client` - Host bridge and client assembly. Wires the Jacquard router, BLE link profile, topology projector, and runtime effects into a usable client API.

- `crates/jq-node-profile` - Shared topology and node snapshot models used by clients and UI-facing surfaces.

- `crates/demo` - Framework-neutral host integration demo. Shows how a host environment can wrap the Jacquard BLE client, publish events, and support optional out-of-band peer introduction.

## Architectural Rules

- One async BLE owner task owns all `blew` runtime state.
- Jacquard time and round cadence are owned by the host bridge, not by the BLE runtime.
- BLE advertisement hints are not full identity. Full remote identity must be resolved before emitting Jacquard link observations.
- The synchronous sender side only queues outbound commands. Actual BLE I/O is performed later by the async runtime owner.
- GATT notifications are explicit multicast/fanout in jq-ble. `blew` targets individual subscribers on Apple/Android, but Linux/BlueZ falls back to characteristic-wide broadcast, so node-targeted sends must use L2CAP or central GATT writes.
- BLE advertising is discovery/control-plane only; routed payloads must not use advertising.
- Apple/CoreBluetooth restoration is configured through jq-ble `BleConfig` restore identifiers. Restored state must be drained before scan/connect/service work; L2CAP listeners are republished because L2CAP channels are not restored.

## External BLE Dependency

This repo does not vendor `blew`. The workspace imports `blew` from crates.io:

- `blew = "0.2.3"`

If you need to inspect or change BLE substrate behavior, look there first.

## Common Commands

- `cargo check --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace`
- `cargo fmt --all`

## Toolkit

This repo uses [`hxrts/toolkit`](https://github.com/hxrts/toolkit) as a pinned Nix flake input. The toolkit provides:

- A pinned nightly rustfmt config (`toolkit-fmt`)
- Clippy wrapper (`toolkit-clippy`)
- Dylint lints (`toolkit-dylint`) — currently `trait_purity` is active
- `toolkit-xtask` for policy checks driven by `toolkit/toolkit.toml`

The active toolkit checks are: `proc_macro_scope`, `docs_semantic_drift`, `crate_root_policy`, `ignored_result`, `unsafe_boundary`, `public_type_width`, `dependency_policy`, `text_formatting`, `result_must_use`, `test_boundaries`, `docs_link_check`, `workspace_hygiene`, `bool_param`, `must_use_public_return`, `assert_shape`, `drop_side_effects`, `recursion_guard`, `naming_units`, `limit_constant`, `workflow_actions`.

All checks must appear in both `justfile` (individual targets + `ci-dry-run` steps) and `.github/workflows/policy.yml`. The `scripts/preflight.sh` enforces this parity at the start of every dry run.

## Policy

Repo-specific lint and enforcement configuration lives in `toolkit/toolkit.toml`. Generic rules belong in the shared toolkit (flake input); `jq-ble`-specific rules go here.

## Editing Expectations

- Keep changes scoped to the Jacquard adapter crates in this repo.
- Avoid introducing hidden async ownership into Jacquard-facing APIs.
- Prefer tests that drive the bridge deterministically rather than depending on ambient timing.
