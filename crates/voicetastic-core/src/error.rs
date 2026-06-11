use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// btleplug-specific BLE failure. Only emitted by the built-in BLE
    /// transport (`ble-btleplug` feature). Downstream `Transport`
    /// implementations report their own failures via [`Error::Other`].
    #[cfg(feature = "ble-btleplug")]
    #[error("BLE error: {0}")]
    Ble(#[from] btleplug::Error),

    #[error("protobuf decode error: {0}")]
    ProtoDecode(#[from] prost::DecodeError),

    #[error("protobuf encode error: {0}")]
    ProtoEncode(#[from] prost::EncodeError),

    #[error("not connected to a Meshtastic node")]
    NotConnected,

    #[error("local node info not yet received (my_node_num is 0)")]
    NoLocalNode,

    #[error("required Meshtastic GATT characteristic not found: {0}")]
    MissingCharacteristic(&'static str),

    #[error("BLE write timed out")]
    WriteTimeout,

    #[error("payload too large: {len} bytes exceeds transport limit of {max} bytes")]
    PayloadTooLarge { len: usize, max: usize },

    #[error("voice protocol error: {0}")]
    Voice(#[from] crate::voice::VoiceError),

    #[error("invalid node id: {0}")]
    InvalidNodeId(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}
