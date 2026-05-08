//! Outbound packet construction and `ToRadio` framing helpers.

use std::time::Duration;

use prost::Message as _;
use tracing::debug;

use crate::error::{Error, Result};
use crate::ports::{BROADCAST_ADDR, PRIVATE_APP, TEXT_MESSAGE_APP};
use crate::proto::{mesh_packet, to_radio, Data, MeshPacket, ToRadio};

use super::transport::Transport;
use super::types::rand_u32;
use super::MeshService;

impl MeshService {
    pub(super) async fn send_want_config(&self) -> Result<()> {
        let nonce: u32 = rand_u32();
        debug!(nonce, "sending want_config_id");
        self.send_to_radio(to_radio::PayloadVariant::WantConfigId(nonce))
            .await
    }

    /// Send a UTF-8 text message. `to` defaults to [`BROADCAST_ADDR`].
    ///
    /// `want_ack` is enabled only for direct messages; broadcasts are sent
    /// without ACK requests (the firmware would drop them anyway).
    pub async fn send_text(&self, text: &str, channel: u32, to: Option<u32>) -> Result<u32> {
        let id = self.next_id().await;
        let want_ack = to.is_some();
        let pkt = MeshPacket {
            from: 0,
            to: to.unwrap_or(BROADCAST_ADDR),
            channel,
            id,
            want_ack,
            hop_limit: 3,
            priority: mesh_packet::Priority::Default as i32,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                portnum: TEXT_MESSAGE_APP as i32,
                payload: text.as_bytes().to_vec(),
                ..Default::default()
            })),
            ..Default::default()
        };
        self.send_to_radio(to_radio::PayloadVariant::Packet(pkt))
            .await?;
        Ok(id)
    }

    /// Send a raw application data packet (e.g. voice chunks via [`PRIVATE_APP`]).
    pub async fn send_data(
        &self,
        portnum: i32,
        payload: Vec<u8>,
        channel: u32,
        to: Option<u32>,
        want_ack: bool,
    ) -> Result<u32> {
        let id = self.next_id().await;
        let pkt = MeshPacket {
            from: 0,
            to: to.unwrap_or(BROADCAST_ADDR),
            channel,
            id,
            want_ack,
            hop_limit: 3,
            priority: mesh_packet::Priority::Default as i32,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                portnum,
                payload,
                ..Default::default()
            })),
            ..Default::default()
        };
        self.send_to_radio(to_radio::PayloadVariant::Packet(pkt))
            .await?;
        Ok(id)
    }

    /// Send pre-chunked voice payloads, sleeping
    /// [`crate::voice::INTER_CHUNK_DELAY_MS`] between each.
    pub async fn send_voice_chunks(
        &self,
        chunks: Vec<Vec<u8>>,
        channel: u32,
        to: Option<u32>,
    ) -> Result<Vec<u32>> {
        let mut ids = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(crate::voice::INTER_CHUNK_DELAY_MS)).await;
            }
            ids.push(
                self.send_data(PRIVATE_APP as i32, chunk, channel, to, false)
                    .await?,
            );
        }
        Ok(ids)
    }

    async fn next_id(&self) -> u32 {
        let mut g = self.inner.next_packet_id.lock().await;
        let id = *g;
        *g = g.wrapping_add(1);
        if *g == 0 {
            *g = 1;
        }
        id
    }

    async fn send_to_radio(&self, payload: to_radio::PayloadVariant) -> Result<()> {
        let transport = {
            let slot = self.inner.transport.lock().await;
            match slot.as_ref() {
                Some(Transport::Ble(c)) => Transport::Ble(c.clone()),
                Some(Transport::Serial(c)) => Transport::Serial(c.clone()),
                None => return Err(Error::NotConnected),
            }
        };
        self.send_to_radio_via(&transport, payload).await
    }

    pub(super) async fn send_to_radio_via(
        &self,
        transport: &Transport,
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
