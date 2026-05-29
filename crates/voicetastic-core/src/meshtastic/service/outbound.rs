//! Outbound packet construction and `ToRadio` framing helpers.

use std::sync::atomic::Ordering;
use std::time::Duration;

use prost::Message as _;
use tracing::debug;

use crate::error::{Error, Result};
use crate::proto::{Channel, Config, Position, ToRadio, User, admin_message, config, to_radio};

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
