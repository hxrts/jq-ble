use std::collections::BTreeMap;

use jacquard_core::{
    LinkRuntimeState, MulticastGroupId, NodeId, OrderStamp, RouteAdmissionRejection, Tick,
    TransportDeliverySupport, TransportObservation,
};

#[derive(Default)]
pub(crate) struct MulticastAdmission {
    subscribers: BTreeMap<NodeId, Tick>,
}

impl MulticastAdmission {
    pub(crate) fn ingest_transport_observation(&mut self, observation: &TransportObservation) {
        let TransportObservation::LinkObserved {
            remote_node_id,
            observation,
        } = observation
        else {
            return;
        };
        if observation.value.endpoint != jq_link_profile::gatt_notify_fanout_endpoint() {
            return;
        }
        if observation.value.state.state == LinkRuntimeState::Faulted {
            self.subscribers.remove(remote_node_id);
        } else {
            self.subscribers
                .insert(*remote_node_id, observation.observed_at_tick);
        }
    }

    pub(crate) fn support_for(
        &self,
        local_node_id: NodeId,
        group_id: MulticastGroupId,
        receivers: &[NodeId],
    ) -> Result<TransportDeliverySupport, RouteAdmissionRejection> {
        if receivers.is_empty()
            || receivers
                .iter()
                .any(|receiver| !self.subscribers.contains_key(receiver))
        {
            return Err(RouteAdmissionRejection::DeliveryAssumptionUnsupported);
        }

        jq_link_profile::gatt_notify_multicast_support(
            local_node_id,
            group_id,
            receivers.iter().copied(),
            latest_observed_tick(&self.subscribers, receivers),
            OrderStamp(0),
        )
        .ok_or(RouteAdmissionRejection::DeliveryAssumptionUnsupported)
    }
}

pub(crate) fn is_multicast_fanout_observation(observation: &TransportObservation) -> bool {
    matches!(
        observation,
        TransportObservation::LinkObserved { observation, .. }
            if observation.value.endpoint == jq_link_profile::gatt_notify_fanout_endpoint()
    )
}

fn latest_observed_tick(subscribers: &BTreeMap<NodeId, Tick>, receivers: &[NodeId]) -> Tick {
    receivers
        .iter()
        .filter_map(|receiver| subscribers.get(receiver))
        .copied()
        .max()
        .unwrap_or(Tick(0))
}
