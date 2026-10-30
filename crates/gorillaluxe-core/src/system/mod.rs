//! Host environment management: WHPX feature detection and the
//! zero-friction "Enable WHPX in 1 Click" auto-setup (UAC elevation),
//! Android runtime bootstrapper, device profiles, virtual sensors,
//! shared-memory frames, snapshots and hybrid CPU pinning.

pub mod android_runtime;
pub mod cpu_pinning;
pub mod device_profile;
pub mod env_doctor;
pub mod manifest;
pub mod selftest;
pub mod sensors;
pub mod shm_publisher;
pub mod snapshot;
