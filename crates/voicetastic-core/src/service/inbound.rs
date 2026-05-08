//! Decode `FromRadio` payloads and fan them out to typed observers.

use prost::Message as _;
use tracing::{debug, info};

use crate::error::Result;
use crate::ids::node_num_to_id;
use crate::proto::{
    AdminMessage, FromRadio, MeshPacket, PortNum, admin_message, config, from_radio, mesh_packet,
};

use super::{ConnectionState, IncomingData, IncomingText, MeshService};

impl MeshService {
    pub(super) async fn handle_from_radio(&self, bytes: &[u8]) -> Result<()> {
        let msg = FromRadio::decode(bytes)?;
        let Some(variant) = msg.payload_variant else {
            return Ok(());
        };
        match variant {
            from_radio::PayloadVariant::MyInfo(info) => {
                debug!(my_node_num = info.my_node_num, "MyNodeInfo");
                let _ = self.inner.my_info_tx.send(Some(info));
            }
            from_radio::PayloadVariant::NodeInfo(ni) => {
                // If this is our own node, surface the User as the "owner".
                let my_num = self
                    .inner
                    .my_info_tx
                    .borrow()
                    .as_ref()
                    .map(|i| i.my_node_num);
                if Some(ni.num) == my_num
                    && let Some(user) = ni.user.as_ref()
                {
                    let _ = self.inner.owner_tx.send(Some(user.clone()));
                }
                let mut nodes = self.inner.nodes_tx.borrow().clone();
                nodes.insert(ni.num, ni);
                let _ = self.inner.nodes_tx.send(nodes);
            }
            from_radio::PayloadVariant::Config(cfg) => {
                if let Some(v) = cfg.payload_variant {
                    match v {
                        config::PayloadVariant::Lora(c) => {
                            let _ = self.inner.lora_tx.send(Some(c));
                        }
                        config::PayloadVariant::Device(c) => {
                            let _ = self.inner.device_tx.send(Some(c));
                        }
                        config::PayloadVariant::Position(c) => {
                            let _ = self.inner.position_tx.send(Some(c));
                        }
                        config::PayloadVariant::Power(c) => {
                            let _ = self.inner.power_tx.send(Some(c));
                        }
                        config::PayloadVariant::Network(c) => {
                            let _ = self.inner.network_tx.send(Some(c));
                        }
                        config::PayloadVariant::Display(c) => {
                            let _ = self.inner.display_tx.send(Some(c));
                        }
                        config::PayloadVariant::Bluetooth(c) => {
                            let _ = self.inner.bluetooth_tx.send(Some(c));
                        }
                        _ => {}
                    }
                }
            }
            from_radio::PayloadVariant::Channel(ch) => {
                let mut chans = self.inner.channels_tx.borrow().clone();
                if let Some(slot) = chans.iter_mut().find(|c| c.index == ch.index) {
                    *slot = ch;
                } else {
                    chans.push(ch);
                    chans.sort_by_key(|c| c.index);
                }
                let _ = self.inner.channels_tx.send(chans);
            }
            from_radio::PayloadVariant::Metadata(meta) => {
                let _ = self.inner.metadata_tx.send(Some(meta));
            }
            from_radio::PayloadVariant::ConfigCompleteId(nonce) => {
                info!(nonce, "config_complete");
                self.set_state(ConnectionState::Ready);
                let _ = self.inner.config_complete_tx.send(nonce);
            }
            from_radio::PayloadVariant::Packet(pkt) => {
                self.handle_packet(pkt);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_packet(&self, pkt: MeshPacket) {
        let Some(mesh_packet::PayloadVariant::Decoded(data)) = pkt.payload_variant.as_ref() else {
            return;
        };
        let portnum = data.portnum;
        let payload = data.payload.clone();
        // Admin responses (e.g. get_owner_response) come back as a packet on
        // ADMIN_APP. Decode them so the settings UI sees the latest values.
        if portnum == PortNum::AdminApp as i32
            && let Ok(admin) = AdminMessage::decode(payload.as_slice())
            && let Some(v) = admin.payload_variant
        {
            match v {
                admin_message::PayloadVariant::GetOwnerResponse(user) => {
                    let _ = self.inner.owner_tx.send(Some(user));
                }
                admin_message::PayloadVariant::GetChannelResponse(ch) => {
                    let mut chans = self.inner.channels_tx.borrow().clone();
                    if let Some(slot) = chans.iter_mut().find(|c| c.index == ch.index) {
                        *slot = ch;
                    } else {
                        chans.push(ch);
                        chans.sort_by_key(|c| c.index);
                    }
                    let _ = self.inner.channels_tx.send(chans);
                }
                admin_message::PayloadVariant::GetDeviceMetadataResponse(meta) => {
                    let _ = self.inner.metadata_tx.send(Some(meta));
                }
                _ => {}
            }
            return;
        }
        if portnum == PortNum::TextMessageApp as i32
            && let Ok(text) = String::from_utf8(payload.clone())
        {
            let from_id = node_num_to_id(pkt.from);
            let _ = self.inner.incoming_text_tx.send(IncomingText {
                from: pkt.from,
                from_id,
                to: pkt.to,
                channel: pkt.channel,
                text,
                rx_time: pkt.rx_time,
                rx_snr: pkt.rx_snr,
                rx_rssi: pkt.rx_rssi,
            });
            return;
        }
        let _ = self.inner.incoming_data_tx.send(IncomingData {
            from: pkt.from,
            to: pkt.to,
            channel: pkt.channel,
            portnum,
            payload,
            rx_time: pkt.rx_time,
        });
    }
}
