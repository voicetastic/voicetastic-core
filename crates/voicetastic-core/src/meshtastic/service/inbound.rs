//! Native driver for the inbound path: run the sans-IO decoder
//! ([`super::protocol::decode_inbound`]) and apply the resulting
//! [`InboundEvent`]s to this service's watch/broadcast channels.
//!
//! All Meshtastic decode/interpret logic lives in [`super::protocol`]; this
//! file is just the tokio-flavoured glue that publishes the decoded effects.

use tracing::{info, warn};

use crate::error::Result;
use crate::proto::{config, module_config};

use super::protocol::{InboundCtx, InboundEvent, decode_inbound};
use super::{ConnectionState, MeshtasticService};

impl MeshtasticService {
    pub(super) async fn handle_from_radio(&self, bytes: &[u8]) -> Result<()> {
        // Snapshot the bits of canonical state the decoder needs. Held
        // briefly across `decode_inbound` (which is sync / sans-IO and
        // never awaits); released before `apply_inbound`, which itself
        // re-acquires the same lock to mutate.
        //
        // We read `my_node_num` from the locked snapshot rather than
        // calling `self.my_node_num()` — that helper takes the same
        // mutex, and `parking_lot::Mutex` is not reentrant (would
        // deadlock silently).
        let events = {
            let state = self.inner.state.lock();
            let ctx = InboundCtx {
                my_node_num: state.my_info.as_ref().map(|i| i.my_node_num),
                our_private_key: state.our_private_key(),
                nodes: &state.nodes,
            };
            decode_inbound(bytes, &ctx)?
        };
        for event in events {
            self.apply_inbound(event);
        }
        Ok(())
    }

    /// Apply one decoded inbound effect. Snapshot events update the canonical
    /// [`ProtocolState`] and are mirrored to the matching watch channel (the
    /// subscriber API); transient messages and queue status go straight to
    /// their broadcast / notify channels. No decoding here (see
    /// [`super::protocol`]).
    fn apply_inbound(&self, event: InboundEvent) {
        if event.is_snapshot() {
            // Canonical state is the single implementation of the update; the
            // watch channel is just a mirror so subscribers (GUI/CLI) and the
            // existing API still observe the value.
            let mut state = self.inner.state.lock();
            state.apply(&event);
            match event {
                InboundEvent::MyInfo(_) => {
                    let _ = self.inner.my_info_tx.send(state.my_info.clone());
                }
                InboundEvent::NodeInfo(_) => {
                    let _ = self.inner.nodes_tx.send(state.nodes.clone());
                }
                InboundEvent::Owner(_) => {
                    let _ = self.inner.owner_tx.send(state.owner.clone());
                }
                InboundEvent::Config(v) => match v {
                    config::PayloadVariant::Lora(_) => {
                        let _ = self.inner.lora_tx.send(state.lora.clone());
                    }
                    config::PayloadVariant::Device(_) => {
                        let _ = self.inner.device_tx.send(state.device.clone());
                    }
                    // Position/Power/Display/Bluetooth configs are Copy.
                    config::PayloadVariant::Position(_) => {
                        let _ = self.inner.position_tx.send(state.position);
                    }
                    config::PayloadVariant::Power(_) => {
                        let _ = self.inner.power_tx.send(state.power);
                    }
                    config::PayloadVariant::Network(_) => {
                        let _ = self.inner.network_tx.send(state.network.clone());
                    }
                    config::PayloadVariant::Display(_) => {
                        let _ = self.inner.display_tx.send(state.display);
                    }
                    config::PayloadVariant::Bluetooth(_) => {
                        let _ = self.inner.bluetooth_tx.send(state.bluetooth);
                    }
                    _ => {}
                },
                InboundEvent::ModuleConfig(v) => match v {
                    module_config::PayloadVariant::Mqtt(_) => {
                        let _ = self.inner.mqtt_tx.send(state.mqtt.clone());
                    }
                    _ => {}
                },
                InboundEvent::Channel(_) => {
                    let _ = self.inner.channels_tx.send(state.channels.clone());
                }
                InboundEvent::Metadata(_) => {
                    let _ = self.inner.metadata_tx.send(state.metadata.clone());
                }
                // is_snapshot() guarantees only the above arms reach here.
                _ => unreachable!("non-snapshot event in snapshot branch"),
            }
            return;
        }

        // Transient / non-snapshot effects.
        match event {
            InboundEvent::ConfigComplete(nonce) => {
                let state = self.inner.state.lock();
                let (lora, device) = (state.lora.is_some(), state.device.is_some());
                drop(state);
                if !lora || !device {
                    warn!(
                        nonce,
                        lora, device, "config_complete received with incomplete config burst"
                    );
                }
                info!(nonce, "config_complete");
                self.set_state(ConnectionState::Ready);
                let _ = self.inner.config_complete_tx.send(nonce);
            }
            InboundEvent::IncomingText(text) => {
                let _ = self.inner.incoming_text_tx.send(text);
            }
            InboundEvent::IncomingData(data) => {
                if self.inner.incoming_data_tx.receiver_count() > 0 {
                    let _ = self.inner.incoming_data_tx.send(data);
                }
            }
            InboundEvent::Voice(voice) => {
                if self.inner.voice_data_tx.receiver_count() > 0 {
                    let _ = self.inner.voice_data_tx.send(voice);
                }
            }
            InboundEvent::QueueStatus(qs) => {
                *self.inner.radio_queue_free.lock() = qs.free;
                // `notify_one` (not `notify_waiters`): stores a permit if the
                // voice TX worker isn't currently parked, closing the
                // check-then-wait race. See voice_tx.rs for the full rationale.
                self.inner.radio_queue_notify.notify_one();
                let _ = self.inner.queue_status_tx.send(qs);
            }
            InboundEvent::AckOrNak { request_id, result } => {
                // Broadcast first so subscribers (Android Kotlin
                // bindings, future delivery-icon UI) get every event,
                // not just the ones with an explicit oneshot waiter.
                let _ = self.inner.ack_event_tx.send((request_id, result));
                self.signal_ack(request_id, result);
            }
            // is_snapshot() routed these above.
            _ => unreachable!("snapshot event in transient branch"),
        }
    }
}
