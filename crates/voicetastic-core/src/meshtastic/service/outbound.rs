//! Outbound packet construction and `ToRadio` framing helpers.

use std::sync::atomic::Ordering;
use std::time::Duration;

use prost::Message as _;
use tracing::debug;

use crate::error::{Error, Result};
use crate::meshtastic::ack::{AckHandle, AckResult};
use crate::proto::{
    Channel, Config, ModuleConfig, Position, ToRadio, User, admin_message, config, module_config,
    to_radio,
};

use super::types::rand_u32;
use super::{MeshtasticService, protocol};
use crate::transport::Transport;

impl MeshtasticService {
    pub(super) async fn send_want_config(&self) -> Result<()> {
        let nonce: u32 = rand_u32()?;
        debug!(nonce, "sending want_config_id");
        self.send_to_radio(protocol::want_config(nonce)).await
    }

    /// Send a UTF-8 text message. `to` defaults to [`BROADCAST_ADDR`].
    ///
    /// `want_ack` is enabled only for direct messages; broadcasts are sent
    /// without ACK requests (the firmware would drop them anyway).
    pub async fn send_text(&self, text: &str, channel: u32, to: Option<u32>) -> Result<u32> {
        // The oversized-payload check (firmware rejects them) lives in
        // `protocol::text_packet`, so the limit is enforced in one place.
        let id = self.next_id();
        self.send_to_radio(protocol::text_packet(id, text, channel, to)?)
            .await?;
        Ok(id)
    }

    /// Send a UTF-8 text DM and return an [`AckHandle`] that resolves
    /// once the firmware reports delivery status (typically within a
    /// few hundred ms over BLE plus the mesh's flood-route latency).
    /// The ack slot is registered before the packet leaves the host, so
    /// no race window where the response could arrive before we're
    /// listening. Use the plain [`Self::send_text`] for broadcasts —
    /// the firmware does not ack them.
    pub async fn send_text_tracked(
        &self,
        text: &str,
        channel: u32,
        to: u32,
    ) -> Result<(u32, AckHandle)> {
        let id = self.next_id();
        let handle = self.register_ack(id);
        // Build the packet before sending so a `text_packet` validation
        // failure doesn't leave a registered ack slot behind.
        let payload = match protocol::text_packet(id, text, channel, Some(to)) {
            Ok(p) => p,
            Err(e) => {
                self.discard_ack(id);
                return Err(e);
            }
        };
        if let Err(e) = self.send_to_radio(payload).await {
            self.discard_ack(id);
            return Err(e);
        }
        Ok((id, handle))
    }

    /// Send a raw application data packet (e.g. voice chunks via [`PRIVATE_APP`]).
    ///
    /// `want_ack` is on the transport layer (next-hop ACK). `want_response`
    /// is on the application Data field — set it to ask the destination app
    /// to reply in kind (e.g. NodeInfo discovery: a broadcast NodeInfo with
    /// `want_response = true` prompts peers to send back their own NodeInfo).
    pub async fn send_data(
        &self,
        portnum: i32,
        payload: Vec<u8>,
        channel: u32,
        to: Option<u32>,
        want_ack: bool,
        want_response: bool,
    ) -> Result<u32> {
        let id = self.next_id();
        self.send_to_radio(protocol::data_packet(
            id,
            portnum,
            payload,
            channel,
            to,
            want_ack,
            want_response,
        ))
        .await?;
        Ok(id)
    }

    /// Send a voice message.
    ///
    /// The caller pre-encodes the audio into wire frames via
    /// [`crate::voice::build_message`] and supplies the resulting
    /// [`crate::voice::EncodedMessage`] together with a `pacing` delay
    /// derived from the current LoRa modem preset (see
    /// [`crate::voice::ModemPreset::pacing`]).
    ///
    /// Frames are pushed onto the shared voice TX queue (see
    /// [`super::voice_tx`]) so concurrent voice messages — including
    /// NACK-driven retransmits — are serialized and paced consistently.
    /// We wait for each frame to actually leave the worker before
    /// enqueuing the next: this preserves the original semantics of
    /// "returns after the burst is on its way" and yields the assigned
    /// packet ids in order.
    pub async fn send_voice(
        &self,
        message: &crate::voice::EncodedMessage,
        channel: u32,
        to: Option<u32>,
        pacing: Duration,
    ) -> Result<Vec<u32>> {
        let want_ack = to.is_some();
        let mut ids = Vec::with_capacity(message.frames.len());
        for frame in &message.frames {
            let id = self
                .enqueue_voice_frame_with_id(frame.clone(), channel, to, want_ack, pacing)
                .await?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Register a `oneshot` slot for the firmware's delivery ack on
    /// `packet_id` and return the receiver wrapped in an [`AckHandle`].
    /// Sweeps any orphaned entries (handle dropped without resolving)
    /// while the lock is held so the table doesn't grow unboundedly
    /// across long-running listeners that ignore the handle.
    fn register_ack(&self, packet_id: u32) -> AckHandle {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut acks = self.inner.pending_acks.lock();
        acks.retain(|_, sender| !sender.is_closed());
        acks.insert(packet_id, tx);
        AckHandle::new(packet_id, rx)
    }

    /// Tear down a registered ack slot without resolving it. Used when
    /// the corresponding send failed before the packet went on the wire
    /// — the firmware won't ack a packet it never saw, so leaving the
    /// slot in place would just be a leak.
    fn discard_ack(&self, packet_id: u32) {
        self.inner.pending_acks.lock().remove(&packet_id);
    }

    /// Resolve a registered ack slot. No-op if no slot exists (we
    /// received a `Routing` packet for a packet we didn't track, or the
    /// handle was already dropped).
    pub(super) fn signal_ack(&self, packet_id: u32, result: AckResult) {
        if let Some(tx) = self.inner.pending_acks.lock().remove(&packet_id) {
            let _ = tx.send(result);
        }
    }

    fn next_id(&self) -> u32 {
        // Atomically reserve the next id. `fetch_add` returns the
        // previous value; if a wrap-around produced `0` we retry so the
        // reserved value is `1` instead. Two concurrent callers landing
        // on the wrap each retry independently and then take consecutive
        // post-wrap ids, so no caller ever observes `0`.
        loop {
            let id = self.inner.next_packet_id.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// Send an [`AdminMessage`] payload to the local node on
    /// [`ADMIN_APP`]. `to=` defaults to our own node number, which is the
    /// only correct destination for config writes; if `my_node_num` is not
    /// yet known the call returns [`Error::NotConnected`].
    ///
    /// All current callers are setters (`SetConfig`, `SetOwner`, …) which
    /// don't need a response payload — `want_ack` already gives us
    /// delivery confirmation. Asking for a response here would prompt the
    /// firmware to echo every config write back as a fresh state push,
    /// which the inbound handler would then mistake for a server-initiated
    /// update.
    pub async fn send_admin(&self, payload: admin_message::PayloadVariant) -> Result<u32> {
        let to = self.my_node_num().ok_or(Error::NotConnected)?;
        let id = self.next_id();
        self.send_to_radio(protocol::admin_packet(id, to, payload)?)
            .await?;
        Ok(id)
    }

    /// Write a [`Config`] section (LoRa, Device, …) to the local node.
    pub async fn write_config(&self, cfg: config::PayloadVariant) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::SetConfig(Config {
            payload_variant: Some(cfg),
        }))
        .await
    }

    /// Write a [`ModuleConfig`] section (MQTT, Telemetry, …) to the local
    /// node. Mirrors [`Self::write_config`] but targets the parallel
    /// module-config admin path the firmware exposes.
    pub async fn write_module_config(
        &self,
        cfg: module_config::PayloadVariant,
    ) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::SetModuleConfig(ModuleConfig {
            payload_variant: Some(cfg),
        }))
        .await
    }

