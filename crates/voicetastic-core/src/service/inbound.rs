//! Decode `FromRadio` payloads and fan them out to typed observers.

use prost::Message as _;
use tracing::{debug, info};

use crate::error::Result;
use crate::ids::node_num_to_id;
use crate::proto::{from_radio, mesh_packet, FromRadio, MeshPacket, PortNum};

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
                let mut nodes = self.inner.nodes_tx.borrow().clone();
                nodes.insert(ni.num, ni);
                let _ = self.inner.nodes_tx.send(nodes);
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
        if portnum == PortNum::TextMessageApp as i32 {
            if let Ok(text) = String::from_utf8(payload.clone()) {
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
