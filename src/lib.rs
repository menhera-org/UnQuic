//! Linux foundations for UnQuic's unencrypted Ethernet transport.
//!
//! This crate provides identity types, the v1 wire codec, a pure handshake state
//! machine, IPv6 UDP sockets, TAP ownership and a worker runtime for configured
//! IPv6 peers and confirmed address updates. Recovery and congestion control remain unfinished.
//! Callers supply configuration explicitly; the library reads no configuration
//! files or environment variables and writes no diagnostic output.
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#[cfg(not(target_os = "linux"))]
compile_error!("UnQuic requires the Linux kernel");

#[cfg(test)]
extern crate self as unquic;

mod ack;
pub mod config;
pub mod counters;
#[cfg(feature = "dns")]
pub mod dns;
pub mod handshake;
pub mod mac;
pub mod pack;
pub mod reassembly;
pub mod runtime;
mod sha256;
pub mod socket;
pub mod spsc;
mod sys;
pub mod tap;
pub mod uuid;
pub mod wire;
mod worker;

pub use mac::MacAddress;
pub use uuid::UnquicUuid;

/// Maximum Ethernet frame size, excluding FCS and TAP metadata.
pub const MAX_FRAME_LEN: usize = 9022;
/// UDP payload size at the IPv6 minimum MTU, without extension headers.
pub const BASE_UDP_PAYLOAD: usize = 1280 - 40 - 8;