    /// Update the device owner / user record.
    pub async fn write_owner(&self, user: User) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::SetOwner(user))
            .await
    }

    /// Write a single channel definition.
    pub async fn write_channel(&self, channel: Channel) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::SetChannel(channel))
            .await
    }

    /// Set the device's manually-fixed location. The firmware also flips
    /// `position.fixed_position = true` as a side effect.
    pub async fn set_fixed_position(&self, position: Position) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::SetFixedPosition(position))
            .await
    }

    /// Send a one-shot `Position` packet on the mesh (port
    /// `POSITION_APP`). `to == None` broadcasts; otherwise the packet is
    /// addressed to that node. This is the "share my location now" path
    /// distinct from the firmware's own scheduled broadcasts and
    /// distinct from [`Self::set_fixed_position`] (which writes a config
    /// admin message to the local radio, not a mesh packet).
    pub async fn broadcast_position(
        &self,
        position: Position,
        channel: u32,
        to: Option<u32>,
    ) -> Result<u32> {
        let mut buf = Vec::with_capacity(position.encoded_len());
        position.encode(&mut buf)?;
        self.send_data(
            crate::ports::POSITION_APP as i32,
            buf,
            channel,
            to,
            /* want_ack: */ false,
            /* want_response: */ false,
        )
        .await
    }

    /// Clear the manually-fixed location and flip
    /// `position.fixed_position = false`.
    pub async fn remove_fixed_position(&self) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::RemoveFixedPosition(true))
            .await
    }

    /// Schedule a reboot in `secs` seconds.
    pub async fn reboot(&self, secs: i32) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::RebootSeconds(secs))
            .await
    }

    /// Factory-reset the device's configuration (preserves BLE bonds).
    pub async fn factory_reset(&self) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::FactoryResetConfig(1))
            .await
    }

    /// Wipe the local node's NodeDB.
    ///
    /// Targets the radio this service is connected to over BLE / serial.
    /// Removes every learned `(node_num, NodeInfo)` entry it has accumulated
    /// from the mesh (positions, names, last-heard timestamps, public keys).
    /// Useful during testing when peer nodes have been re-flashed or
    /// re-named and their stale entries linger on this radio.
    ///
    /// Does NOT touch other nodes' NodeDBs: each radio has to be told
    /// individually, either over a direct link or via remote-admin DM
    /// (requires the admin key configured).
    pub async fn reset_nodedb(&self) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::NodedbReset(true))
            .await
    }

    /// Remove a single entry from the local node's NodeDB.
    ///
    /// Targets the radio this service is connected to. Use [`Self::reset_nodedb`]
    /// to wipe everything instead. No-op on the radio if `node_num` is not
    /// in its NodeDB; the firmware still acks the admin packet.
    pub async fn remove_node(&self, node_num: u32) -> Result<u32> {
        self.send_admin(admin_message::PayloadVariant::RemoveByNodenum(node_num))
            .await
    }

    async fn send_to_radio(&self, payload: to_radio::PayloadVariant) -> Result<()> {
        let transport = {
            let slot = self.inner.transport.lock().await;
            match slot.as_ref() {
                Some(t) => t.clone(),
                None => return Err(Error::NotConnected),
            }
        };
        self.send_to_radio_via(transport.as_ref(), payload).await
    }

    pub(super) async fn send_to_radio_via(
        &self,
        transport: &dyn Transport,
        payload: to_radio::PayloadVariant,
    ) -> Result<()> {
        let msg = ToRadio {
            payload_variant: Some(payload),
        };
        let mut buf = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut buf)?;
        transport.write_to_radio(&buf).await
    }
}
