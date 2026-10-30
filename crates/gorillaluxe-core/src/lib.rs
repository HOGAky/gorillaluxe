//! GorillaLuxe core library.
//!
//! Phase 1: WHPX VMM foundation, guest smoke payload, RU/EN i18n, config.
//! Later phases: virtio devices, DX12 zero-copy compositor, CAS pass.

pub mod audio;
pub mod config;
pub mod gfx;
pub mod guest;
pub mod i18n;
pub mod input;
pub mod system;
pub mod ui;
pub mod vmm;
pub mod whpx;

pub const APP_NAME: &str = "GorillaLuxe";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
