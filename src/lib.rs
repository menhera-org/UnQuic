//! Linux foundations for UnQuic's unencrypted Ethernet transport.
//!
//! This crate currently provides identity types, the v1 wire codec, IPv6 UDP
//! sockets and TAP ownership.
//! It does not yet establish sessions or forward Ethernet frames.
//! Callers supply configuration explicitly; the library reads no configuration
//! files or environment variables and writes no diagnostic output.
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#[cfg(not(target_os = "linux"))]
compile_error!("UnQuic requires the Linux kernel");

pub mod mac;
mod sha256;
pub mod socket;
mod sys;
pub mod tap;
pub mod uuid;
pub mod wire;

pub use mac::MacAddress;
pub use uuid::UnquicUuid;

/// Maximum Ethernet frame size, excluding FCS and TAP metadata.
pub const MAX_FRAME_LEN: usize = 9022;
/// UDP payload size at the IPv6 minimum MTU, without extension headers.
pub const BASE_UDP_PAYLOAD: usize = 1280 - 40 - 8;
