use std::collections::HashMap;

use crate::{packet_features::PacketFeatures, Flow};
use chrono::{DateTime, TimeDelta, Utc};
use log::error;
use tokio::sync::mpsc;

const EXPIRATION_CHECK_INTERVAL: TimeDelta = chrono::Duration::seconds(60); // Check for expired flows every 60 seconds

pub struct FlowTable<T> {
    flow_map: HashMap<String, T>,  // HashMap for fast flow access by key
    active_timeout: u64,
    idle_timeout: u64,
    early_export: Option<u64>,
    export_channel: mpsc::Sender<T>,
    next_check_time: Option<DateTime<Utc>>, // Track the next time we check for flow expirations
}

impl<T> FlowTable<T>
where
    T: Flow,
{
    pub fn new(
        active_timeout: u64,
        idle_timeout: u64,
        early_export: Option<u64>,
        export_channel: mpsc::Sender<T>,
    ) -> Self {
        Self {
            flow_map: HashMap::new(),
            active_timeout,
            idle_timeout,
            early_export,
            export_channel,
            next_check_time: None,
        }
    }

    /// Processes a packet (either IPv4 or IPv6) and updates the flow map.
    pub async fn process_packet(
        &mut self,
        packet: &PacketFeatures,
    ) {
        // Check if enough virtual time has passed to trigger flow expiration checks
        if self.next_check_time.map_or(true, |next_check| packet.timestamp >= next_check) {
            self.export_expired_flows(packet.timestamp).await;
            
            // Set the next check time by adding the expiration interval to the current timestamp
            self.next_check_time = Some(packet.timestamp + EXPIRATION_CHECK_INTERVAL);
        }

        // Determine the flow direction and key
        let flow_key = if self.flow_map.contains_key(&packet.flow_key_bwd()) {
            packet.flow_key_bwd()
        } else {
            packet.flow_key()
        };

        // Check if the flow exists
        if let Some(flow) = self.flow_map.get_mut(&flow_key) {
            if flow.is_expired(packet.timestamp, self.active_timeout, self.idle_timeout) {
                // If expired, remove and export the flow
                let expired_flow = self.flow_map.remove(&flow_key).unwrap();
                self.export_flow(expired_flow).await;

                // Create a new flow for this packet
                let new_flow = T::new(
                    packet.flow_key(),
                    packet.source_ip,
                    packet.source_port,
                    packet.destination_ip,
                    packet.destination_port,
                    packet.protocol,
                    packet.timestamp,
                );
                self.flow_map.insert(packet.flow_key(), new_flow);
            } else {
                // Update the flow in forward or backward direction
                let is_forward = flow_key == packet.flow_key();
                let flow_terminated = flow.update_flow(&packet, is_forward);

                if flow_terminated {
                    // If terminated, remove and export the flow
                    if let Some(flow) = self.flow_map.remove(&flow_key) {
                        self.export_flow(flow).await;
                    }
                } else if let Some(early_export) = self.early_export {
                    // If flow duration is greater than early export, export the flow immediately (without deletion from the flow table)
                    if (packet.timestamp - flow.get_first_timestamp()).num_seconds() as u64 > early_export {
                        let flow_early_export = flow.clone();
                        self.export_flow(flow_early_export).await;
                    }
                }
            }
        } else {
            // If flow doesn't exist, create a new flow
            let new_flow = T::new(
                flow_key.clone(),
                packet.source_ip,
                packet.source_port,
                packet.destination_ip,
                packet.destination_port,
                packet.protocol,
                packet.timestamp,
            );
            self.flow_map.insert(flow_key.clone(), new_flow);
        }
    }

    pub async fn export_all_flows(&mut self) {
        // Export all flows in the flow map in order first packet arrival
        let mut flows_to_export: Vec<_> = self.flow_map
            .drain() // Drain all entries from the map
            .map(|(_, flow)| flow) // Collect all flows
            .collect();

        // Sort flows by `first_timestamp`
        flows_to_export.sort_by_key(|flow| flow.get_first_timestamp());

        // Export each flow in order of `first_timestamp`
        for flow in flows_to_export {
            self.export_flow(flow).await;
        }
    }

    pub async fn export_flow(&self, flow: T) {
        if let Err(e) = self.export_channel.send(flow).await {
            error!("Failed to send flow: {}", e);
        }
    }

    pub async fn export_expired_flows(&mut self, timestamp: DateTime<Utc>) {
        // Export all expired flows
        let expired_flows: Vec<_> = self.flow_map
            .iter()
            .filter_map(|(key, flow)| {
                if flow.is_expired(timestamp, self.active_timeout, self.idle_timeout) {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();

        for key in expired_flows {
            if let Some(flow) = self.flow_map.remove(&key) {
                self.export_flow(flow).await;
            }
        }
    }
}
