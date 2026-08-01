//! jdups — a readout for an APC Back-UPS over USB HID.
//!
//! The library half: device access, decoding, and the model the tray, the
//! sampler and (later) the agent all render from. See docs/implementation-plan.md.

pub mod decode;
pub mod hid;
pub mod model;
