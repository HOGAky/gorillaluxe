//! Audio subsystem (Phase 3): virtio-snd backend feeding a Windows
//! WASAPI shared-mode low-latency stream (<= 10 ms period, spec §4).

pub mod wasapi;
