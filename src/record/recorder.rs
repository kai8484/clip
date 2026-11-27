//! Recording thread: capture -> NV12 -> encoder -> RAM ring buffer; saves clips on demand.
//! Wakes at `fps` Hz using a high-resolution waitable timer.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

use super::audio::{self, Audio, AudioBuf, Source};
use super::capture::{Aim, Capture};
use super::clock::{now_100ns, Timer};
use super::convert::Converter;
use super::encoder::{Encoder, Packet};
use super::{game, mux};
use crate::config::{CaptureMode, Config};
use crate::log::log;
use crate::mf::{frames_at, Mf};
use crate::ui_kit::sound;

pub enum Cmd {
    Save,
    /// Config updates that do not require restarting video capture (length, audio, sound, folder).
    Update(Config),
    Stop,
}

/// Tray uses a subset of events; `devtools` modes print all of them.
#[cfg_attr(not(feature = "devtools"), allow(dead_code))]
pub enum Event {
    Started(String),
    Saved(std::path::PathBuf),
    SaveFailed(String),
    Error(String),
    /// Detected game executable changed (without `.exe`).
    Game(Option<String>),
}

pub type Notify = Arc<dyn Fn(Event) + Send + Sync>;

pub struct Recorder {
    tx: mpsc::Sender<Cmd>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start(cfg: Config, notify: Notify) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("clip-rec".into())
            .spawn(move || thread_main(cfg, rx, notify))
            .ok();
        Self { tx, handle }
    }

    pub fn save(&self) {
        let _ = self.tx.send(Cmd::Save);
    }

    pub fn update(&self, cfg: Config) {
        let _ = self.tx.send(Cmd::Update(cfg));
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

enum End {
    Stop,
    /// "Games only" mode with no active game: release capture & encoder until a game starts.
    Idle,
    /// Restart capturing this monitor (the detected game's monitor).
    Restart(HMONITOR),
}

struct WaitStop;

fn wait_for_game(cfg: &mut Config, rx: &Receiver<Cmd>, notify: &Notify) -> std::result::Result<Option<HMONITOR>, WaitStop> {
    let t0 = now_100ns();
    let mut game = game::Detector::default();
    game.poll(0);
    if game.pid().is_some() {
        return Ok(game.monitor());
    }
    notify(Event::Game(None));
    log("waiting for a game (GPU idle)");
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return Err(WaitStop),
            Ok(Cmd::Save) => {
                notify(Event::SaveFailed("No game is running right now".into()));
            }
            Ok(Cmd::Update(c)) => {
                *cfg = c;
                if cfg.capture_mode == CaptureMode::Screen {
                    return Ok(None);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = now_100ns() - t0;
                game.poll(now);
                if game.pid().is_some() {
                    return Ok(game.monitor());
                }
            }
        }
    }
}

