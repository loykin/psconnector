//! Talking to the console: status and wake-up (DDP, UDP 9302), session setup and the control channel
//! (TCP 9295), and the Takion packets and protobuf messages the stream (UDP 9296) is made of.

pub mod ddp;
pub(crate) mod proto;
pub(crate) mod session;
pub(crate) mod takion;
