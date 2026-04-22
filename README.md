# jq-ble

`jq-ble` contains adapters that allow the [`jacquard`](https://github.com/hxrts/jacquard) router to use Bluetooth Low Energy via [`blew`](https://github.com/mcginty/blew).

`jacquard` is a mesh-routing system with a synchronous routing core. `blew` is a cross-platform async BLE library that provides central/peripheral roles + GATT and optional L2CAP support. `jq-ble` bridges these two models, letting jacquard observe BLE links, route over them, and expose host-facing APIs around the BLE transport.

Unicast sends use point-to-point BLE paths: L2CAP when available, otherwise central GATT writes. Peripheral GATT notifications are exposed only through explicit multicast fanout intent. `blew` targets notifications by subscriber on Apple and Android; Linux/BlueZ falls back to characteristic-wide broadcast, so jq-ble only uses notify fanout for an admitted multicast receiver set. BLE advertising remains discovery/control-plane only.

## BLE Configuration

`JacquardBleClient::new(local_node_id)` uses conservative defaults. Call
`JacquardBleClient::new_with_config(local_node_id, BleConfig { .. })` or
`JacquardBleService::new_with_ble_config(...)` when a host needs platform
policy:

- Apple/CoreBluetooth restore identifiers for central and peripheral roles.
- Optional startup readiness timeout before scanning or publishing services.
- Scan service UUID filters and low-latency/low-power scan mode.

When restore identifiers are configured, jq-ble constructs `blew` with
`with_config`, drains restored state before issuing new BLE work, and republishes
L2CAP listeners because L2CAP channels are not restored. iOS background scanning
requires a non-empty service UUID filter; an empty filter remains the foreground
default.

## Crates

- `crates/jq-link-profile` - BLE link profile and runtime owner task for `jacquard`
- `crates/jq-client` - Host bridge, runtime effects, topology projection, and client API
- `crates/jq-node-profile` - Shared node and topology snapshot models
- `crates/demo` - Generic host integration demo showing how an app environment can wrap the `jacquard` BLE client and expose messaging, topology, and optional peer-introduction flows

## Development

```sh
# Enter nix dev shell
nix develop

# Build the workspace
cargo build --workspace

# Run all tests
cargo test --workspace

# Run the full local CI dry run
just ci-dry-run
```

## License

`jq-ble` is licensed under Apache 2.0, but depends on `blew`, which is licensed under [AGPL-3.0](https://github.com/mcginty/blew/blob/main/LICENSE). Users should be aware of this transitive copyleft obligation.
