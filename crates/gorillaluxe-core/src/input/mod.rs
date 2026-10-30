//! Input subsystem (Phase 3): virtio-input multi-touch protocol
//! model, Raw Input (`WM_INPUT`) host mouse capture, and the FPS
//! "Infinite Swipe" state machine (spec §3.2).

pub mod infinite_swipe;
pub mod raw;
pub mod selftest;
pub mod virtio_input;
