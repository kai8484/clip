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
    /// Owned by the poller so draining never frees the batch the reader thread keeps reusing.
    drain: PacketBatch,
}

/// Shared state between `Audio` and its reader thread.
#[derive(Default)]
struct Shared {
    /// Packets queued for the recorder thread, plus any fatal read error.
    read: Mutex<(PacketBatch, Option<anyhow::Error>)>,
    stop: AtomicBool,
}

/// One packet's place in `PacketBatch::samples`, with QPC timestamp (0 if absent).
/// `len == 0` marks a silent packet, which stores no samples.
struct PacketMeta {
    qpc: i64,
    frames: usize,
    offset: usize,
    len: usize,
}

/// Flat, reusable batch of downmixed packets: `meta` indexes `samples`.
/// Both `Vec`s keep their capacity across drains so the 10 ms reader never reallocates.
#[derive(Default)]
struct PacketBatch {
    meta: Vec<PacketMeta>,
    samples: Vec<i16>,
}

impl PacketBatch {
    fn clear(&mut self) {
        self.meta.clear();
        self.samples.clear();
        // A stall can queue seconds of audio; don't keep that peak allocation forever.
        if self.samples.capacity() > RATE as usize * 8 {
            self.samples.shrink_to(RATE as usize * 2);
        }
    }

    /// Moves every packet of `other` to the end of `self`, leaving `other` empty but allocated.
    fn append_from(&mut self, other: &mut PacketBatch) {
        let base = self.samples.len();
        self.meta.extend(other.meta.drain(..).map(|mut p| {
            p.offset += base;
            p
        }));
        self.samples.extend_from_slice(&other.samples);
        other.samples.clear();
    }
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
            let mut got = PacketBatch::default();
            while !s.stop.load(Ordering::Relaxed) {
                // Bounded wait: per-process loopback emits no events when the process is silent.
                WaitForSingleObject(client.event, 100);
                let r = client.read(&mut got);
                let mut read = s.read.lock().unwrap();
                read.0.append_from(&mut got);
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
        Ok(Self { source, device_id, shared, drain: PacketBatch::default() })
    }

    /// Drains queued packets into `buf`, aligned on the timeline via their QPC timestamps.
    pub fn poll(&mut self, buf: &mut AudioBuf, t0: i64) -> Result<()> {
        let err = {
            let mut read = self.shared.read.lock().unwrap();
            // Swap instead of take: the reader thread gets its buffers back and keeps their capacity.
            std::mem::swap(&mut self.drain, &mut read.0);
            read.1.take()
        };
        for p in &self.drain.meta {
            let pos = if p.qpc != 0 { frames_at(p.qpc - t0) } else { buf.end() };
            let samples = (p.len > 0).then(|| &self.drain.samples[p.offset..p.offset + p.len]);
            buf.push_at(pos, samples, p.frames);
        }
        self.drain.clear();
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
    unsafe fn read(&self, got: &mut PacketBatch) -> Result<()> {
        while self.cap.GetNextPacketSize()? != 0 {
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            self.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
            let offset = got.samples.len();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                let ch = self.downmix.len();
                let src = std::slice::from_raw_parts(data as *const f32, frames as usize * ch);
                downmix_to_stereo(src, &self.downmix, &mut got.samples);
            }
            let len = got.samples.len() - offset;
            self.cap.ReleaseBuffer(frames)?;
            got.meta.push(PacketMeta { qpc: qpc as i64, frames: frames as usize, offset, len });
        }
        Ok(())
    }
}

/// Clamps to full scale and converts to `i16`.
#[inline]
fn to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Downmixes interleaved `src` (native channel layout, `downmix.len()` channels per frame)
/// to stereo, appending interleaved `i16` pairs to `out`.
fn downmix_to_stereo(src: &[f32], downmix: &[(f32, f32)], out: &mut Vec<i16>) {
    // Studio stereo and mono are the common cases; skip the per-sample gain loop for them.
    if downmix == [(1.0f32, 0.0f32), (0.0, 1.0)] {
        out.reserve(src.len());
        for &[l, r] in src.as_chunks::<2>().0 {
            out.push(to_i16(l));
            out.push(to_i16(r));
        }
        return;
    }
    if downmix == [(1.0f32, 1.0f32)] {
        out.reserve(src.len() * 2);
        for &x in src {
            let s = to_i16(x);
            out.push(s);
            out.push(s);
        }
        return;
    }
    let ch = downmix.len();
    if ch == 0 {
        return;
    }
    out.reserve(src.len() / ch * 2);
    for f in src.chunks_exact(ch) {
        let (mut l, mut r) = (0.0f32, 0.0f32);
        for (x, (gl, gr)) in f.iter().zip(downmix) {
            l += x * gl;
            r += x * gr;
        }
        out.push(to_i16(l));
        out.push(to_i16(r));
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
/// Initializes MTA COM on the calling thread, so any worker can call this without `Mf::init()`.
pub fn capture_devices() -> Vec<(String, String)> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::STGM_READ;
    let mut v = Vec::new();
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            if com {
                CoUninitialize();
            }
            return v;
        };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
            if com {
                CoUninitialize();
            }
            return v;
        };
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
    if com {
        unsafe { CoUninitialize() };
    }
    v
}

