//! Static policy checks on the runtime source.
//!
//! Asserts that the runtime source does not assign Jacquard ticks and that
//! the sender type does not own BLE runtime handles, catching ownership
//! violations at test time without requiring hardware.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn runtime_source() -> String {
    let src_dir = repo_root().join("src");
    let mut sources = Vec::new();
    for path in [
        src_dir.join("transport.rs"),
        src_dir.join("task.rs"),
        src_dir.join("session.rs"),
        src_dir.join("l2cap.rs"),
    ] {
        sources.push(fs::read_to_string(path).expect("read runtime source"));
    }
    sources.join("\n")
}

#[test]
fn ble_driver_source_does_not_assign_jacquard_ticks() {
    let source = runtime_source();
    assert!(
        !source.contains(".observe_at("),
        "BLE driver code must not stamp Jacquard ticks"
    );
    assert!(
        !source.contains("observed_at_tick"),
        "BLE driver code must not assign Jacquard observation ticks"
    );
}

#[test]
fn sender_source_does_not_own_ble_runtime_handles() {
    let source = runtime_source();
    let sender_section = source
        .split("pub struct BleTransportSender")
        .nth(1)
        .and_then(|section| section.split("pub struct BleDriverControl").next())
        .expect("extract sender section");

    for forbidden in ["Central<", "Peripheral<", "L2capChannel", "EventStream"] {
        assert!(
            !sender_section.contains(forbidden),
            "sender must not directly own BLE runtime handle `{forbidden}`"
        );
    }
}
