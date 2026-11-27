//! WASAPI audio capture:
//! - "Game": active fullscreen game process only (process loopback, including child processes).
//! - "Desktop": everything else (process loopback excluding the game), or full system output if no game is detected.
//! - "Microphone": chosen capture device or Windows default.
//!
//! Captured in the device's native float layout and downmixed to stereo in-app because
//! Windows' built-in 5.1/7.1 -> stereo downmix attenuates audio by 10 dB or more.
//! Stored in RAM as 48 kHz stereo PCM and mixed/encoded to AAC on save.
//!
//! Each capture reads packets on a dedicated thread as soon as they arrive (every 10 ms).
//! Per-process loopback has a tiny queue: polling from the 30 fps video thread caused Windows
//! to drop packets silently and produce choppy audio.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::core::{implement, Interface, GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, WaitForSingleObject, THREAD_PRIORITY_TIME_CRITICAL,
};

use crate::mf::{frames_at, RATE};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// Full default output device loopback.
    System,
    /// Only this process tree.
    Only(u32),
    /// Everything except this process tree.
    AllBut(u32),
    /// Microphone (device ID; empty = Windows default).
    Mic(String),
}

/// Active audio capture. Its reader thread drains packets and `poll` moves them into the ring buffer.
pub struct Audio {
    pub source: Source,
    pub device_id: String,
    shared: Arc<Shared>,
}

/// Shared state between `Audio` and its reader thread.
#[derive(Default)]
struct Shared {
    /// Packets queued for the recorder thread, plus any fatal read error.
    read: Mutex<(Vec<Packet>, Option<anyhow::Error>)>,
    stop: AtomicBool,
}

/// WASAPI packet already downmixed to stereo (`None` = silence), with QPC timestamp (0 if absent).
struct Packet {
    qpc: i64,
    frames: usize,
    stereo: Option<Vec<i16>>,
}

/// WASAPI client handle. Opened on the recording thread (MTA COM, matching the reader thread)
/// and then owned exclusively by its reader thread.
struct Client {
    client: IAudioClient,
    cap: IAudioCaptureClient,
    event: HANDLE,
    device_id: String,
    /// Left/right gain per captured channel.
    downmix: Vec<(f32, f32)>,
}

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Channel count and speaker mask from a client's mix format (falls back to stereo).
pub(crate) unsafe fn mix_layout(client: &IAudioClient) -> (u16, u32) {
    let Ok(f) = client.GetMixFormat() else { return (2, 0x3) };
    let ch = (*f).nChannels.max(1);
    let mask = if (*f).wFormatTag == WAVE_FORMAT_EXTENSIBLE { (*(f as *const WAVEFORMATEXTENSIBLE)).dwChannelMask } else { 0 };
    CoTaskMemFree(Some(f as *const _));
    (ch, mask)
}

/// Computes `(left, right)` downmix weights per channel. Front-left/right pass at unity gain
/// so stereo games record at playback level; center and surrounds mix lower because many
/// 5.1/7.1 drivers mirror stereo across those channels. LFE/subwoofer is omitted.
fn downmix(channels: u16, mask: u32) -> Vec<(f32, f32)> {
    const H: f32 = 0.35;
    if channels == 1 {
        return vec![(1.0, 1.0)];
    }
    // No mask: assume standard channel order (FL, FR, FC, LFE, BL, BR, ...).
    let mask = if mask == 0 { (1u32 << channels) - 1 } else { mask };
    (0..32)
        .filter(|b| mask & (1 << b) != 0)
        .take(channels as usize)
        .map(|b| match 1u32 << b {
            0x1 => (1.0, 0.0),
            0x2 => (0.0, 1.0),
            0x4 => (0.5, 0.5),
            0x10 | 0x40 | 0x200 => (H, 0.0),
            0x20 | 0x80 | 0x400 => (0.0, H),
            0x100 => (H, H),
            _ => (0.0, 0.0),
        })
        .chain(std::iter::repeat((0.0, 0.0)))
        .take(channels as usize)
        .collect()
}

