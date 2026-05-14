//! Decode `FromRadio` payloads and fan them out to typed observers.

use prost::Message as _;
use tracing::{debug, info, warn};

use crate::error::Result;
use crate::ids::node_num_to_id;
use crate::ports::MAX_TEXT_BYTES;
use crate::proto::{
    AdminMessage, FromRadio, MeshPacket, PortNum, admin_message, config, from_radio, mesh_packet,
};

use super::{ConnectionState, IncomingData, IncomingText, MeshService};

/// Latitude bounds in fixed-point 1e-7 degrees: [-90°, +90°].
const LAT_I_MIN: i32 = -900_000_000;
const LAT_I_MAX: i32 = 900_000_000;
/// Longitude bounds in fixed-point 1e-7 degrees: [-180°, +180°].
const LON_I_MIN: i32 = -1_800_000_000;
const LON_I_MAX: i32 = 1_800_000_000;

fn lat_i_in_range(v: i32) -> bool {
    (LAT_I_MIN..=LAT_I_MAX).contains(&v)
}
fn lon_i_in_range(v: i32) -> bool {
    (LON_I_MIN..=LON_I_MAX).contains(&v)
}

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
                let mut ni = ni;
                // Sanitise position fields against absurd values from a
                // misbehaving radio so downstream UI never sees garbage.
                if let Some(pos) = ni.position.as_mut() {
                    if let Some(lat) = pos.latitude_i
                        && !lat_i_in_range(lat)
                    {
                        warn!(node = ni.num, lat, "dropping out-of-range latitude_i");
                        pos.latitude_i = None;
                    }
                    if let Some(lon) = pos.longitude_i
                        && !lon_i_in_range(lon)
                    {
                        warn!(node = ni.num, lon, "dropping out-of-range longitude_i");
                        pos.longitude_i = None;
                    }
                }
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
            from_radio::PayloadVariant::QueueStatus(qs) => {
                // Firmware-driven backpressure. The radio publishes its
                // outbound queue depth on every accept/drain; surface it
                // to the voice TX worker so we never blast frames into a
                // full firmware queue (which is what causes the sender
                // device to OOM / watchdog-reboot under long voice
                // bursts on slow modem presets).
                debug!(
                    free = qs.free,
                    maxlen = qs.maxlen,
                    res = qs.res,
                    pkt = qs.mesh_packet_id,
                    "queue_status"
                );
                *self.inner.radio_queue_free.lock() = qs.free;
                // `notify_one` (not `notify_waiters`): if the voice TX
                // worker isn't currently parked on `.notified()` — the
                // common case during a frame-by-frame send burst — the
                // permit is stored and consumed by the next call,
                // closing the check-then-wait race that `notify_waiters`
                // had (which would silently drop pre-arrival notifies
                // and stall the worker for the full backpressure
                // timeout per frame).
                self.inner.radio_queue_notify.notify_one();
                let _ = self
                    .inner
                    .queue_status_tx
                    .send(crate::service::QueueStatusEvent {
                        res: qs.res,
                        free: qs.free,
                        maxlen: qs.maxlen,
                        mesh_packet_id: qs.mesh_packet_id,
                    });
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
        let mut payload = data.payload.clone();
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
        if portnum == PortNum::TextMessageApp as i32 {
            if payload.len() > MAX_TEXT_BYTES {
                warn!(
                    from = pkt.from,
                    len = payload.len(),
                    "dropping oversized text payload"
                );
                return;
            }
            match String::from_utf8(payload) {
                Ok(text) => {
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
                Err(e) => {
                    warn!(
                        from = pkt.from,
                        len = e.as_bytes().len(),
                        "malformed UTF-8 on TextMessageApp; falling through to data fan-out"
                    );
                    payload = e.into_bytes();
                }
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
