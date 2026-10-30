//! WASAPI shared-mode output — `virtio-snd` host backend.
//! Targets <= 10 ms engine periods and degrades gracefully on
//! headless hosts with no audio endpoint (CI runners).

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL};

/// Requested engine period in 100 ns units (10 ms).
pub const PERIOD_HNS: i64 = 100_000;

/// A live WASAPI shared-mode output stream.
pub struct AudioOutput {
    client: IAudioClient,
    pub buffer_frames: u32,
    pub period_hns: i64,
    pub sample_rate: u32,
    pub channels: u16,
    /// `true` when the engine actually granted <= 10 ms.
    pub low_latency: bool,
}

impl AudioOutput {
    /// Initialize the default render endpoint in shared mode.
    /// Returns `Err` (never panics) when no endpoint exists — the
    /// caller treats that as "audio unavailable", not a failure.
    pub fn init() -> Result<AudioOutput, String> {
        unsafe {
            // COM may already be initialized by the caller; all results
            // other than a hard failure are acceptable here.
            let _ = CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("MMDeviceEnumerator: {e}"))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("no default render endpoint: {e}"))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("IMMDevice::Activate(IAudioClient): {e}"))?;

            let fmt = client
                .GetMixFormat()
                .map_err(|e| format!("GetMixFormat: {e}"))?;
            if fmt.is_null() {
                return Err("GetMixFormat returned null".into());
            }
            let (rate, ch) = ((*fmt).nSamplesPerSec, (*fmt).nChannels);

            // Try the requested 10 ms period first, then fall back to
            // the engine's minimum supported device period.
            let mut period = PERIOD_HNS;
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                period,
                0,
                fmt,
                None,
            );
            if init.is_err() {
                let mut min = 0i64;
                client
                    .GetDevicePeriod(None, Some(&mut min))
                    .map_err(|e| format!("GetDevicePeriod: {e}"))?;
                if min > 0 {
                    period = min;
                }
                client
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, period, 0, fmt, None)
                    .map_err(|e| format!("IAudioClient::Initialize: {e}"))?;
            }

            let buffer_frames = client
                .GetBufferSize()
                .map_err(|e| format!("GetBufferSize: {e}"))?;
            windows::Win32::System::Com::CoTaskMemFree(Some(fmt as *const _));

            Ok(AudioOutput {
                client,
                buffer_frames,
                period_hns: period,
                sample_rate: rate,
                channels: ch,
                low_latency: period <= PERIOD_HNS,
            })
        }
    }

    /// Start the stream (guest audio frames arrive later phases).
    pub fn start(&self) -> Result<(), String> {
        unsafe { self.client.Start().map_err(|e| format!("IAudioClient::Start: {e}")) }
    }
}