impl Audio {
    /// Opens the capture source and spawns its dedicated packet-reader thread.
    pub fn open(source: Source) -> Result<Self> {
        let client = unsafe { Client::open(&source)? };
        let device_id = client.device_id.clone();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        std::thread::Builder::new().name("audio".into()).spawn(move || unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            // Mostly sleeps on events and downmixes small buffers; high priority keeps wakeups timely under load.
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            let mut got = Vec::new();
            while !s.stop.load(Ordering::Relaxed) {
                // Bounded wait: per-process loopback emits no events when the process is silent.
                WaitForSingleObject(client.event, 100);
                let r = client.read(&mut got);
                let mut read = s.read.lock().unwrap();
                read.0.append(&mut got);
                if let Err(e) = r {
                    read.1 = Some(e);
                    break;
                }
            }
            drop(client);
            if com {
                CoUninitialize();
            }
        })?;
        Ok(Self { source, device_id, shared })
    }

    /// Drains queued packets into `buf`, aligned on the timeline via their QPC timestamps.
    pub fn poll(&mut self, buf: &mut AudioBuf, t0: i64) -> Result<()> {
        let (got, err) = {
            let mut read = self.shared.read.lock().unwrap();
            (std::mem::take(&mut read.0), read.1.take())
        };
        for p in got {
            let pos = if p.qpc != 0 { frames_at(p.qpc - t0) } else { buf.end() };
            buf.push_at(pos, p.stereo.as_deref(), p.frames);
        }
        err.map_or(Ok(()), Err)
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // Reader thread observes this on its next wakeup (10 ms, or 100 ms when silent) and drops the client.
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

// WASAPI objects live in MTA COM and can move between MTA threads.
unsafe impl Send for Client {}

impl Client {
    unsafe fn open(source: &Source) -> Result<Self> {
        let mic = matches!(source, Source::Mic(_));
        let (client, device_id) = match source {
            Source::System => {
                let dev = default_device()?;
                let id = device_id(&dev);
                (dev.Activate::<IAudioClient>(CLSCTX_ALL, None)?, id)
            }
            Source::Only(pid) => (activate_process(*pid, true)?, String::new()),
            Source::AllBut(pid) => (activate_process(*pid, false)?, String::new()),
            Source::Mic(id) => {
                let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let dev = if id.is_empty() {
                    en.GetDefaultAudioEndpoint(eCapture, eConsole)?
                } else {
                    en.GetDevice(&windows::core::HSTRING::from(id.as_str()))?
                };
                (dev.Activate::<IAudioClient>(CLSCTX_ALL, None)?, device_id(&dev))
            }
        };
        let loopback = if mic { 0 } else { AUDCLNT_STREAMFLAGS_LOOPBACK };
        let event = CreateEventW(None, false, false, PCWSTR::null())?;
        // Per-process loopback has no mix format of its own; query the default output's layout.
        // Windows resamples to 48 kHz and we downmix the channels ourselves.
        let (channels, mask) = match source {
            Source::Only(_) | Source::AllBut(_) => mix_layout(&default_device()?.Activate::<IAudioClient>(CLSCTX_ALL, None)?),
            _ => mix_layout(&client),
        };
        let format = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: channels,
                nSamplesPerSec: RATE as u32,
                nAvgBytesPerSec: RATE as u32 * channels as u32 * 4,
                nBlockAlign: channels * 4,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
            dwChannelMask: if mask == 0 { (1u32 << channels) - 1 } else { mask },
            SubFormat: SUBTYPE_IEEE_FLOAT,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            loopback
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            2_000_000,
            0,
            &format.Format,
            None,
        )?;
        client.SetEventHandle(event)?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Self { client, cap, event, device_id, downmix: downmix(channels, mask) })
    }

    /// Appends all currently available packets (downmixed to stereo) to `got`.
    unsafe fn read(&self, got: &mut Vec<Packet>) -> Result<()> {
        while self.cap.GetNextPacketSize()? != 0 {
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            self.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
            let stereo = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null()).then(|| {
                let ch = self.downmix.len();
                let src = std::slice::from_raw_parts(data as *const f32, frames as usize * ch);
                let mut out = Vec::with_capacity(frames as usize * 2);
                for f in src.chunks_exact(ch) {
                    let (mut l, mut r) = (0.0f32, 0.0f32);
                    for (x, (gl, gr)) in f.iter().zip(&self.downmix) {
                        l += x * gl;
                        r += x * gr;
                    }
                    out.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                    out.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
                out
            });
            self.cap.ReleaseBuffer(frames)?;
            got.push(Packet { qpc: qpc as i64, frames: frames as usize, stereo });
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Option<&IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// `PROPVARIANT` holding a `VT_BLOB` (x64 layout) pointing to activation parameters.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    size: u32,
    _pad: u32,
    data: *const u8,
}

pub(crate) unsafe fn activate_process(pid: u32, include: bool) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if include {
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                } else {
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                },
            },
        },
    };
    let blob = BlobVariant {
        vt: 65, // VT_BLOB
        r1: 0,
        r2: 0,
        r3: 0,
        size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        data: &params as *const _ as *const u8,
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    let op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&blob as *const BlobVariant as *const windows::core::PROPVARIANT),
        &handler,
    )?;
    // Runs on the video thread: bail out quickly instead of stalling recording if Windows hangs.
    rx.recv_timeout(Duration::from_secs(1)).map_err(|_| anyhow!("per-process audio capture didn't respond"))?;
    let mut hr = windows::core::HRESULT(0);
    let mut unk = None;
    op.GetActivateResult(&mut hr, &mut unk)?;
    hr.ok()?;
    Ok(unk.ok_or_else(|| anyhow!("missing audio client"))?.cast()?)
}

