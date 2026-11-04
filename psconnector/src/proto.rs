//! Takion control messages (protobuf, only the needed parts of the proto2 `tkproto` package).
//!
//! Defined directly with prost derive macros, no protoc needed. Field numbers match pyremoteplay's takion_pb2.

use prost::Message;

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum PayloadType {
    Big = 0,
    Bang = 1,
    Info = 2,
    Heartbeat = 3,
    PacketLoss = 4,
    CorruptFrame = 5,
    Disconnect = 8,
    Senkusha = 12,
    StreamInfo = 13,
    StreamInfoAck = 14,
    ControllerConnection = 21,
    IdrRequest = 25,
}

#[derive(Clone, PartialEq, Message)]
pub struct TakionMessage {
    #[prost(enumeration = "PayloadType", required, tag = "1")]
    pub r#type: i32,
    #[prost(message, optional, tag = "2")]
    pub big_payload: Option<BigPayload>,
    #[prost(message, optional, tag = "3")]
    pub bang_payload: Option<BangPayload>,
    #[prost(message, optional, tag = "6")]
    pub corrupt_payload: Option<CorruptFramePayload>,
    #[prost(message, optional, tag = "10")]
    pub disconnect_payload: Option<DisconnectPayload>,
    #[prost(message, optional, tag = "15")]
    pub stream_info_payload: Option<StreamInfoPayload>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BigPayload {
    #[prost(uint32, required, tag = "1")]
    pub client_version: u32,
    #[prost(string, required, tag = "2")]
    pub session_key: String,
    #[prost(string, required, tag = "3")]
    pub launch_spec: String,
    #[prost(bytes = "vec", required, tag = "4")]
    pub encrypted_key: Vec<u8>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub ecdh_pub_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "6")]
    pub ecdh_sig: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BangPayload {
    #[prost(uint32, optional, tag = "1")]
    pub server_version: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub token: Option<u32>,
    #[prost(bool, optional, tag = "3")]
    pub encrypted_key_accepted: Option<bool>,
    #[prost(bool, optional, tag = "4")]
    pub version_accepted: Option<bool>,
    #[prost(string, optional, tag = "5")]
    pub session_key: Option<String>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub ecdh_pub_key: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "9")]
    pub ecdh_sig: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CorruptFramePayload {
    #[prost(uint32, required, tag = "1")]
    pub start: u32,
    #[prost(uint32, required, tag = "2")]
    pub end: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct DisconnectPayload {
    #[prost(string, optional, tag = "1")]
    pub reason: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ResolutionPayload {
    #[prost(uint32, optional, tag = "1")]
    pub width: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub height: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub video_header: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StreamInfoPayload {
    #[prost(message, repeated, tag = "1")]
    pub resolution: Vec<ResolutionPayload>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub audio_header: Option<Vec<u8>>,
}

pub fn simple(t: PayloadType) -> Vec<u8> {
    TakionMessage { r#type: t as i32, ..Default::default() }.encode_to_vec()
}

pub fn big(session_key: String, launch_spec: String, pub_key: Vec<u8>, sig: Vec<u8>) -> Vec<u8> {
    TakionMessage {
        r#type: PayloadType::Big as i32,
        big_payload: Some(BigPayload {
            client_version: 12, // PS5
            session_key,
            launch_spec,
            encrypted_key: vec![0; 4],
            ecdh_pub_key: Some(pub_key),
            ecdh_sig: Some(sig),
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

pub fn disconnect() -> Vec<u8> {
    TakionMessage {
        r#type: PayloadType::Disconnect as i32,
        disconnect_payload: Some(DisconnectPayload { reason: Some("Client Disconnecting".into()) }),
        ..Default::default()
    }
    .encode_to_vec()
}

/// Reports lost frames. The console then sends a new key frame.
pub fn corrupt(start: u32, end: u32) -> Vec<u8> {
    TakionMessage {
        r#type: PayloadType::CorruptFrame as i32,
        corrupt_payload: Some(CorruptFramePayload { start, end }),
        ..Default::default()
    }
    .encode_to_vec()
}
