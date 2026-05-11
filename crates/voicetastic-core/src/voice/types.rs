//! Enums and small wire-shape types used across the protocol.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::consts::MAX_BODY_SIZE;

/// `packet_type` (top 2 bits of `type_flags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketType {
    /// Original codec frame data for `chunk_index ∈ [0, total_data)`.
    Data = 0,
    /// Reed-Solomon parity for `chunk_index ∈ [0, parity_count)`.
    Parity = 1,
    /// Selective-retransmit request — bitmap of missing data chunks.
    Nack = 2,
}

impl PacketType {
    pub(super) fn from_bits(b: u8) -> Option<Self> {
        Some(match (b & 0xC0) >> 6 {
            0 => Self::Data,
            1 => Self::Parity,
            2 => Self::Nack,
            _ => return None,
        })
    }

    pub(super) fn to_bits(self) -> u8 {
        (self as u8) << 6
    }
}

/// Codec advertised in the chunk header. The protocol does not transcode;
/// receivers that don't speak the codec drop the frame and surface
/// "codec unsupported" upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoiceCodec {
    AmrNb,
    Opus,
    PcmS16Le,
    Unknown(u8),
}

impl VoiceCodec {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::AmrNb,
            1 => Self::Opus,
            2 => Self::PcmS16Le,
            _ => Self::Unknown(b),
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Self::AmrNb => 0,
            Self::Opus => 1,
            Self::PcmS16Le => 2,
            Self::Unknown(b) => b,
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

/// Destination of a voice message: a specific node or the channel broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoiceDestination {
    Node(u32),
    Broadcast,
}

/// Meshtastic LoRa modem presets, used to pick adaptive pacing and a sane
/// chunk size. Mirrors the firmware enum order; receivers only read; senders
/// look up [`Self::pacing`] and [`Self::recommended_chunk_size`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModemPreset {
    ShortTurbo,
    ShortFast,
    ShortSlow,
    MediumFast,
    MediumSlow,
    LongFast,
    LongModerate,
    LongSlow,
    VeryLongSlow,
}

impl ModemPreset {
    /// Recommended inter-frame delay for adaptive pacing (see spec §2.1).
    ///
    /// Values are tuned so that `pacing >= air_time(recommended_chunk_size)`
    /// for each preset (with ~30 % headroom for firmware CSMA, queue
    /// drain, and ACK windows). Sending faster than this overruns the
    /// firmware's small outbound queue and causes frames to be dropped
    /// before they ever hit the air — which is exactly the failure mode
    /// the receiver-side reassembler then has to chase with NACKs.
    pub fn pacing(self) -> Duration {
        Duration::from_millis(match self {
            Self::ShortTurbo => 150,
            Self::ShortFast => 250,
            Self::ShortSlow => 400,
            Self::MediumFast => 500,
            Self::MediumSlow => 700,
            Self::LongFast => 900,
            Self::LongModerate => 1200,
            Self::LongSlow => 1800,
            Self::VeryLongSlow => 3000,
        })
    }

    /// Recommended `chunk_size` per modem preset (see spec §4).
    pub fn recommended_chunk_size(self) -> usize {
        match self {
            Self::ShortTurbo | Self::ShortFast => MAX_BODY_SIZE, // 219
            Self::ShortSlow | Self::MediumFast => 160,
            Self::MediumSlow | Self::LongFast => 96,
            Self::LongModerate | Self::LongSlow | Self::VeryLongSlow => 48,
        }
    }

    /// Default fallback when the radio's preset is unknown.
    pub fn fallback_pacing() -> Duration {
        Duration::from_millis(500)
    }

    /// Map the firmware's `LoRaConfig.modem_preset` enum value (i32) to
    /// our local enum. Returns `None` for unknown values so callers can
    /// fall back to safe defaults.
    ///
    /// Mirrors `meshtastic.Config.LoRaConfig.ModemPreset` integer values.
    pub fn from_proto(value: i32) -> Option<Self> {
        // SAFETY: numeric values come straight from the .proto enum.
        // Keep this match in sync with `proto/meshtastic/config.proto`.
        Some(match value {
            0 => Self::LongFast,
            1 => Self::LongSlow,
            2 => Self::VeryLongSlow,
            3 => Self::MediumSlow,
            4 => Self::MediumFast,
            5 => Self::ShortSlow,
            6 => Self::ShortFast,
            7 => Self::LongModerate,
            8 => Self::ShortTurbo,
            _ => return None,
        })
    }
}
