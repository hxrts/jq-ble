# jq-ble

`jq-ble` contains adapters that allow the [`jacquard`](https://github.com/hxrts/jacquard) router to use Bluetooth Low Energy via [`blew`](https://github.com/mcginty/blew).

`jacquard` is a mesh-routing system with a synchronous routing core. `blew` is a cross-platform async BLE library that provides central/peripheral roles + GATT and optional L2CAP support. `jq-ble` bridges these two models, letting jacquard observe BLE links, route over them, and expose host-facing APIs around the BLE transport.

Unicast sends use point-to-point BLE paths: L2CAP when available, otherwise central GATT writes. Peripheral GATT notifications are exposed only through explicit multicast fanout intent, and BLE advertising remains discovery/control-plane only.

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