unsafe fn default_device() -> Result<IMMDevice> {
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    Ok(en.GetDefaultAudioEndpoint(eRender, eConsole)?)
}

unsafe fn device_id(dev: &IMMDevice) -> String {
    match dev.GetId() {
        Ok(p) => {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        }
        Err(_) => String::new(),
    }
}

/// Active capture devices: `(id, friendly_name)`.
pub fn capture_devices() -> Vec<(String, String)> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::STGM_READ;
    let mut v = Vec::new();
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return v };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return v };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(dev) = col.Item(i) else { continue };
            let id = device_id(&dev);
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|ps| ps.GetValue(&PKEY_Device_FriendlyName))
                .map(|p| p.to_string())
                .unwrap_or_else(|_| "Microphone".to_string());
            v.push((id, name));
        }
    }
    v
}

/// Current default render device ID (used to detect headset/speaker switches).
pub fn current_default_id() -> String {
    unsafe { default_device().map(|d| device_id(&d)).unwrap_or_default() }
}

/// Ring buffer of interleaved stereo PCM. `start` = frame index (from `t0`) of `data[0]`.
#[derive(Default)]
pub struct AudioBuf {
    pub start: i64,
    pub data: VecDeque<i16>,
}

impl AudioBuf {
    pub fn end(&self) -> i64 {
        self.start + (self.data.len() / 2) as i64
    }

    pub fn push_at(&mut self, pos: i64, samples: Option<&[i16]>, frames: usize) {
        if self.data.is_empty() {
            self.start = pos;
        }
        let end = self.end();
        let mut skip = 0usize;
        if pos > end + RATE / 100 {
            // Silence gap: pad zeros to preserve A/V sync.
            let gap = (pos - end) as usize;
            self.data.extend(std::iter::repeat_n(0, gap * 2));
        } else if pos < end - RATE / 50 {
            // Overlapping frames: drop the already-buffered prefix.
            skip = ((end - pos) as usize).min(frames);
        }
        match samples {
            Some(s) => self.data.extend(s[skip * 2..].iter().copied()),
            None => self.data.extend(std::iter::repeat_n(0, (frames - skip) * 2)),
        }
    }

    pub fn trim_before(&mut self, frame: i64) {
        if frame > self.start {
            let n = ((frame - self.start) as usize).min(self.data.len() / 2);
            self.data.drain(..n * 2);
            self.start += n as i64;
        }
    }

    /// Accumulates audio starting at `from` into `out` (interleaved stereo), scaled by `gain`,
    /// reading across both halves of the ring buffer.
    #[inline(never)] // called per chunk; inlining across call sites bloats the exe by ~3 KB for no gain
    pub fn mix_into(&self, out: &mut [f32], from: i64, gain: f32) {
        let skip = (((self.start - from).max(0) * 2) as usize).min(out.len());
        let mut out = &mut out[skip..];
        let mut first = ((from - self.start).max(0) * 2) as usize;
        let (a, b) = self.data.as_slices();
        for part in [a, b] {
            let src = part.get(first..).unwrap_or(&[]);
            first = first.saturating_sub(part.len());
            let n = src.len().min(out.len());
            for (o, &s) in out[..n].iter_mut().zip(src) {
                *o += s as f32 * gain;
            }
            out = &mut std::mem::take(&mut out)[n..];
        }
    }
}