/// Current default render device ID (used to detect headset/speaker switches).
pub fn current_default_id() -> String {
    unsafe { default_device().map(|d| device_id(&d)).unwrap_or_default() }
}

/// Frames per immutable block: 0.5 s at 48 kHz.
const BLOCK_FRAMES: usize = RATE as usize / 2;

/// Immutable run of interleaved stereo PCM. `start` = frame index (from `t0`) of `samples[0]`.
/// Blocks are shared with snapshots by `Arc` clone, so saving never copies the clip.
#[derive(Clone)]
pub struct AudioBlock {
    pub start: i64,
    pub samples: Arc<[i16]>,
}

/// Timestamped audio ring: sealed `blocks` plus a mutable `tail` (`tail_start` = its frame index).
/// Silence and QPC gaps store no samples at all — the gap is simply absent from the timeline.
#[derive(Default)]
pub struct AudioBuf {
    blocks: VecDeque<AudioBlock>,
    tail_start: i64,
    tail: Vec<i16>,
    /// Frame index one past the last captured frame (gaps included), i.e. the timeline position.
    end_frame: i64,
}

impl AudioBuf {
    pub fn end(&self) -> i64 {
        self.end_frame
    }

    /// Appends one WASAPI packet or silence marker at timeline position `pos`.
    pub fn push_at(&mut self, pos: i64, samples: Option<&[i16]>, frames: usize) {
        // Floor of the history: a gap only becomes silence once real audio is dropped for it.
        // `end_frame` tracks `max(pos + frames)`, so blocks/tail can start anywhere in the ring.
        if self.blocks.is_empty() && self.tail.is_empty() && self.end_frame == 0 {
            self.tail_start = pos;
            self.end_frame = pos;
        }
        let mut skip = 0usize;
        if pos < self.end_frame - RATE / 50 {
            // Overlapping frames: drop the already-buffered prefix.
            skip = ((self.end_frame - pos) as usize).min(frames);
        } else if pos > self.end_frame + RATE / 100 {
            // Silence or sleep/QPC gap: seal what we have and store no zeros for the hole.
            self.flush_tail();
        }
        if let Some(s) = samples {
            let slice = s.get(skip * 2..).unwrap_or(&[]);
            if !slice.is_empty() {
                let at = pos + skip as i64;
                // Keep `tail` contiguous; a small discontinuity starts a new block.
                if !self.tail.is_empty() && at != self.tail_start + (self.tail.len() / 2) as i64 {
                    self.flush_tail();
                }
                if self.tail.is_empty() {
                    self.tail_start = at;
                }
                self.tail.extend_from_slice(slice);
                while self.tail.len() >= BLOCK_FRAMES * 2 {
                    let rest = self.tail.split_off(BLOCK_FRAMES * 2);
                    let sealed = std::mem::replace(&mut self.tail, rest);
                    self.blocks.push_back(AudioBlock { start: self.tail_start, samples: Arc::from(sealed) });
                    self.tail_start += BLOCK_FRAMES as i64;
                }
            }
        }
        // `None` stores nothing: the gap stays absent and the timeline below still advances.
        self.end_frame = self.end_frame.max(pos + frames as i64);
    }

    /// Seals the mutable tail into an immutable block.
    fn flush_tail(&mut self) {
        if self.tail.is_empty() {
            return;
        }
        let sealed = std::mem::take(&mut self.tail);
        self.blocks.push_back(AudioBlock { start: self.tail_start, samples: Arc::from(sealed) });
    }

    pub fn trim_before(&mut self, frame: i64) {
        while self.blocks.front().is_some_and(|b| b.start + (b.samples.len() / 2) as i64 <= frame) {
            self.blocks.pop_front();
        }
        if !self.tail.is_empty() && self.tail_start + (self.tail.len() / 2) as i64 <= frame {
            self.tail.clear();
        }
        // A sleep gap can drop thousands of blocks at once; don't keep the peak capacity.
        if self.blocks.capacity() > 64 && self.blocks.len() * 4 < self.blocks.capacity() {
            self.blocks.shrink_to_fit();
        }
    }