fn thread_main(mut cfg: Config, rx: Receiver<Cmd>, notify: Notify) {
    let _mf = Mf::init();
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    let mut target = None;
    loop {
        if cfg.capture_mode == CaptureMode::GamesOnly && target.is_none() {
            match wait_for_game(&mut cfg, &rx, &notify) {
                Ok(m) => target = m,
                Err(WaitStop) => break,
            }
        }
        match session(&mut cfg, &rx, &notify, target.take()) {
            Ok(End::Stop) => break,
            Ok(End::Idle) => continue,
            Ok(End::Restart(m)) => target = Some(m),
            Err(e) => {
                log(&format!("capture error: {e:?}"));
                // Notify each retry failure; the tray shows a balloon only when entering error state.
                notify(Event::Error(e.to_string()));
                match rx.recv_timeout(Duration::from_secs(3)) {
                    Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                    Ok(Cmd::Update(c)) => cfg = c,
                    Ok(Cmd::Save) => notify(Event::SaveFailed("clip isn't recording right now".into())),
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }
}

/// Delay between triggering the save cue sound and its arrival in desktop loopback (100 ns);
/// muted from the desktop audio channel alongside the cue duration.
const SOUND_LATENCY: i64 = 2_050_000;

/// Retry interval for an audio source that failed to open (100 ns = 5 s).
const AUDIO_RETRY: i64 = 50_000_000;

/// Single audio channel (game, desktop, or microphone) with its own ring buffer.
#[derive(Default)]
struct Channel {
    audio: Option<Audio>,
    buf: AudioBuf,
    /// Sources that failed to open and the earliest timestamp to retry them.
    failed: Vec<(Source, i64)>,
}

impl Channel {
    /// Ensures `want` (or `fallback`) is open. Opens the new source before dropping the old one
    /// so a failed retry never cuts off currently recording audio.
    fn sync(&mut self, want: Option<Source>, fallback: Option<Source>, now: i64, name: &str) {
        let sources = [want, fallback];
        let sources = || sources.iter().flatten();
        if sources().next().is_none() {
            self.audio = None;
            self.buf = AudioBuf::default();
            return;
        }
        // Only retain failures for currently requested sources (don't accumulate old games).
        self.failed.retain(|(s, _)| sources().any(|w| w == s));
        for src in sources() {
            if self.audio.as_ref().is_some_and(|a| a.source == *src) {
                return;
            }
            if self.failed.iter().any(|(s, at)| s == src && now < *at) {
                continue;
            }
            match Audio::open(src.clone()) {
                Ok(a) => {
                    log(&format!("audio {name}: {src:?}"));
                    self.failed.retain(|(s, _)| s != src);
                    self.audio = Some(a);
                    return;
                }
                Err(e) => match self.failed.iter_mut().find(|(s, _)| s == src) {
                    Some(f) => f.1 = now + AUDIO_RETRY,
                    None => {
                        log(&format!("failed to open audio {name} ({src:?}): {e}"));
                        self.failed.push((src.clone(), now + AUDIO_RETRY));
                    }
                },
            }
        }
        self.audio = None;
    }

    fn poll(&mut self, t0: i64) {
        if let Some(a) = self.audio.as_mut() {
            if let Err(e) = a.poll(&mut self.buf, t0) {
                log(&format!("audio: {e}"));
                self.audio = None;
            }
        }
    }
}

struct AudioState {
    game_ch: Channel,
    desktop: Channel,
    mic: Channel,
    game: game::Detector,
    next_check: i64,
    /// Time windows (100 ns) where the save cue played, muted from desktop audio.
    masks: Vec<(i64, i64)>,
}

impl AudioState {
    fn sync(&mut self, cfg: &Config, now: i64) {
        if now >= self.next_check {
            self.next_check = now + 30_000_000;
            // Reopen system loopback if the default output device changed.
            if let Some(a) = &self.desktop.audio {
                if a.source == Source::System && a.device_id != audio::current_default_id() {
                    self.desktop.audio = None;
                }
            }
        }
        let pid = self.game.pid();
        let game = pid.filter(|_| cfg.game_audio).map(Source::Only);
        self.game_ch.sync(game, None, now, "game");
        self.mic.sync(cfg.mic_audio.then(|| Source::Mic(cfg.mic_device.clone())), None, now, "microphone");
        // Desktop = everything except the game; fall back to full system if exclusion fails.
        let (desk, fallback) = match pid {
            Some(p) => (Source::AllBut(p), Some(Source::System)),
            None => (Source::System, None),
        };
        let on = cfg.desktop_audio;
        self.desktop.sync(on.then_some(desk), fallback.filter(|_| on), now, "desktop");
    }
}

/// Monitor tracking interval (100 ns).
const AIM_EVERY: i64 = 2_500_000;

/// Delay after creating a `Converter` before logging the image brightness check (100 ns).
const PROBE_AFTER: i64 = 20_000_000;

fn scaled_size(src_w: u32, src_h: u32, max_h: u32) -> (u32, u32) {
    if max_h == 0 || src_h <= max_h {
        return ((src_w & !1).max(2), (src_h & !1).max(2));
    }
    let out_h = (max_h & !1).max(2);
    let out_w = ((src_w as f64 * out_h as f64 / src_h as f64).round() as u32 & !1).max(2);
    (out_w, out_h)
}

/// `restarted`: game monitor that triggered a restart (already opened at its resolution).
fn session(cfg: &mut Config, rx: &Receiver<Cmd>, notify: &Notify, restarted: Option<HMONITOR>) -> Result<End> {
    let fps = cfg.fps.max(1);
    let frame_dur = 10_000_000i64 / fps as i64;

    let mut game = game::Detector::default();
    game.poll(0);
    // Target monitor: game monitor, else active window monitor, else primary.
    let mut cap = Capture::new(restarted.or_else(|| game.monitor()).or_else(game::active_monitor))?;
    let (w, h) = scaled_size(cap.width, cap.height, cfg.max_height);
    let mut enc = Encoder::new(&cap.device, cap.luid, w, h, fps, cfg.bitrate_mbps * 1_000_000, cfg.codec)?;
    log(&format!(
        "recording {} at {w}x{h} @ {fps} fps, {} {} Mbps, buffer {} s, encoder: {}",
        cap.describe(),
        enc.codec_label(),
        cfg.bitrate_mbps,
        cfg.seconds,
        enc.name
    ));
    notify(Event::Started(enc.name.clone()));

    let mut conv: Option<Converter> = None;
    let timer = Timer::new();
    let t0 = now_100ns();
    let mut au = AudioState {
        game_ch: Channel::default(),
        desktop: Channel::default(),
        mic: Channel::default(),
        game,
        next_check: 0,
        masks: Vec::new(),
    };
    notify(Event::Game(au.game.name()));
    let mut game_monitor = restarted;
    let mut next_aim = 0i64;
    // When to run the image sanity check for the current monitor (once per `Converter`).
    let mut probe_at: Option<i64> = None;
    au.sync(cfg, 0);
    let mut next_frame = 0i64;
    let mut packets: VecDeque<Packet> = VecDeque::new();
    let mut out = Vec::new();

    loop {
        loop {
            match rx.try_recv() {
                Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => return Ok(End::Stop),
                Ok(Cmd::Save) => {
                    let now = now_100ns() - t0;
                    if save_clip(cfg, &packets, &enc, &au, now, frame_dur, notify) && cfg.sound_on_save {
                        sound::play_saved(cfg.sound_volume);
                        au.masks.push((now, now + sound::SAVED_LEN + SOUND_LATENCY));
                    }
                }
                Ok(Cmd::Update(c)) => {
                    *cfg = c;
                    au.sync(cfg, now_100ns() - t0);
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let keep = cfg.seconds as i64 * 10_000_000;

        let now = now_100ns() - t0;
        if au.game.poll(now) {
            let name = au.game.name();
            log(&format!("detected game: {}", name.as_deref().unwrap_or("none")));
            notify(Event::Game(name));
        }
        if cfg.capture_mode == CaptureMode::GamesOnly && au.game.pid().is_none() {
            log("no active game: pausing capture until a game starts");
            return Ok(End::Idle);
        }
        if now >= next_aim {
            next_aim = now + AIM_EVERY;
            // When the game appears or moves to another monitor, adopt that monitor's resolution.
            let gm = au.game.monitor();
            let native = gm.is_some() && gm != game_monitor;
            game_monitor = gm;
            if let Some(m) = gm.or_else(game::active_monitor) {
                if let Aim::Restart = cap.aim(m, native) {
                    return Ok(End::Restart(m));
                }
            }
        }
        cap.poll(now)?;
        if let Some(f) = &cap.latest {
            if !conv.as_ref().is_some_and(|c| c.is_for(f)) {
                conv = Some(Converter::new(&cap.device, &cap.context, f, w, h, fps, 8)?);
                probe_at = Some(now + PROBE_AFTER);
            }
        }
        au.sync(cfg, now);
        au.game_ch.poll(t0);
        au.desktop.poll(t0);
        au.mic.poll(t0);

        enc.poll(&mut out)?;
        packets.extend(out.drain(..));

        if now >= next_frame {
            let pts = (now / frame_dur) * frame_dur;
            let visible = cfg.capture_mode == CaptureMode::Screen || au.game.visible();
            if let Some(c) = conv.as_mut() {
                // Only convert if the encoder requested input; skip frame if encoder is busy.
                if enc.pending_input > 0 {
                    let tex = if visible { c.convert(cap.frame_seq)? } else { c.black() };
                    if visible && probe_at.is_some_and(|t| now >= t) {
                        probe_at = None;
                        log(&c.probe(&tex));
                    }
                    enc.feed(&tex, pts, frame_dur)?;
                }
            }
            next_frame = pts + frame_dur;
        }

        trim_video(&mut packets, now - keep);
        let a_min = frames_at(now - keep - 20_000_000);
        au.game_ch.buf.trim_before(a_min);
        au.desktop.buf.trim_before(a_min);
        au.mic.buf.trim_before(a_min);
        au.masks.retain(|m| m.1 > now - keep - 20_000_000);

        timer.sleep(next_frame - (now_100ns() - t0));
    }
}

/// Drops complete GOPs outside the retention window.
fn trim_video(p: &mut VecDeque<Packet>, min_pts: i64) {
    while p.front().is_some_and(|f| !f.key) {
        p.pop_front();
    }
    while let Some(second) = p.iter().skip(1).position(|x| x.key).map(|i| i + 1) {
        if p[second].pts > min_pts {
            break;
        }
        p.drain(..second);
    }
}

/// Mixer settings copied for the save thread (volume 0 = channel off).
#[derive(Clone, Copy)]
struct MixSettings {
    game: f32,
    mic: f32,
    desktop: f32,
}

impl MixSettings {
    fn new(cfg: &Config) -> Self {
        Self {
            game: if cfg.game_audio { cfg.game_volume as f32 / 100.0 } else { 0.0 },
            mic: if cfg.mic_audio { cfg.mic_volume as f32 / 100.0 } else { 0.0 },
            desktop: if cfg.desktop_audio { cfg.desktop_volume as f32 / 100.0 } else { 0.0 },
        }
    }
}

/// Contiguous copy of an `AudioBuf` window, so mixing on the mux thread never touches the live
/// ring buffer that the capture thread keeps writing to.
struct AudioWindow {
    /// Frame index (from `t0`) of `data[0]`.
    start: i64,
    data: Vec<i16>,
}

impl AudioWindow {
    /// Copies the part of `buf` at or after frame `from`.
    fn snapshot(buf: &AudioBuf, from: i64) -> Self {
        let total = buf.data.len() / 2;
        let skip = ((from - buf.start).max(0) as usize).min(total);
        let mut data = Vec::with_capacity((total - skip) * 2);
        let mut drop_frames = skip;
        let (a, b) = buf.data.as_slices();
        for part in [a, b] {
            if drop_frames * 2 >= part.len() {
                drop_frames -= part.len() / 2;
                continue;
            }
            data.extend_from_slice(&part[drop_frames * 2..]);
            drop_frames = 0;
        }
        Self { start: buf.start + skip as i64, data }
    }

    /// Same contract as `AudioBuf::mix_into`, reading the snapshot instead of the live ring.
    fn mix_into(&self, out: &mut [f32], from: i64, gain: f32) {
        let skip = (((self.start - from).max(0) * 2) as usize).min(out.len());
        let out = &mut out[skip..];
        let first = ((from - self.start).max(0) * 2) as usize;
        let Some(src) = self.data.get(first..) else { return };
        for (o, &s) in out.iter_mut().zip(src) {
            *o += s as f32 * gain;
        }
    }
}

/// Everything a save needs from the audio pipeline, snapshotted on the capture thread so that
/// mixing and muxing run on the save thread.
struct AudioSnapshot {
    game: AudioWindow,
    mic: AudioWindow,
    desktop: AudioWindow,
    /// Time windows (100 ns) where the save cue played, muted from desktop audio.
    masks: Vec<(i64, i64)>,
    settings: MixSettings,
}

impl AudioSnapshot {
    fn capture(cfg: &Config, au: &AudioState, from: i64) -> Self {
        Self {
            game: AudioWindow::snapshot(&au.game_ch.buf, from),
            mic: AudioWindow::snapshot(&au.mic.buf, from),
            desktop: AudioWindow::snapshot(&au.desktop.buf, from),
            masks: au.masks.clone(),
            settings: MixSettings::new(cfg),
        }
    }
}

/// Mixes game, microphone, and desktop audio with configured volumes (masking out the save cue
/// on the desktop channel). Runs in short blocks on the save thread: a first pass finds the peak,
/// a second writes straight into one `Vec<i16>`, so no clip-length float buffer is ever kept.
fn mixed_audio(snap: &AudioSnapshot, from: i64, to: i64) -> Vec<i16> {
    const CHUNK: usize = 4096; // frames per block
    let n = (to - from).max(0) as usize;
    // Intervals on the desktop track where the save cue did not play (masks are already sorted).
    let frame = |t: i64| ((frames_at(t) - from).max(0) as usize).min(n);
    let mut desktop = Vec::new();
    let mut at = 0;
    for (a, b) in snap.masks.iter().map(|&(a, b)| (frame(a), frame(b))).chain([(n, n)]) {
        if a > at {
            desktop.push((at, a));
        }
        at = at.max(b);
    }

    // Pass 1: peak of the mixed signal. If overall audio is quiet (e.g., low in-game volume
    // compensated by headphones), the clip is auto-boosted with one uniform gain (capped at +30 dB
    // so true silence doesn't amplify noise).
    let mut buf = vec![0f32; CHUNK * 2];
    let mut peak = 0f32;
    for c0 in (0..n).step_by(CHUNK) {
        let c1 = (c0 + CHUNK).min(n);
        let mix = &mut buf[..(c1 - c0) * 2];
        mix.fill(0.0);
        mix_block(snap, &desktop, from + c0 as i64, c0, c1, mix);
        peak = mix.iter().fold(peak, |m, &x| m.max(x.abs()));
    }
    // Threshold & target on i16 scale: 4096 = 32/255 (-18 dBFS), 23118 = 180/255.
    let gain = if peak >= 4096.0 || peak == 0.0 { 1.0 } else { (23118.0 / peak).min(32.0) };
    if gain > 1.0 {
        log(&format!(
            "quiet audio (peak {:.0}/255): boosting clip ×{gain:.1}",
            peak / 128.0
        ));
    }

    // Pass 2: write the final i16 samples directly, one block at a time.
    let mut out = Vec::with_capacity(n * 2);
    for c0 in (0..n).step_by(CHUNK) {
        let c1 = (c0 + CHUNK).min(n);
        let mix = &mut buf[..(c1 - c0) * 2];
        mix.fill(0.0);
        mix_block(snap, &desktop, from + c0 as i64, c0, c1, mix);
        out.extend(mix.iter().map(|x| (x * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16));
    }
    out
}

/// Mixes one block of `[c0, c1)` frames starting at absolute frame `f` into `mix`.
fn mix_block(snap: &AudioSnapshot, desktop: &[(usize, usize)], f: i64, c0: usize, c1: usize, mix: &mut [f32]) {
    let s = snap.settings;
    if s.game > 0.0 {
        snap.game.mix_into(mix, f, s.game);
    }
    if s.mic > 0.0 {
        snap.mic.mix_into(mix, f, s.mic);
    }
    if s.desktop > 0.0 {
        for &(a, b) in desktop {
            let (a, b) = (a.max(c0), b.min(c1));
            if a < b {
                snap.desktop.mix_into(&mut mix[(a - c0) * 2..(b - c0) * 2], f + (a - c0) as i64, s.desktop);
            }
        }
    }
}

/// Number of `_N` suffixes tried before falling back to a PID-tagged name.
const CLAIM_TRIES: u32 = 10_000;

/// Monotonic suffix for the last-resort name, so two simultaneous saves can't pick the same one.
static FALLBACK_SEQ: AtomicU32 = AtomicU32::new(1);

/// Picks `clip_<stamp>.mp4` (collision: `clip_<stamp>_<n>.mp4`) and atomically claims it with
/// `create_new`, which refuses an existing file and a dangling symlink alike — no check-then-write
/// window that a swapped-in link could exploit. Runs on the save thread, never on capture.
fn claim_clip_path(dir: &Path, stamp: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    reject_reparse(dir)?;
    claim_in(dir, stamp, CLAIM_TRIES)
}

fn claim_in(dir: &Path, stamp: &str, tries: u32) -> std::io::Result<PathBuf> {
    let create = |name: String| -> std::io::Result<Option<PathBuf>> {
        let p = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            Ok(_) => Ok(Some(p)),
            // `create_new` refuses an existing file or dangling symlink (EXISTS) and an existing
            // directory (ACCESS_DENIED on Windows); both just mean the name is taken.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(_e) if p.symlink_metadata().is_ok() => Ok(None),
            Err(e) => Err(e),
        }
    };
    for i in 1..=tries {
        let name = if i == 1 { format!("clip_{stamp}.mp4") } else { format!("clip_{stamp}_{i}.mp4") };
        if let Some(p) = create(name)? {
            return Ok(p);
        }
    }
    // Bounded last resort: a PID-tagged name always terminates, even if every suffix is taken.
    let pid = std::process::id();
    let base = FALLBACK_SEQ.load(Ordering::Relaxed);
    for n in 0..tries.max(1) {
        let seq = base.wrapping_add(n);
        if let Some(p) = create(format!("clip_{stamp}_{pid}_{seq}.mp4"))? {
            FALLBACK_SEQ.store(seq.wrapping_add(1), Ordering::Relaxed);
            return Ok(p);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free clip name in {}", dir.display()),
    ))
}

/// Refuses a clips folder that is a symlink/junction, so a swapped-in link can't redirect writes.
fn reject_reparse(dir: &Path) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(dir)?;
    if !md.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a folder", dir.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing to save clips into a linked folder: {}", dir.display()),
            ));
        }
    }
    #[cfg(not(windows))]
    if md.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing to save clips into a linked folder: {}", dir.display()),
        ));
    }
    Ok(())
}

