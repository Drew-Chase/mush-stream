//! Per-process WASAPI loopback capture via `ActivateAudioInterfaceAsync`
//! with `AUDIOCLIENT_ACTIVATION_PARAMS`. Used by the audio mixer to
//! capture each non-blacklisted process tree separately so the mixed
//! output excludes blacklisted apps.
//!
//! Available on Windows 10 build 20348+ / Windows 11. On older Windows
//! the activation will fail at runtime — callers should log and fall
//! back (we never do, since the host README requires Windows 11).
//!
//! The PROPVARIANT for VT_BLOB has to be built by hand: windows-core
//! 0.58 keeps the inner anonymous union private, so we zero a
//! `MaybeUninit<PROPVARIANT>` and write the well-known field offsets
//! directly. The same trick is used in Microsoft's C++ samples.

use std::mem::{MaybeUninit, size_of};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicI32, Ordering};

use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::{
    SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::core::{Interface, PROPVARIANT, implement};

use super::{CHANNELS, SAMPLE_RATE};

/// VT_BLOB == 65 (`VARENUM`).
const VT_BLOB_VALUE: u16 = 65;

#[derive(Debug, Error)]
pub enum ProcessLoopbackError {
    #[error("WASAPI/COM call failed: {context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("ActivateAudioInterfaceAsync timed out for PID {pid}")]
    ActivationTimeout { pid: u32 },
    #[error("ActivateAudioInterfaceAsync failed for PID {pid}: HRESULT 0x{hr:08X}")]
    ActivationFailed { pid: u32, hr: u32 },
}

trait WinExt<T> {
    fn ctx(self, context: &'static str) -> Result<T, ProcessLoopbackError>;
}
impl<T> WinExt<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, ProcessLoopbackError> {
        self.map_err(|source| ProcessLoopbackError::Win { context, source })
    }
}

/// Wraps a per-process loopback `IAudioClient`. Same shape and API as
/// the system-wide [`super::LoopbackCapture`].
pub struct ProcessLoopbackCapture {
    audio_client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

impl ProcessLoopbackCapture {
    /// Open a loopback capture targeting `pid`'s process tree.
    /// `INCLUDE_TARGET_PROCESS_TREE` includes child processes too,
    /// which matters for browsers (Chrome / Firefox spawn audio
    /// renderers in worker processes).
    #[allow(clippy::too_many_lines)] // single coherent activation sequence
    pub fn open(pid: u32) -> Result<Self, ProcessLoopbackError> {
        unsafe {
            // Build the activation params blob. `params` MUST outlive
            // the ActivateAudioInterfaceAsync call — keep it on the
            // stack and only use it inside this function.
            let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        ProcessLoopbackMode:
                            PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };

            // Pack the params into a PROPVARIANT(VT_BLOB). windows-core
            // hides the inner anonymous union, so we go raw on a zeroed
            // MaybeUninit. Layout (x64):
            //   offset 0:  u16 vt
            //   offset 2:  u16 wReserved1
            //   offset 4:  u16 wReserved2
            //   offset 6:  u16 wReserved3
            //   offset 8:  u32 cbSize          (BLOB.cbSize)
            //   offset 12: u32 padding
            //   offset 16: *const u8 pBlobData (BLOB.pBlobData)
            //
            // CRITICAL: we never call `assume_init()` on this. windows-
            // core's `PROPVARIANT` impls `Drop`, which calls
            // `PropVariantClear`. For VT_BLOB that frees `pBlobData` via
            // `CoTaskMemFree` — but our blob points at a *stack* local
            // (`params`), not at a CoTaskMem-allocated buffer. Running
            // that destructor segfaults silently and takes the host
            // process down. Keeping the storage as `MaybeUninit` means
            // its `Drop` is a no-op; we just pass `as_ptr()` to
            // `ActivateAudioInterfaceAsync` and let the stack reclaim
            // the bytes when the function returns.
            let mut prop_storage = MaybeUninit::<PROPVARIANT>::zeroed();
            let raw = prop_storage.as_mut_ptr().cast::<u8>();
            ptr::write_unaligned(raw.add(0).cast::<u16>(), VT_BLOB_VALUE);
            ptr::write_unaligned(
                raw.add(8).cast::<u32>(),
                size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            );
            ptr::write_unaligned(
                raw.add(16).cast::<*mut u8>(),
                ptr::from_mut(&mut params).cast::<u8>(),
            );
            let prop_ptr: *const PROPVARIANT = prop_storage.as_ptr();

            // Set up a completion handler that signals an event when
            // the activation finishes. The handler also stashes the
            // HRESULT so we can propagate failure cleanly.
            let event = CreateEventW(None, false, false, None).ctx("CreateEventW")?;
            let hr_slot = std::sync::Arc::new(AtomicI32::new(0));
            let handler: IActivateAudioInterfaceCompletionHandler =
                ActivateHandler { event, hr: hr_slot.clone() }.into();

            let async_op: IActivateAudioInterfaceAsyncOperation = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(prop_ptr),
                &handler,
            )
            .ctx("ActivateAudioInterfaceAsync")?;

            // Wait up to 2s for the completion handler to fire. Real
            // activations on a healthy system finish in <50ms.
            let wait = WaitForSingleObject(event, 2_000);
            if wait != WAIT_OBJECT_0 {
                return Err(ProcessLoopbackError::ActivationTimeout { pid });
            }
            let hr = windows::core::HRESULT(hr_slot.load(Ordering::Acquire));
            if !hr.is_ok() {
                return Err(ProcessLoopbackError::ActivationFailed {
                    pid,
                    hr: hr.0 as u32,
                });
            }

            // Pull the activated IAudioClient out of the async op.
            let mut activated_iface: Option<windows::core::IUnknown> = None;
            let mut activate_hr = windows::core::HRESULT(0);
            async_op
                .GetActivateResult(&raw mut activate_hr, &raw mut activated_iface)
                .ctx("GetActivateResult")?;
            if !activate_hr.is_ok() {
                return Err(ProcessLoopbackError::ActivationFailed {
                    pid,
                    hr: activate_hr.0 as u32,
                });
            }
            let activated = activated_iface.ok_or(ProcessLoopbackError::ActivationFailed {
                pid,
                hr: 0x8000_4005, // E_FAIL
            })?;
            let audio_client: IAudioClient =
                activated.cast().ctx("activated.cast::<IAudioClient>")?;

            // Format: 48 kHz / 2-ch / f32 interleaved, same as the
            // system-mix path. Windows handles the resample/downmix
            // when the per-process render is at a different rate.
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

            // Per MSDN process-loopback samples: shared mode, LOOPBACK
            // + EVENTCALLBACK. The buffer duration MUST be 0 for
            // process loopback (the system manages buffering itself).
            audio_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    0,
                    0,
                    (&raw const format).cast::<WAVEFORMATEX>(),
                    None,
                )
                .ctx("IAudioClient::Initialize(process loopback)")?;

            let capture_event =
                CreateEventW(None, false, false, None).ctx("CreateEventW(capture)")?;
            audio_client.SetEventHandle(capture_event).ctx("SetEventHandle")?;
            let capture: IAudioCaptureClient = audio_client
                .GetService()
                .ctx("GetService(IAudioCaptureClient)")?;
            audio_client.Start().ctx("IAudioClient::Start")?;

            // Done with the activation event.
            let _ = CloseHandle(event);

            Ok(Self {
                audio_client,
                capture,
                event: capture_event,
            })
        }
    }

    /// Drain whatever the engine currently has into `out`. Same
    /// semantics as `LoopbackCapture::read_into`: a 16ms event wait
    /// when nothing is available, then loop until the queue is empty.
    pub fn read_into(&mut self, out: &mut Vec<f32>) -> Result<(), ProcessLoopbackError> {
        unsafe {
            let wait = WaitForSingleObject(self.event, 16);
            if wait != WAIT_OBJECT_0 {
                return Ok(());
            }
            loop {
                let next = self
                    .capture
                    .GetNextPacketSize()
                    .ctx("GetNextPacketSize")?;
                if next == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                self.capture
                    .GetBuffer(&raw mut data, &raw mut frames, &raw mut flags, None, None)
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
                    #[allow(clippy::cast_ptr_alignment)]
                    let f32_ptr = data.cast::<f32>();
                    let slice = slice::from_raw_parts(f32_ptr, samples);
                    out.extend_from_slice(slice);
                }
                if flags & discont_bit != 0 {
                    tracing::debug!("process-loopback discontinuity");
                }
                self.capture.ReleaseBuffer(frames).ctx("ReleaseBuffer")?;
            }
        }
    }
}

impl Drop for ProcessLoopbackCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.audio_client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

// SAFETY: the WASAPI client objects are MTA-safe and we only touch them
// from the dedicated audio thread.
unsafe impl Send for ProcessLoopbackCapture {}

/// Tiny COM object that signals an event when the async activation
/// finishes and stashes the HRESULT for the caller to inspect.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivateHandler {
    event: HANDLE,
    hr: std::sync::Arc<AtomicI32>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(
        &self,
        activateoperation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe {
            // Best-effort: try to capture the operation's HRESULT here
            // so the caller can see the real failure code instead of a
            // generic E_FAIL.
            if let Some(op) = activateoperation {
                let mut activate_hr = windows::core::HRESULT(0);
                let mut iface: Option<windows::core::IUnknown> = None;
                let _ = op.GetActivateResult(&raw mut activate_hr, &raw mut iface);
                self.hr.store(activate_hr.0, Ordering::Release);
            }
            let _ = SetEvent(self.event);
        }
        Ok(())
    }
}
