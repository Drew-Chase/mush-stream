//! WASAPI loopback capture from the default render endpoint.
//!
//! v1 captures the entire system mix. Per-process loopback (the API we'd
//! need for the blacklist) is the same `IAudioClient` plumbing but
//! activated via `ActivateAudioInterfaceAsync` with
//! `AUDIOCLIENT_ACTIVATION_PARAMS` and a target PID — left as a follow-up.
//!
//! The audio engine is asked to deliver 48 kHz interleaved-stereo f32
//! samples; `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` plus
//! `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` make Windows handle any
//! resampling / channel-mixing transparently.

use std::ptr;
use std::slice;

use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{
    SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{CHANNELS, SAMPLE_RATE};

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("WASAPI/COM call failed: {context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
}

trait WinExt<T> {
    fn ctx(self, context: &'static str) -> Result<T, CaptureError>;
}
impl<T> WinExt<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, CaptureError> {
        self.map_err(|source| CaptureError::Win { context, source })
    }
}

pub struct LoopbackCapture {
    audio_client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

impl LoopbackCapture {
    pub fn open() -> Result<Self, CaptureError> {
        unsafe {
            // CoInitializeEx is per-thread. Multiple calls are fine
            // (returns S_FALSE the second time); we ignore that.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ctx("CoCreateInstance(MMDeviceEnumerator)")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ctx("GetDefaultAudioEndpoint(eRender, eConsole)")?;
            let audio_client: IAudioClient =
                device.Activate(CLSCTX_ALL, None).ctx("device.Activate(IAudioClient)")?;

            // Build a WAVEFORMATEXTENSIBLE asking for 48 kHz / 2-ch / f32
            // interleaved. AUTOCONVERTPCM + SRC_DEFAULT_QUALITY makes
            // Windows resample / downmix from whatever the engine has.
            let format = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                    nChannels: CHANNELS,
                    nSamplesPerSec: SAMPLE_RATE,
                    nAvgBytesPerSec: SAMPLE_RATE * u32::from(CHANNELS) * 4,
                    nBlockAlign: CHANNELS * 4,
                    wBitsPerSample: 32,
                    cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>())
                        as u16,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
                dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
                SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            };

            // 1-second buffer (in 100-ns "REFERENCE_TIME" units).
            let buffer_duration_hns: i64 = 10_000_000;
            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    buffer_duration_hns,
                    0,
                    (&raw const format).cast::<WAVEFORMATEX>(),
                    None,
                )
                .ctx("IAudioClient::Initialize(LOOPBACK)")?;

            let event = CreateEventW(None, false, false, None).ctx("CreateEventW")?;
            audio_client.SetEventHandle(event).ctx("SetEventHandle")?;

            let capture: IAudioCaptureClient =
                audio_client.GetService().ctx("GetService(IAudioCaptureClient)")?;
            audio_client.Start().ctx("IAudioClient::Start")?;

            Ok(Self {
                audio_client,
                capture,
                event,
            })
        }
    }

    /// Drains as many WASAPI buffers as are currently available into
    /// `out` (interleaved stereo f32). Blocks up to ~16 ms when the
    /// engine has no data ready, so the caller's outer loop polls
    /// gracefully even during silent periods.
    pub fn read_into(&mut self, out: &mut Vec<f32>) -> Result<(), CaptureError> {
        unsafe {
            let wait = WaitForSingleObject(self.event, 16);
            if wait != WAIT_OBJECT_0 {
                // Timeout / abandoned / failed — return empty; the
                // caller will loop again.
                return Ok(());
            }
            loop {
                let next = self.capture.GetNextPacketSize().ctx("GetNextPacketSize")?;
                if next == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture
                    .GetBuffer(
                        &raw mut data,
                        &raw mut frames,
                        &raw mut flags,
                        None,
                        None,
                    )
                    .ctx("IAudioCaptureClient::GetBuffer")?;

                let frames_usize = frames as usize;
                let samples = frames_usize * CHANNELS as usize;

                #[allow(clippy::cast_sign_loss)]
                let silent_bit = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;
                #[allow(clippy::cast_sign_loss)]
                let discont_bit = AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32;

                if flags & silent_bit != 0 {
                    out.extend(std::iter::repeat_n(0.0f32, samples));
                } else if !data.is_null() {
                    // Engine wrote interleaved f32 thanks to our
                    // WAVEFORMATEXTENSIBLE + AUTOCONVERTPCM. The buffer
                    // returned by WASAPI is f32-aligned by contract.
                    #[allow(clippy::cast_ptr_alignment)]
                    let f32_ptr = data.cast::<f32>();
                    let slice = slice::from_raw_parts(f32_ptr, samples);
                    out.extend_from_slice(slice);
                }
                if flags & discont_bit != 0 {
                    tracing::debug!("WASAPI reported discontinuity");
                }
                self.capture.ReleaseBuffer(frames).ctx("ReleaseBuffer")?;
            }
        }
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.audio_client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

// SAFETY: WASAPI client objects are MTA-safe; we only ever access them
// from the dedicated audio thread that constructed them.
unsafe impl Send for LoopbackCapture {}
