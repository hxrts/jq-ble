# jq-ble

`jq-ble` contains Jacquard adapters that let Jacquard use Bluetooth Low Energy through [`blew`](https://github.com/mcginty/blew).

Jacquard is a mesh-routing system with a synchronous routing core. `blew` is an async cross-platform BLE library that provides central/peripheral roles plus GATT and optional L2CAP support. This repo bridges those models so Jacquard can observe BLE links, route over them, and expose host-facing APIs around that transport.

## Crates

- `crates/jq-link-profile` - BLE link profile and runtime owner task for Jacquard
- `crates/jq-client` - Host bridge, runtime effects, topology projection, and client API
- `crates/jq-node-profile` - Shared node and topology snapshot models
- `crates/demo` - Generic host integration demo showing how an app environment can wrap the Jacquard BLE client and expose messaging, topology, and optional peer-introduction flows

## Development

- `cargo check --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace`

## License Note

The `jq-ble` code in this repository is licensed under Apache 2.0.

However, it depends on the `blew` library, which is licensed under AGPL-3.0.
Users of `jq-ble` should be aware of that transitive copyleft obligation.
