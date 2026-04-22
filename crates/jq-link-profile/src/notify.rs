use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::collections::BTreeSet;

use blew::types::DeviceId;
use jacquard_core::NodeId;

use crate::transport::PeerSessions;

#[cfg(target_os = "linux")]
pub(crate) fn platform_notify_device_ids(
    sessions: &BTreeMap<NodeId, PeerSessions>,
    receivers: &[NodeId],
    device_ids: &[DeviceId],
) -> Option<Vec<DeviceId>> {
    let requested = receivers.iter().copied().collect::<BTreeSet<_>>();
    let all_subscribers = sessions
        .iter()
        .filter_map(|(node_id, sessions)| sessions.notify_subscriber_device_id().map(|_| *node_id))
        .collect::<BTreeSet<_>>();
    if requested != all_subscribers {
        return None;
    }
    // BlueZ cannot target a specific subscribed central for GATT notify, so the Linux backend
    // broadcasts once to the characteristic after the receiver set is proven to match all subscribers.
    device_ids.first().cloned().map(|device_id| vec![device_id])
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn platform_notify_device_ids(
    _sessions: &BTreeMap<NodeId, PeerSessions>,
    _receivers: &[NodeId],
    device_ids: &[DeviceId],
) -> Option<Vec<DeviceId>> {
    Some(device_ids.to_vec())
}
