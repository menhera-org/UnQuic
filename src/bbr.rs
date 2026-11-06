//! BBRv3 components pinned to draft-ietf-ccwg-bbr-07.
//!
//! Only the pure delivery sampler is implemented. The controller state machine
//! and runtime send/report integration remain unfinished.
mod delivery;
pub use delivery::{Delivery, DeliveryError, DeliveryPacket, RateSample};
