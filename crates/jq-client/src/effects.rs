//! Runtime effects bundle for the BLE client.
//!
//! [`BleRuntimeEffects`] implements [`TimeEffects`], [`OrderEffects`],
//! [`StorageEffects`], and [`RouteEventLogEffects`] over a shared in-memory
//! state. It also implements [`BleBridgeTickSource`] so the host bridge can
//! advance the logical clock at each round boundary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use jacquard_core::{OrderStamp, RouteEventLogError, RouteEventStamped, StorageError, Tick};
use jacquard_traits::{
    OrderEffects, RouteEventLogEffects, StorageEffects, TimeEffects, effect_handler,
};

use crate::BleBridgeTickSource;

#[derive(Debug, Default)]
struct BleRuntimeState {
    now_tick: Tick,
    next_tick: Tick,
    next_order: u64,
    storage: BTreeMap<Vec<u8>, Vec<u8>>,
    events: Vec<RouteEventStamped>,
}

#[derive(Clone, Debug)]
pub struct BleRuntimeEffects {
    state: Arc<Mutex<BleRuntimeState>>,
}

impl BleRuntimeEffects {
    #[must_use]
    pub fn new(initial_tick: Tick) -> Self {
        // recursion-exception: constructor seeds the shared runtime state while retaining the conventional `new` entrypoint
        Self {
            state: Arc::new(Mutex::new(BleRuntimeState {
                now_tick: initial_tick,
                next_tick: initial_tick,
                ..Default::default()
            })),
        }
    }

    fn locked_state(&self) -> std::sync::MutexGuard<'_, BleRuntimeState> {
        self.state.lock().expect("ble runtime state lock")
    }
}

impl Default for BleRuntimeEffects {
    fn default() -> Self {
        Self::new(Tick(1))
    }
}

impl BleBridgeTickSource for BleRuntimeEffects {
    fn advance_tick(&mut self) -> Tick {
        let mut state = self.locked_state();
        // now_tick is what TimeEffects::now_tick() will return during this round.
        state.now_tick = state.next_tick;
        state.next_tick = Tick(state.next_tick.0.saturating_add(1));
        state.now_tick
    }
}

#[effect_handler]
impl TimeEffects for BleRuntimeEffects {
    fn now_tick(&self) -> Tick {
        self.locked_state().now_tick
    }
}

#[effect_handler]
impl OrderEffects for BleRuntimeEffects {
    fn next_order_stamp(&mut self) -> OrderStamp {
        let mut state = self.locked_state();
        // Monotonically increasing counter used to impose a deterministic ordering on router events.
        state.next_order = state.next_order.saturating_add(1);
        OrderStamp(state.next_order)
    }
}

#[effect_handler]
impl StorageEffects for BleRuntimeEffects {
    fn load_bytes(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.locked_state().storage.get(key).cloned())
    }

    fn store_bytes(&mut self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        self.locked_state()
            .storage
            .insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn remove_bytes(&mut self, key: &[u8]) -> Result<(), StorageError> {
        self.locked_state().storage.remove(key);
        Ok(())
    }
}

#[effect_handler]
impl RouteEventLogEffects for BleRuntimeEffects {
    fn record_route_event(&mut self, event: RouteEventStamped) -> Result<(), RouteEventLogError> {
        // In-memory append-only log; no persistence across restarts.
        self.locked_state().events.push(event);
        Ok(())
    }
}