/// Snapshots the clip job and hands the expensive part (audio mixing, naming, muxing) to the save
/// thread, so the real-time capture loop only copies packets and PCM windows. Returns `false` if
/// the buffer holds no clip yet.
fn save_clip(cfg: &Config, packets: &VecDeque<Packet>, enc: &Encoder, au: &AudioState, now: i64, frame_dur: i64, notify: &Notify) -> bool {
    let keep = cfg.seconds as i64 * 10_000_000;
    let fail = |m: &str| {
        notify(Event::SaveFailed(m.into()));
        false
    };
    let Some(video_type) = enc.type_blob() else { return fail("There's no video in the buffer yet") };
    let min = now - keep;
    let start = packets
        .iter()
        .rposition(|p| p.key && p.pts <= min)
        .or_else(|| packets.iter().position(|p| p.key));
    let Some(start) = start else { return fail("There's no video in the buffer yet") };
    let pk: Vec<Packet> = packets.iter().skip(start).cloned().collect();
    log(&format!("clip: {} frames, {} keyframes", pk.len(), pk.iter().filter(|p| p.key).count()));
    let base = pk[0].pts;
    let end = pk.last().map_or(base, |p| p.pts) + frame_dur;
    // Cheap snapshots only: packet list, PCM windows, mixer settings, cue masks.
    let audio = cfg.audio_on().then(|| AudioSnapshot::capture(cfg, au, frames_at(base)));
    if au.game.pid().is_none() {
        if cfg.game_audio {
            log("clip has no game audio: no game was detected");
        }
        if cfg.capture_mode == CaptureMode::GamesOnly {
            log("black clip: \"Games only\" mode did not detect a fullscreen game");
        }
    }

    let dir = cfg.output_dir.clone();
    let stamp = crate::log::file_stamp();
    let notify = notify.clone();
    let spawn = std::thread::Builder::new().name("clip-save".into()).spawn(move || {
        let path = match claim_clip_path(&dir, &stamp) {
            Ok(p) => p,
            Err(e) => {
                log(&format!("failed to save clip: {e:?}"));
                notify(Event::SaveFailed(e.to_string()));
                return;
            }
        };
        let audio = audio.map(|a| mixed_audio(&a, frames_at(base), frames_at(end)));
        match mux::write(mux::Job { path, video_type, packets: pk, base, frame_dur, audio }) {
            Ok(p) => {
                log(&format!("clip saved: {}", p.display()));
                notify(Event::Saved(p));
            }
            Err(e) => {
                log(&format!("failed to save clip: {e:?}"));
                notify(Event::SaveFailed(e.to_string()));
            }
        }
    });
    match spawn {
        Ok(_) => true,
        Err(e) => fail(&format!("couldn't start the save thread: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("clip_test_{}_{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn clip_names_use_date_and_time_and_are_claimed_atomically() {
        let tmp = TempDir::new("stamp");
        let dir = &tmp.0;
        let stamp = "2026-11-20_21-20-04";

        let p1 = claim_clip_path(dir, stamp).unwrap();
        assert_eq!(p1, dir.join(format!("clip_{stamp}.mp4")));
        assert!(p1.exists(), "the claimed path must exist so no second writer can take it");

        // Same second: the next save gets `_2` instead of overwriting the first clip.
        let p2 = claim_clip_path(dir, stamp).unwrap();
        assert_eq!(p2, dir.join(format!("clip_{stamp}_2.mp4")));
        assert_eq!(claim_in(dir, stamp, 3).unwrap(), dir.join(format!("clip_{stamp}_3.mp4")));

        // A missing output folder is created on demand.
        let missing = dir.join("nested").join("clips");
        assert_eq!(claim_clip_path(&missing, stamp).unwrap(), missing.join(format!("clip_{stamp}.mp4")));
    }

    #[test]
    fn claim_skips_taken_names_and_stops_at_the_attempt_bound() {
        let tmp = TempDir::new("bound");
        let dir = &tmp.0;
        let stamp = "2026-01-02_03-04-05";
        let pid = std::process::id();

        // A stale placeholder from a crashed save is never overwritten.
        std::fs::write(dir.join(format!("clip_{stamp}.mp4")), b"stale").unwrap();
        let p = claim_in(dir, stamp, 1).unwrap();
        assert!(p.exists());
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(&format!("clip_{stamp}_{pid}_")), "expected the bounded fallback, got {name}");

        // A directory squatting on a candidate name counts as taken, not as a fatal error.
        std::fs::create_dir_all(dir.join(format!("clip_{stamp}_2.mp4"))).unwrap();
        let p = claim_in(dir, stamp, 2).unwrap();
        assert!(p.exists());
        assert_ne!(p, dir.join(format!("clip_{stamp}_2.mp4")));
    }

    #[test]
    fn dangling_symlink_destination_is_not_followed() {
        let tmp = TempDir::new("symlink");
        let dir = &tmp.0;
        let stamp = "2026-02-03_04-05-06";
        let link = dir.join(format!("clip_{stamp}.mp4"));
        let target = dir.join("elsewhere.mp4");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&target, &link).is_ok();
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(&target, &link).is_ok();
        if !made {
            return; // creating symlinks needs Developer Mode or admin on Windows
        }

        let p = claim_clip_path(dir, stamp).unwrap();
        assert_eq!(p, dir.join(format!("clip_{stamp}_2.mp4")));
        assert!(!target.exists(), "create_new must not follow a dangling symlink");
    }

    #[test]
    fn audio_window_snapshot_matches_the_ring_buffer() {
        let mut buf = AudioBuf::default();
        let frames = 10_000usize;
        let pcm: Vec<i16> = (0..frames * 2).map(|i| i as i16).collect();
        buf.push_at(100, Some(&pcm), frames);

        let from = 1_234;
        let snap = AudioWindow::snapshot(&buf, from);
        assert_eq!(snap.start, from);
        assert_eq!(snap.data.len(), (frames as i64 - from + 100) as usize * 2);

        let mut want = vec![0f32; 32];
        let mut got = vec![0f32; 32];
        buf.mix_into(&mut want, from + 7, 0.5);
        snap.mix_into(&mut got, from + 7, 0.5);
        assert_eq!(want, got);

        // A window starting after the buffer's end is empty, not a panic.
        let snap = AudioWindow::snapshot(&buf, 999_999);
        assert!(snap.data.is_empty());
    }
}