    /// Blocks overlapping the window at or after frame `from`, shared by `Arc` clone (no copy).
    pub fn snapshot_blocks(&self, from: i64) -> Vec<AudioBlock> {
        let mut v: Vec<AudioBlock> = self
            .blocks
            .iter()
            .filter(|b| b.start + (b.samples.len() / 2) as i64 > from)
            .cloned()
            .collect();
        if !self.tail.is_empty() && self.tail_start + (self.tail.len() / 2) as i64 > from {
            v.push(AudioBlock { start: self.tail_start, samples: Arc::from(self.tail.as_slice()) });
        }
        v
    }

    /// All buffered samples in timeline order (diagnostics only).
    pub fn samples(&self) -> impl Iterator<Item = i16> + '_ {
        self.blocks.iter().flat_map(|b| b.samples.iter().copied()).chain(self.tail.iter().copied())
    }

    /// Accumulates audio starting at `from` into `out` (interleaved stereo), scaled by `gain`.
    pub fn mix_into(&self, out: &mut [f32], from: i64, gain: f32) {
        for b in &self.blocks {
            mix_slice(out, b.start, &b.samples, from, gain);
        }
        if !self.tail.is_empty() {
            mix_slice(out, self.tail_start, &self.tail, from, gain);
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_heap_bytes(&self) -> (usize, usize) {
        let elem = std::mem::size_of::<i16>();
        let stored: usize = self.blocks.iter().map(|b| b.samples.len() * elem).sum();
        let cap = stored + self.tail.capacity() * elem + self.blocks.capacity() * std::mem::size_of::<AudioBlock>();
        (stored + self.tail.len() * elem, cap)
    }
}

/// Adds `samples` (interleaved stereo, starting at frame `start`) into `out`, which starts at
/// frame `from`. Shared by the live ring and by snapshots so both mix identically.
#[inline(never)] // called per chunk; inlining across call sites bloats the exe by ~3 KB for no gain
pub fn mix_slice(out: &mut [f32], start: i64, samples: &[i16], from: i64, gain: f32) {
    let skip = (((start - from).max(0) * 2) as usize).min(out.len());
    let out = &mut out[skip..];
    let first = ((from - start).max(0) * 2) as usize;
    let Some(src) = samples.get(first..) else { return };
    for (o, &s) in out.iter_mut().zip(src) {
        *o += s as f32 * gain;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "micro-benchmark"]
    fn perf_bench_audio_buf() {
        let mut buf = AudioBuf::default();
        let t0 = std::time::Instant::now();
        // 60s of silent 48kHz stereo packets (100 packets/s)
        for i in 0..6_000i64 {
            buf.push_at(i * 480, None, 480);
        }
        // Simulate a 10-minute sleep/hibernate QPC jump followed by one active packet
        let active = vec![1200i16; 480 * 2];
        buf.push_at(6_000 * 480 + 48_000 * 600, Some(&active), 480);
        let push_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = std::time::Instant::now();
        buf.trim_before(6_000 * 480 + 48_000 * 570);
        let trim_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let (used_b, cap_b) = buf.debug_heap_bytes();
        let t2 = std::time::Instant::now();
        let snap = buf.snapshot_blocks(6_000 * 480 + 48_000 * 570);
        let snap_ms = t2.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box(snap);

        println!(
            "BENCH audio_buf: push={push_ms:.3}ms trim={trim_ms:.3}ms snapshot={snap_ms:.3}ms used={:.2}MB cap={:.2}MB",
            used_b as f64 / (1024.0 * 1024.0),
            cap_b as f64 / (1024.0 * 1024.0)
        );
    }

    #[test]
    fn downmix_stereo_identity() {
        let mut out = Vec::new();
        downmix_to_stereo(&[0.5, -0.5, 1.5, 1.0], &[(1.0, 0.0), (0.0, 1.0)], &mut out);
        assert_eq!(out, vec![16383, -16383, 32767, 32767]);
    }

    #[test]
    fn downmix_mono_duplicates() {
        let mut out = Vec::new();
        downmix_to_stereo(&[0.25, -1.0], &[(1.0, 1.0)], &mut out);
        assert_eq!(out, vec![8191, 8191, -32767, -32767]);
    }

    #[test]
    fn downmix_surround_matches_reference() {
        // 5.1 layout: FL, FR, FC, LFE, BL, BR.
        let weights = downmix(6, 0x3f);
        assert_ne!(weights, [(1.0, 0.0), (0.0, 1.0)], "must take the general path");
        let src: Vec<f32> = (0..6 * 4).map(|i| (i as f32 * 0.1).sin() * 0.4).collect();
        let mut out = Vec::new();
        downmix_to_stereo(&src, &weights, &mut out);

        let mut want = Vec::new();
        for f in src.chunks_exact(6) {
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for (x, (gl, gr)) in f.iter().zip(&weights) {
                l += x * gl;
                r += x * gr;
            }
            want.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
            want.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
        }
        assert_eq!(out, want);
    }

    #[test]
    fn packet_batch_append_keeps_offsets() {
        let mut a = PacketBatch::default();
        a.samples.extend_from_slice(&[1, 2, 3, 4]);
        a.meta.push(PacketMeta { qpc: 1, frames: 2, offset: 0, len: 4 });

        let mut b = PacketBatch::default();
        b.samples.extend_from_slice(&[5, 6]);
        b.samples.extend_from_slice(&[7, 8, 9, 10]);
        b.meta.push(PacketMeta { qpc: 2, frames: 1, offset: 0, len: 2 });
        b.meta.push(PacketMeta { qpc: 3, frames: 2, offset: 2, len: 4 });
        b.meta.push(PacketMeta { qpc: 4, frames: 2, offset: 6, len: 0 });
        let cap = b.samples.capacity();

        a.append_from(&mut b);
        assert!(b.meta.is_empty() && b.samples.is_empty(), "source batch is emptied");
        assert_eq!(b.samples.capacity(), cap, "source keeps its allocation");

        let got: Vec<(i64, i64, usize)> = a
            .meta
            .iter()
            .map(|p| (p.qpc, p.offset as i64, p.len))
            .collect();
        assert_eq!(got, vec![(1, 0, 4), (2, 4, 2), (3, 6, 4), (4, 10, 0)]);
        for p in &a.meta {
            if p.len > 0 {
                assert_eq!(p.frames * 2, p.len, "non-silent packet stores 2 samples per frame");
            }
        }
        assert_eq!(&a.samples[4..6], &[5, 6]);
        assert_eq!(&a.samples[6..10], &[7, 8, 9, 10]);

        a.clear();
        assert!(a.meta.is_empty() && a.samples.is_empty());
        assert!(a.samples.capacity() >= 10, "clear keeps capacity");
    }

    #[test]
    #[ignore = "micro-benchmark"]
    fn perf_bench_audio_downmix() {
        let src: Vec<f32> = (0..480 * 2).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect();
        let mut out = Vec::new();
        let t = std::time::Instant::now();
        for _ in 0..10_000 {
            out.clear();
            downmix_to_stereo(&src, &[(1.0, 0.0), (0.0, 1.0)], &mut out);
        }
        std::hint::black_box(&out);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("BENCH audio_downmix: 10_000 x 480 stereo frames in {ms:.3}ms");
    }

    #[test]
    fn silent_packets_and_gaps_store_no_samples() {
        let mut buf = AudioBuf::default();
        let pcm = vec![1000i16; 480 * 2];
        buf.push_at(0, Some(&pcm), 480);
        buf.push_at(480, None, 480); // silent WASAPI packet
        let jump = 48_000 * 60; // 1-minute sleep/QPC jump
        buf.push_at(960 + jump, Some(&pcm), 480);

        let stored = 480 * 2 * 2 * std::mem::size_of::<i16>(); // the two real packets only
        assert_eq!(buf.debug_heap_bytes().0, stored, "no zero padding for silence or gaps");
        assert_eq!(buf.end(), 960 + jump + 480, "timeline still advances across the gap");
    }

    #[test]
    fn gaps_keep_av_sync_in_mix_into() {
        let mut buf = AudioBuf::default();
        let pcm = vec![1000i16; 480 * 2];
        buf.push_at(480, Some(&pcm), 480);
        buf.push_at(960, None, 480);
        let jump = 48_000 * 60;
        let late = 960 + jump;
        buf.push_at(late, Some(&pcm), 480);

        // The packet after the gap must still land at its own timestamp, unmuted and unshifted.
        let mut out = vec![0f32; 480 * 2];
        buf.mix_into(&mut out, late, 1.0);
        assert!(out.iter().all(|&x| x == 1000.0));

        // The silent packet and the omitted gap mix as silence, not as stale audio.
        let mut hole = vec![0f32; 480 * 2];
        buf.mix_into(&mut hole, 960, 1.0);
        assert!(hole.iter().all(|&x| x == 0.0));

        // The snapshot keeps the late packet too (gap omitted, not padded).
        let mut snap = vec![0f32; 480 * 2];
        for b in buf.snapshot_blocks(late) {
            mix_slice(&mut snap, b.start, &b.samples, late, 1.0);
        }
        assert_eq!(snap, out);
    }
}
