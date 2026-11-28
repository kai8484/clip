#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentThreadId, OpenEventW, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetMessageW, GetSystemMetrics, GetWindowRect, IsIconic, IsZoomed, LoadImageW,
    PeekMessageW, PostThreadMessageW, SendMessageW, SetClassLongPtrW, SetForegroundWindow,
    SetWindowPos, ShowWindow, GCLP_HICON, GCLP_HICONSM, ICON_BIG, ICON_SMALL, IMAGE_ICON,
    LR_SHARED, MSG, NCCALCSIZE_PARAMS, PM_NOREMOVE, SM_CXICON, SM_CXSMICON, SM_CYICON,
    SM_CYSMICON, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SW_RESTORE, SW_SHOW, WM_APP, WM_HOTKEY, WM_NCCALCSIZE, WM_NCDESTROY, WM_QUIT, WM_SETICON,
};

use clip::app::autostart;
use clip::app::install;
use clip::app::keys::{self, hotkey_name};
use clip::app::mode::{Window, TRAY_MUTEX};
use clip::app::update;
use clip::config::{self, CaptureMode, Config, GlobalAction, VideoCodec};
use clip::library;
use clip::log::{app_dir, exe_dir, log};
use clip::record::audio;
use clip::record::recorder::{Event, Notify, Recorder};
use clip::share::{
    self, PurgeR2Result, R2Friend, R2Stats, RevokeInviteResult, ShareMeta, ShareRecord, ShareResult,
    ShareStore,
};

use clip::ui_kit::sound;
use clip::ui_kit::theme::fmt_secs;
use clip::ui_kit::window::{find_own_window, open_folder as open_explorer};

const WM_HOTKEY_RELOAD: u32 = WM_APP + 1;
const WM_HOTKEY_SUSPEND: u32 = WM_APP + 2;
const SUBCLASS_FRAMELESS_ID: usize = 1;
/// Signalled by a second instance so this one reopens its on-demand window.
const SHOW_EVENT: PCWSTR = windows::core::w!("Local\\clip.Show");

// On Windows 11, tao leaves a 1 px top inset in WM_NCCALCSIZE for undecorated windows with shadows,
// causing DWM to draw a gray non-client hairline (#202020). Restore the original top edge when not
// maximized so the client area covers from y=0.
unsafe extern "system" fn frameless_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    uid: usize,
    _ref_data: usize,
) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if lparam.0 != 0 => {
            let rect_ptr = if wparam.0 != 0 {
                &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0] as *mut RECT
            } else {
                lparam.0 as *mut RECT
            };
            let orig_top = (*rect_ptr).top;
            let res = DefSubclassProc(hwnd, msg, wparam, lparam);
            if !IsZoomed(hwnd).as_bool() {
                (*rect_ptr).top = orig_top;
            }
            res
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(frameless_subclass_proc), uid);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

#[derive(Clone, Serialize)]
struct RecorderState {
    running: bool,
    enabled: bool,
    idle_waiting: bool,
    seconds: u32,
    ram_mb: u32,
    clips_count: usize,
    hotkey_label: String,
    game: Option<String>,
    error: Option<String>,
    encoder: Option<String>,
    autostart: bool,
    app_dir: String,
    version: String,
}

#[derive(Clone, Serialize)]
struct MicDevice {
    id: String,
    name: String,
}

#[derive(Clone, Serialize)]
struct RecorderToast {
    kind: String,
    title: String,
    message: String,
}

#[derive(Clone, Serialize)]
struct HotkeyCapturedPayload {
    cancelled: bool,
    vk: Option<u32>,
    mods: Option<u32>,
    label: Option<String>,
    note: Option<String>,
}

/// Redacted view of [`Config`] handed to the webview. The upload key never crosses IPC (or the
/// `config-changed` event); the UI only learns whether one is configured.
#[derive(Clone, Serialize)]
struct FrontendConfig {
    enabled: bool,
    output_dir: PathBuf,
    seconds: u32,
    hotkey_vk: u32,
    hotkey_mods: u32,
    fps: u32,
    bitrate_mbps: u32,
    codec: VideoCodec,
    max_height: u32,
    game_audio: bool,
    game_volume: u32,
    desktop_audio: bool,
    desktop_volume: u32,
    mic_audio: bool,
    mic_volume: u32,
    mic_device: String,
    capture_mode: CaptureMode,
    sound_on_save: bool,
    sound_volume: u32,
    toggle_vk: u32,
    toggle_mods: u32,
    folder_vk: u32,
    folder_mods: u32,
    share_url: String,
    has_share_key: bool,
}

impl FrontendConfig {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.enabled,
            output_dir: cfg.output_dir.clone(),
            seconds: cfg.seconds,
            hotkey_vk: cfg.hotkey_vk,
            hotkey_mods: cfg.hotkey_mods,
            fps: cfg.fps,
            bitrate_mbps: cfg.bitrate_mbps,
            codec: cfg.codec,
            max_height: cfg.max_height,
            game_audio: cfg.game_audio,
            game_volume: cfg.game_volume,
            desktop_audio: cfg.desktop_audio,
            desktop_volume: cfg.desktop_volume,
            mic_audio: cfg.mic_audio,
            mic_volume: cfg.mic_volume,
            mic_device: cfg.mic_device.clone(),
            capture_mode: cfg.capture_mode,
            sound_on_save: cfg.sound_on_save,
            sound_volume: cfg.sound_volume,
            toggle_vk: cfg.toggle_vk,
            toggle_mods: cfg.toggle_mods,
            folder_vk: cfg.folder_vk,
            folder_mods: cfg.folder_mods,
            share_url: cfg.share_url.clone(),
            has_share_key: !cfg.share_key.trim().is_empty(),
        }
    }
}

#[derive(Clone, Serialize)]
struct InstallResponse {
    relaunched: bool,
    install_dir: String,
}

#[derive(Clone, Serialize)]
struct SetupState {
    needs_setup: bool,
    default_install_dir: String,
    current_exe_dir: String,
}

struct CoreState {
    cfg: Config,
    running: bool,
    rec: Option<Recorder>,
    error: Option<String>,
    game: Option<String>,
    encoder: Option<String>,
    /// Cached clip count so state snapshots never touch the disk under the mutex.
    clips_count: usize,
    /// Cached autostart flag so snapshots never read the registry under the mutex.
    autostart: bool,
    thumbs: ThumbStore,
    /// Uploads remembered across restarts so an unchanged clip is never shared twice.
    shares: ShareStore,
    /// In-flight uploads: one leader per clip path, everyone else waits for its result.
    uploads: UploadTracker,
    hotkey_tid: u32,
    hotkeys_suspended: bool,
    capture_cancel: Option<Arc<AtomicBool>>,
    tray: Option<TrayIcon>,
    menu_toggle: Option<CheckMenuItem<tauri::Wry>>,
    menu_mic: Option<CheckMenuItem<tauri::Wry>>,
    menu_save: Option<MenuItem<tauri::Wry>>,
    ev_tx: mpsc::Sender<Event>,
    height_anim_id: u64,
    /// First run: no `data\config.json` yet, so nothing may be written before the user installs.
    needs_setup: bool,
    /// Raw `Local\clip.Main` mutex handle, released before handing off to the installed copy.
    instance_mutex: Option<usize>,
    /// Set before `app.exit(0)` so `RunEvent::ExitRequested` stops holding the tray alive.
    exiting: Arc<AtomicBool>,
    /// In-app update checking: status, HTTP validators and the background checker's wake signal.
    updates: UpdateManagerState,
}

/// Update state shared by the background checker, the commands and the webview.
struct UpdateManagerState {
    /// Latest status shown in Settings and on the titlebar pill.
    status: update::UpdateStatus,
    /// HTTP validator from the last release check, so polling stays a cheap `304`.
    etag: Option<String>,
    /// Verified release waiting for `apply_update`.
    info: Option<update::UpdateInfo>,
    /// A check or download is in flight; a second one is never started.
    busy: bool,
    /// `(stopped, wake)`: the checker waits out the interval, but shutdown wakes it at once.
    shutdown: Arc<(Mutex<bool>, Condvar)>,
}

impl UpdateManagerState {
    fn new() -> Self {
        Self {
            status: update::UpdateStatus::default(),
            etag: None,
            info: None,
            busy: false,
            shutdown: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }
}

#[derive(Clone)]
struct AppShared(Arc<Mutex<CoreState>>);

fn tray_status_icon() -> Image<'static> {
    Image::from_bytes(include_bytes!("../../assets/icon/tray32.png")).unwrap().to_owned()
}

fn tray_tooltip(cfg: &Config, running: bool, error: Option<&str>, game: Option<&str>) -> String {
    if !running {
        return "clip · closed".into();
    }
    if let Some(e) = error {
        return format!("clip · error: {e}");
    }
    if cfg.enabled {
        if cfg.capture_mode == CaptureMode::GamesOnly && game.is_none() {
            return format!(
                "clip · waiting for game · {} saves {}",
                hotkey_name(cfg.hotkey_vk, cfg.hotkey_mods),
                fmt_secs(cfg.seconds)
            );
        }
        let mut s = format!(
            "clip · recording · {} saves {}",
            hotkey_name(cfg.hotkey_vk, cfg.hotkey_mods),
            fmt_secs(cfg.seconds)
        );
        if let Some(g) = game {
            s.push_str(&format!(" · game: {g}"));
        }
        s
    } else {
        "clip · paused".into()
    }
}

#[derive(Clone, Serialize)]
struct ClipEntry {
    path: String,
    file_name: String,
    stem: String,
    ext: String,
    display_name: String,
    size_bytes: u64,
    modified_ms: u64,
    duration_ms: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    codec: Option<String>,
    uploaded_url: Option<String>,
    uploading: bool,
}

#[derive(Clone, Serialize)]
struct ClipPreview {
    path: String,
    /// `http://thumb.localhost/?path=…` served by the `thumb` URI scheme protocol.
    thumb_url: Option<String>,
    duration_ms: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    codec: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
struct DeleteClipFailure {
    path: String,
    name: String,
    error: String,
}

#[derive(Clone, Serialize)]
struct DeleteClipsResult {
    deleted: Vec<String>,
    failed: Vec<DeleteClipFailure>,
}

#[derive(Clone, Serialize)]
struct ShareClipResult {
    url: String,
    video_url: String,
    slug: String,
    copied: bool,
    already_uploaded: bool,
}

#[derive(Clone, Serialize)]
struct ClipUploadStateEvent {
    path: String,
    uploading: bool,
    uploaded_url: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ThumbMeta {
    v: u32,
    size: u64,
    modified_ms: u64,
    duration_ms: u64,
    width: u32,
    height: u32,
    codec: String,
    /// `false` when no cover frame could be decoded: the gallery then skips the `thumb` request.
    #[serde(default = "default_true")]
    has_thumb: bool,
}

fn default_true() -> bool {
    true
}

const THUMB_CACHE_VERSION: u32 = 1;
const THUMB_WIDTH: u32 = 320;
const THUMB_UNUSED_FOR: Duration = Duration::from_secs(60 * 24 * 60 * 60);
/// Only bump the cache file's mtime when it has not been used for this long: keeps reads free of
/// write-on-read churn while still protecting hot files from the prune sweep.
const THUMB_TOUCH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

fn is_clip_name(name: &str) -> bool {
    !name.contains("clip-tmp") && !name.contains("clippy-tmp") && library::is_video(Path::new(name))
}

fn count_clips(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_file())
                && e.file_name().to_str().is_some_and(is_clip_name)
        })
        .count()
}

fn thumb_cache_dir() -> PathBuf {
    app_dir().join("cache").join("thumbs")
}

fn thumb_cache_key(p: &Path) -> String {
    // Callers pass canonical paths (the resolved clip or a canonical scan dir), so this stays
    // disk-free: no per-file `canonicalize` during gallery scans.
    let h = p
        .to_string_lossy()
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |acc, b| {
            (acc ^ b as u64).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{h:016x}")
}

fn thumb_cache_paths(p: &Path) -> (PathBuf, PathBuf) {
    let dir = thumb_cache_dir();
    let key = thumb_cache_key(p);
    (dir.join(format!("{key}.json")), dir.join(format!("{key}.png")))
}

fn remove_thumb_cache(p: &Path) {
    let (meta_p, png_p) = thumb_cache_paths(p);
    let _ = std::fs::remove_file(meta_p);
    let _ = std::fs::remove_file(png_p);
}

fn migrate_thumb_cache(old_meta: &Path, old_png: &Path, to: &Path) {
    let (new_meta, new_png) = thumb_cache_paths(to);
    if old_meta == new_meta {
        return;
    }
    if old_meta.exists() {
        let _ = std::fs::remove_file(&new_meta);
        if std::fs::rename(old_meta, &new_meta).is_err() {
            let _ = std::fs::remove_file(old_meta);
        }
    }
    if old_png.exists() {
        let _ = std::fs::remove_file(&new_png);
        if std::fs::rename(old_png, &new_png).is_err() {
            let _ = std::fs::remove_file(old_png);
        }
    }
}

fn prune_thumb_cache() {
    let Some(limit) = SystemTime::now().checked_sub(THUMB_UNUSED_FOR) else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(thumb_cache_dir()) else {
        return;
    };
    for e in rd.flatten() {
        if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < limit) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn modified_ms_of(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn read_thumb_meta(p: &Path, size: u64, modified_ms: u64) -> Option<ThumbMeta> {
    let (meta_p, _) = thumb_cache_paths(p);
    let raw = std::fs::read_to_string(meta_p).ok()?;
    let m: ThumbMeta = serde_json::from_str(&raw).ok()?;
    (m.v == THUMB_CACHE_VERSION && m.size == size && m.modified_ms == modified_ms).then_some(m)
}

/// In-memory index of the thumbnail cache. Repeated gallery scans resolve metadata from here
/// instead of re-opening one JSON file per clip; misses fall back to disk once.
#[derive(Clone, Default)]
struct ThumbStore(Arc<Mutex<HashMap<String, ThumbMeta>>>);

impl ThumbStore {
    fn cached(&self, p: &Path, size: u64, modified_ms: u64) -> Option<ThumbMeta> {
        let key = thumb_cache_key(p);
        {
            let map = self.0.lock().unwrap();
            if let Some(m) = map.get(&key) {
                if m.v == THUMB_CACHE_VERSION && m.size == size && m.modified_ms == modified_ms {
                    return Some(m.clone());
                }
            }
        }
        let meta = read_thumb_meta(p, size, modified_ms)?;
        self.0.lock().unwrap().insert(key, meta.clone());
        Some(meta)
    }

    fn remember(&self, p: &Path, meta: &ThumbMeta) {
        self.0.lock().unwrap().insert(thumb_cache_key(p), meta.clone());
    }

    fn forget(&self, p: &Path) {
        self.0.lock().unwrap().remove(&thumb_cache_key(p));
    }
}

/// Tracker key: verbatim prefixes stripped and case folded, matching `ShareStore`'s own keys.
fn upload_key(path: &Path) -> String {
    strip_verbatim(path.to_path_buf()).to_string_lossy().to_lowercase()
}

type UploadSlots = Arc<Mutex<HashMap<String, Arc<UploadSlot>>>>;

/// Single-flight uploads: the first caller for a clip uploads, later ones wait for its result.
#[derive(Clone, Default)]
struct UploadTracker(UploadSlots, Arc<Condvar>);

#[derive(Default)]
struct UploadSlot {
    done: AtomicBool,
}

enum UploadRole {
    Leader(UploadGuard),
    Follower(Arc<UploadSlot>),
}

/// RAII leader marker: dropping it always frees the slot and wakes every follower, including on
/// an early `?` return, an error path or a panic.
struct UploadGuard {
    tracker: UploadTracker,
    key: String,
    slot: Arc<UploadSlot>,
}

impl Drop for UploadGuard {
    fn drop(&mut self) {
        // Flag first, then unlink: a follower that grabs the lock in between sees `done` and
        // returns instead of waiting for an upload that already ended.
        self.slot.done.store(true, Ordering::SeqCst);
        {
            let mut slots = self
                .tracker
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots.get(&self.key).is_some_and(|s| Arc::ptr_eq(s, &self.slot)) {
                slots.remove(&self.key);
            }
        }
        self.tracker.1.notify_all();
    }
}

impl UploadTracker {
    fn begin(&self, key: String) -> UploadRole {
        let mut slots = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = slots.get(&key) {
            return UploadRole::Follower(slot.clone());
        }
        let slot = Arc::new(UploadSlot::default());
        slots.insert(key.clone(), slot.clone());
        UploadRole::Leader(UploadGuard { tracker: self.clone(), key, slot })
    }

    /// Blocks until the leader holding `slot` released the tracker.
    fn wait_for(&self, slot: &Arc<UploadSlot>) {
        let mut slots = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while !slot.done.load(Ordering::SeqCst) {
            slots = self
                .1
                .wait(slots)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn is_uploading(&self, path: &Path) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&upload_key(path))
    }
}

fn encode_png_bytes(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    // Thumbnails are disposable and re-generated on demand: speed beats a few KB.
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    drop(writer);
    Ok(out)
}

fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for b in raw.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let lo = (*bytes.get(i + 2)? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// URL handed to `<img src>`: on Windows/WebView2 Tauri serves URI schemes at
/// `http://<scheme>.localhost/`. The `v` parameter busts the webview cache when a clip changes.
fn clip_thumb_url(path: &Path, modified_ms: u64) -> String {
    format!(
        "http://thumb.localhost/?path={}&v={modified_ms}",
        percent_encode(&path.display().to_string())
    )
}

fn thumb_arg(query: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("path="))
        .and_then(percent_decode)
}

fn http_response(status: u16, content_type: &'static str, body: Vec<u8>) -> tauri::http::Response<Vec<u8>> {
    let mut res = tauri::http::Response::new(body);
    *res.status_mut() = tauri::http::StatusCode::from_u16(status)
        .unwrap_or(tauri::http::StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(value) = tauri::http::HeaderValue::from_str(content_type) {
        res.headers_mut().insert("content-type", value);
    }
    // Thumbnail URLs carry the clip's mtime, so a hit can be cached for good; errors must not be.
    let cache = if status == 200 {
        "private, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    res.headers_mut()
        .insert("cache-control", tauri::http::HeaderValue::from_static(cache));
    res
}

fn touch_thumb_if_stale(png: &Path) {
    let Ok(modified) = std::fs::metadata(png).and_then(|m| m.modified()) else {
        return;
    };
    let stale = SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > THUMB_TOUCH_AFTER);
    if !stale {
        return;
    }
    if let Ok(f) = std::fs::File::options().write(true).open(png) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Returns the cached (or freshly generated) PNG plus clip metadata. Metadata is cached even when
/// no cover frame could be decoded, so the gallery still shows duration and resolution.
fn read_clip_thumb(clip: &Path, thumbs: &ThumbStore) -> Result<(Option<Vec<u8>>, ThumbMeta), String> {
    let meta = std::fs::metadata(clip).map_err(|e| e.to_string())?;
    let size = meta.len();
    let modified_ms = modified_ms_of(&meta);
    let (meta_p, png_p) = thumb_cache_paths(clip);

    if let Some(cached) = thumbs.cached(clip, size, modified_ms) {
        if !cached.has_thumb {
            return Ok((None, cached));
        }
        if let Ok(bytes) = std::fs::read(&png_p) {
            touch_thumb_if_stale(&png_p);
            return Ok((Some(bytes), cached));
        }
    }

    // `probe_and_cover` initializes Media Foundation itself.
    let info = library::probe_and_cover(clip, THUMB_WIDTH)?;
    let png = info
        .cover_rgb
        .and_then(|(w, h, rgb)| encode_png_bytes(w, h, &rgb).ok());
    let cached = ThumbMeta {
        v: THUMB_CACHE_VERSION,
        size,
        modified_ms,
        duration_ms: info.duration_ms,
        width: info.width,
        height: info.height,
        codec: info.codec.to_string(),
        has_thumb: png.is_some(),
    };

    let _ = std::fs::create_dir_all(thumb_cache_dir());
    let _ = std::fs::write(&meta_p, serde_json::to_string(&cached).unwrap_or_default());
    if let Some(bytes) = &png {
        let _ = std::fs::write(&png_p, bytes);
    }
    thumbs.remember(clip, &cached);
    Ok((png, cached))
}

/// Serves `http://thumb.localhost/?path=…`: only files that resolve inside the configured clips
/// folder are ever read, so the protocol cannot be used to exfiltrate arbitrary disk files.
fn thumb_protocol_response(
    request: &tauri::http::Request<Vec<u8>>,
    shared: &AppShared,
) -> tauri::http::Response<Vec<u8>> {
    let Some(raw) = request.uri().query().and_then(thumb_arg) else {
        return http_response(400, "text/plain", b"missing path".to_vec());
    };
    let (output_dir, thumbs) = {
        let st = shared.0.lock().unwrap();
        (st.cfg.output_dir.clone(), st.thumbs.clone())
    };
    let Ok(clip) = resolve_clip_in_dir(&output_dir, &raw) else {
        return http_response(404, "text/plain", b"not found".to_vec());
    };
    match read_clip_thumb(&clip, &thumbs) {
        Ok((Some(png), _)) => http_response(200, "image/png", png),
        _ => http_response(404, "text/plain", b"no preview".to_vec()),
    }
}

/// `std::fs::canonicalize` yields `\\?\` verbatim paths on Windows: they break `explorer`
/// arguments and would not match the paths produced by directory scans, so strip the prefix.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

fn canonicalize_plain(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().map(strip_verbatim)
}

/// Fails closed: the clip must canonicalize inside the canonical clips folder, be a real file
/// (never a symlink/junction) and have no `:` in its name (NTFS alternate data streams).
fn resolve_clip_in_dir(output_dir: &Path, raw: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(raw);
    if p.file_name()
        .and_then(|n| n.to_str())
        .is_none_or(|n| n.contains(':'))
    {
        return Err("Invalid clip path.".into());
    }
    let meta = std::fs::symlink_metadata(&p).map_err(|_| "Clip file not found.".to_string())?;
    if !meta.file_type().is_file() {
        return Err("Clip file not found.".into());
    }
    let canon_dir =
        canonicalize_plain(output_dir).ok_or_else(|| "Clips folder is unavailable.".to_string())?;
    let canon_file = canonicalize_plain(&p).ok_or_else(|| "Clip file not found.".to_string())?;
    if canon_file.parent() != Some(canon_dir.as_path()) {
        return Err("Clip is outside the configured clips folder.".into());
    }
    if !canon_file
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_clip_name)
    {
        return Err("Clip file not found.".into());
    }
    Ok(canon_file)
}

/// Whether both halves of the share target are set: otherwise uploading is impossible.
fn is_share_configured(cfg: &Config) -> bool {
    !cfg.share_url.trim().is_empty() && !cfg.share_key.trim().is_empty()
}

/// Canonical base URL used as the `ShareRecord` key. Falls back to the raw trimmed URL so an
/// unparsable setting still yields a stable key instead of disabling caching entirely.
fn normalized_share_base(share_url: &str) -> String {
    share::normalize_base_url(share_url)
        .unwrap_or_else(|_| share_url.trim().trim_end_matches('/').to_string())
}

/// Snapshot of everything sharing needs, taken under one brief state lock so uploads and gallery
/// scans never hold the `CoreState` mutex across disk, network or condvar work.
#[derive(Clone)]
struct ShareCtx {
    app: AppHandle,
    configured: bool,
    base_url: String,
    share_url: String,
    share_key: String,
    thumbs: ThumbStore,
    shares: ShareStore,
    uploads: UploadTracker,
}

impl ShareCtx {
    fn capture(app: &AppHandle, shared: &AppShared) -> Self {
        let st = shared.0.lock().unwrap();
        Self::from_state(app, &st)
    }

    fn from_state(app: &AppHandle, st: &CoreState) -> Self {
        Self {
            app: app.clone(),
            configured: is_share_configured(&st.cfg),
            base_url: normalized_share_base(&st.cfg.share_url),
            share_url: st.cfg.share_url.clone(),
            share_key: st.cfg.share_key.clone(),
            thumbs: st.thumbs.clone(),
            shares: st.shares.clone(),
            uploads: st.uploads.clone(),
        }
    }

    fn uploaded_url(&self, path: &Path, size_bytes: u64, modified_ms: u64) -> Option<String> {
        if !self.configured {
            return None;
        }
        self.shares
            .get_matching(path, size_bytes, modified_ms, &self.base_url)
            .map(|rec| rec.url)
    }

    fn uploading(&self, path: &Path) -> bool {
        self.uploads.is_uploading(path)
    }

    fn emit_upload_state(&self, path: &str, uploading: bool, uploaded_url: Option<String>) {
        let _ = self.app.emit(
            "clip-upload-state",
            ClipUploadStateEvent {
                path: path.to_string(),
                uploading,
                uploaded_url,
            },
        );
    }
}

/// Stored share for an unchanged clip, if any.
fn cached_share(ctx: &ShareCtx, clip_path: &Path) -> Option<ShareResult> {
    let meta = std::fs::metadata(clip_path).ok()?;
    let rec = ctx.shares.get_matching(
        clip_path,
        meta.len(),
        modified_ms_of(&meta),
        &ctx.base_url,
    )?;
    Some(ShareResult {
        url: rec.url,
        video_url: rec.video_url,
        slug: rec.slug,
    })
}

fn upload_and_record(ctx: &ShareCtx, clip_path: &Path) -> Result<ShareResult, String> {
    let thumb = read_clip_thumb(clip_path, &ctx.thumbs).ok();
    let width = thumb.as_ref().map(|(_, m)| m.width);
    let height = thumb.as_ref().map(|(_, m)| m.height);
    let thumb_png = thumb.and_then(|(png, _)| png);

    let title = library::pretty_name(clip_path);
    let res = share::upload_clip(
        &ctx.share_url,
        &ctx.share_key,
        clip_path,
        &ShareMeta {
            title: &title,
            width,
            height,
            thumb_png: thumb_png.as_deref(),
        },
    )?;

    // Only remember clips that still exist: a deleted or renamed file must not be re-linked.
    if let Ok(meta) = std::fs::metadata(clip_path) {
        ctx.shares.record(
            clip_path,
            ShareRecord {
                url: res.url.clone(),
                video_url: res.video_url.clone(),
                slug: res.slug.clone(),
                base_url: ctx.base_url.clone(),
                size_bytes: meta.len(),
                modified_ms: modified_ms_of(&meta),
            },
        );
    }
    Ok(res)
}

/// Single entry point for both the Share button and auto-upload: returns the share plus whether it
/// was already uploaded (cached, or finished by a concurrent upload of the same clip).
fn upload_or_get_clip(ctx: &ShareCtx, clip_path: &Path) -> Result<(ShareResult, bool), String> {
    if let Some(res) = cached_share(ctx, clip_path) {
        return Ok((res, true));
    }
    match ctx.uploads.begin(upload_key(clip_path)) {
        UploadRole::Follower(slot) => {
            ctx.uploads.wait_for(&slot);
            let res = cached_share(ctx, clip_path)
                .ok_or_else(|| "The upload of this clip did not finish.".to_string())?;
            Ok((res, true))
        }
        UploadRole::Leader(guard) => {
            let path = clip_path.display().to_string();
            ctx.emit_upload_state(&path, true, None);
            let uploaded = upload_and_record(ctx, clip_path);
            // The slot has to leave the tracker before the UI is told the upload ended, otherwise
            // a refresh could still report the clip as busy.
            drop(guard);
            match uploaded {
                Ok(res) => {
                    ctx.emit_upload_state(&path, false, Some(res.url.clone()));
                    Ok((res, false))
                }
                Err(e) => {
                    ctx.emit_upload_state(&path, false, None);
                    Err(e)
                }
            }
        }
    }
}

fn build_clip_entry(
    path: &Path,
    file_name: &str,
    meta: &std::fs::Metadata,
    share: &ShareCtx,
) -> Option<ClipEntry> {
    let size_bytes = meta.len();
    let modified_ms = modified_ms_of(meta);
    let cached = share.thumbs.cached(path, size_bytes, modified_ms);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_string());
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(ClipEntry {
        path: path.display().to_string(),
        file_name: file_name.to_string(),
        stem,
        ext,
        display_name: library::pretty_name(path),
        size_bytes,
        modified_ms,
        duration_ms: cached.as_ref().map(|c| c.duration_ms),
        width: cached.as_ref().map(|c| c.width),
        height: cached.as_ref().map(|c| c.height),
        codec: cached.map(|c| c.codec),
        uploaded_url: share.uploaded_url(path, size_bytes, modified_ms),
        uploading: share.uploading(path),
    })
}

fn build_clip_entry_at(path: &Path, share: &ShareCtx) -> Option<ClipEntry> {
    let meta = std::fs::metadata(path).ok()?;
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    build_clip_entry(path, &file_name, &meta, share)
}

fn scan_clips(dir: &Path, share: &ShareCtx) -> Vec<ClipEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut clips = Vec::new();
    for entry in rd.flatten() {
        // `DirEntry::file_type` reads the type from the directory entry: no extra stat, and
        // symlinks/junctions are rejected without following them.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_clip_name(name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if let Some(clip) = build_clip_entry(&entry.path(), name, &meta, share) {
            clips.push(clip);
        }
    }
    clips.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| b.file_name.cmp(&a.file_name))
    });
    clips
}

/// RAM held by the capture ring buffer, as shown to the UI.
///
/// `typical_ram_mb` describes an active buffer, so it must not be reported when no buffer is
/// alive: the capture thread stops on request (`!running`), pauses via `apply_config` which drops
/// the recorder (`!cfg.enabled`), waits for a game in `GamesOnly` mode, or exits its session after a
/// capture error and retries in 3 s (`has_error`).
fn estimated_ram_mb(cfg: &Config, running: bool, idle_waiting: bool, has_error: bool) -> u32 {
    if !running || !cfg.enabled || idle_waiting || has_error {
        0
    } else {
        cfg.typical_ram_mb()
    }
}

fn build_state_snapshot(st: &CoreState) -> RecorderState {
    let idle_waiting = st.running
        && st.cfg.enabled
        && st.cfg.capture_mode == CaptureMode::GamesOnly
        && st.game.is_none();
    RecorderState {
        running: st.running,
        enabled: st.cfg.enabled,
        idle_waiting,
        seconds: st.cfg.seconds,
        ram_mb: estimated_ram_mb(&st.cfg, st.running, idle_waiting, st.error.is_some()),
        clips_count: st.clips_count,
        hotkey_label: hotkey_name(st.cfg.hotkey_vk, st.cfg.hotkey_mods),
        game: st.game.clone(),
        error: st.error.clone(),
        encoder: st.encoder.clone(),
        autostart: st.autostart,
        app_dir: app_dir().display().to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn sync_tray_locked(st: &CoreState) {
    let active = st.running && st.cfg.enabled;
    if let Some(tray) = &st.tray {
        let _ = tray.set_icon(Some(tray_status_icon()));
        let tip = tray_tooltip(&st.cfg, st.running, st.error.as_deref(), st.game.as_deref());
        let _ = tray.set_tooltip(Some(tip));
    }
    if let Some(item) = &st.menu_toggle {
        let _ = item.set_checked(active);
    }
    if let Some(item) = &st.menu_mic {
        let _ = item.set_checked(st.cfg.mic_audio);
    }
    if let Some(item) = &st.menu_save {
        let _ = item.set_enabled(active);
    }
}

fn make_notify(tx: mpsc::Sender<Event>) -> Notify {
    Arc::new(move |ev| {
        let _ = tx.send(ev);
    })
}

fn emit_state(app: &AppHandle, shared: &AppShared) {
    let snapshot = {
        let st = shared.0.lock().unwrap();
        build_state_snapshot(&st)
    };
    let _ = app.emit("state-changed", snapshot);
}

fn emit_config_and_state(app: &AppHandle, shared: &AppShared) {
    let (cfg, snapshot) = {
        let st = shared.0.lock().unwrap();
        (FrontendConfig::from_config(&st.cfg), build_state_snapshot(&st))
    };
    let _ = app.emit("config-changed", cfg);
    let _ = app.emit("state-changed", snapshot);
}

/// Rescans the clips folder outside the state lock and refreshes the cached count used by
/// `build_state_snapshot` (which must never touch the disk while holding the mutex).
fn refresh_clips_count(shared: &AppShared) {
    let dir = shared.0.lock().unwrap().cfg.output_dir.clone();
    let count = count_clips(&dir);
    let mut st = shared.0.lock().unwrap();
    if st.cfg.output_dir == dir {
        st.clips_count = count;
    }
}

fn emit_toast(app: &AppHandle, kind: &str, title: &str, message: &str) {
    let _ = app.emit(
        "recorder-event",
        RecorderToast {
            kind: kind.to_string(),
            title: title.to_string(),
            message: message.to_string(),
        },
    );
}

fn apply_config(
    shared: &AppShared,
    app: &AppHandle,
    mut new_cfg: Config,
    new_running: Option<bool>,
) {
    new_cfg.sanitize();

    let (old_rec, need_start, notify_tx, reload_tid, dir_changed) = {
        let mut st = shared.0.lock().unwrap();
        let prev_running = st.running;
        if let Some(r) = new_running {
            st.running = r;
        }
        let now_running = st.running;

        let video_changed = new_cfg.video_params() != st.cfg.video_params() || now_running != prev_running;
        let keys_of = |c: &Config, run: bool| (GlobalAction::ALL.map(|a| c.hotkey(a)), c.enabled, run);
        let hotkey_changed = keys_of(&new_cfg, now_running) != keys_of(&st.cfg, prev_running);

        let dir_changed = new_cfg.output_dir != st.cfg.output_dir;
        st.cfg = new_cfg.clone();
        let should_record = st.running && st.cfg.enabled;

        let mut old_rec = None;
        let mut need_start = false;
        if !should_record {
            old_rec = st.rec.take();
            st.error = None;
            st.game = None;
            st.encoder = None;
        } else if video_changed || st.rec.is_none() {
            old_rec = st.rec.take();
            st.error = None;
            st.game = None;
            st.encoder = None;
            need_start = true;
        } else if let Some(r) = &st.rec {
            r.update(st.cfg.clone());
        }

        sync_tray_locked(&st);
        let reload_tid = if hotkey_changed && !st.hotkeys_suspended && st.hotkey_tid != 0 {
            Some(st.hotkey_tid)
        } else {
            None
        };
        (old_rec, need_start, st.ev_tx.clone(), reload_tid, dir_changed)
    };

    // Drop previous recorder outside the mutex so Recorder::drop never deadlocks with notify.
    drop(old_rec);

    if need_start {
        let rec = Recorder::start(new_cfg.clone(), make_notify(notify_tx));
        let mut extra_rec = None;
        {
            let mut st = shared.0.lock().unwrap();
            if st.running && st.cfg.enabled && st.rec.is_none() {
                st.rec = Some(rec);
            } else {
                extra_rec = Some(rec);
            }
        }
        drop(extra_rec);
    }

    if let Some(tid) = reload_tid {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_HOTKEY_RELOAD, WPARAM(0), LPARAM(0));
        }
    }

    if dir_changed {
        refresh_clips_count(shared);
    }
    emit_config_and_state(app, shared);
}

/// Frameless chrome, rounded Win11 corners and the Miku dark-slate border. Runs on every
/// (re)creation of the main window, since the window is destroyed when closed to the tray.
fn configure_main_window(win: &tauri::WebviewWindow) {
    let _ = win.set_icon(
        Image::from_bytes(include_bytes!("../../assets/icon/app64.png"))
            .unwrap()
            .to_owned(),
    );
    let _ = win.set_background_color(Some(tauri::window::Color(13, 17, 26, 255)));
    let Ok(raw_hwnd) = win.hwnd() else { return };
    let hwnd = HWND(raw_hwnd.0);
    unsafe {
        apply_window_icons(hwnd);
        let pref = DWMWCP_ROUND.0 as u32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&pref as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        // Match Win11 native border color to the Miku dark-slate window chrome
        // (#223B4A, keyed to the #141926 tile / #0D111A background) to prevent
        // gray DWM artifacts around the rounded top corners.
        let border: u32 = 0x004A_3B22;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&border as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        let _ = SetWindowSubclass(hwnd, Some(frameless_subclass_proc), SUBCLASS_FRAMELESS_ID, 0);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

/// Builds the `main` webview on demand. Tray-only launches never call this, so no WebView2
/// process tree, GPU compositor or background DOM refresh exists until the UI is opened.
fn create_main_window(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "main",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("clip")
    .inner_size(880.0, 453.0)
    .min_inner_size(760.0, 240.0)
    .resizable(false)
    .maximizable(false)
    .transparent(false)
    .decorations(false)
    .theme(Some(tauri::Theme::Dark))
    .visible(false)
    .build()?;
    configure_main_window(&win);
    let _ = win.show();
    let _ = win.set_focus();
    Ok(win)
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    } else {
        // Closed to the tray earlier: the window was destroyed, so build a fresh one.
        let _ = create_main_window(app);
    }
}

fn trigger_save(shared: &AppShared) {
    let st = shared.0.lock().unwrap();
    if let Some(r) = &st.rec {
        r.save();
    }
}

fn perform_toggle_recording(shared: &AppShared, app: &AppHandle, play_sound: bool) -> RecorderState {
    let (mut cfg, next_running) = {
        let st = shared.0.lock().unwrap();
        let mut c = st.cfg.clone();
        if !st.running {
            c.enabled = true;
            (c, true)
        } else {
            c.enabled = !c.enabled;
            (c, true)
        }
    };
    cfg.sanitize();
    let _ = config::save(&cfg);
    apply_config(shared, app, cfg.clone(), Some(next_running));
    if play_sound {
        if cfg.enabled {
            sound::play_resumed();
        } else {
            sound::play_paused();
        }
    }
    let st = shared.0.lock().unwrap();
    build_state_snapshot(&st)
}

/// Marks the process as exiting and wakes the `clip-show` thread. `RunEvent::ExitRequested`
/// prevents the default exit while this flag is clear (tray mode), so every `exit(0)` path
/// must go through here first.
fn begin_exit(shared: &AppShared) {
    let exiting = shared.0.lock().unwrap().exiting.clone();
    exiting.store(true, Ordering::Relaxed);
    unsafe {
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, SHOW_EVENT) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
        }
    }
}

fn shutdown_app(shared: &AppShared, app: &AppHandle) {
    begin_exit(shared);
    let (old_rec, hotkey_tid, cancel_flag) = {
        let mut st = shared.0.lock().unwrap();
        st.running = false;
        (st.rec.take(), st.hotkey_tid, st.capture_cancel.take())
    };
    // Wakes the background update checker immediately instead of letting it sleep out the
    // six-hour interval.
    signal_update_shutdown(shared);
    if let Some(c) = cancel_flag {
        c.store(true, Ordering::Relaxed);
    }
    if hotkey_tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(hotkey_tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    drop(old_rec);
    release_instance_mutex(shared);
    app.exit(0);
}

fn register_hotkeys(cfg: &Config, running: bool) -> Vec<String> {
    unsafe {
        for a in GlobalAction::ALL {
            let _ = UnregisterHotKey(None, a.id());
        }
    }
    let mut failed = Vec::new();
    for a in GlobalAction::ALL {
        let (vk, mods) = cfg.hotkey(a);
        if vk == 0 || (a == GlobalAction::SaveClip && !(running && cfg.enabled)) {
            continue;
        }
        let ok = unsafe {
            RegisterHotKey(None, a.id(), HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0), vk).is_ok()
        };
        if !ok {
            failed.push(hotkey_name(vk, mods));
        }
    }
    failed
}

fn unregister_all_hotkeys() {
    unsafe {
        for a in GlobalAction::ALL {
            let _ = UnregisterHotKey(None, a.id());
        }
    }
}

fn spawn_hotkey_thread(shared: AppShared, app: AppHandle) -> u32 {
    let (tid_tx, tid_rx) = mpsc::sync_channel(1);
    let _ = std::thread::Builder::new()
        .name("clip-hotkeys".into())
        .spawn(move || unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            let tid = GetCurrentThreadId();
            let _ = tid_tx.send(tid);

            let (cfg, running, suspended) = {
                let st = shared.0.lock().unwrap();
                (st.cfg.clone(), st.running, st.hotkeys_suspended)
            };
            if !suspended {
                let failed = register_hotkeys(&cfg, running);
                if !failed.is_empty() {
                    emit_toast(
                        &app,
                        "warn",
                        "Hotkey in use",
                        &format!(
                            "Another program is using {}. Choose a different one in Settings.",
                            failed.join(", ")
                        ),
                    );
                }
            }

            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                match msg.message {
                    WM_HOTKEY => match GlobalAction::from_id(msg.wParam.0 as i32) {
                        Some(GlobalAction::SaveClip) => trigger_save(&shared),
                        Some(GlobalAction::ToggleRecording) => {
                            perform_toggle_recording(&shared, &app, true);
                        }
                        Some(GlobalAction::OpenFolder) => {
                            let dir = shared.0.lock().unwrap().cfg.output_dir.clone();
                            open_explorer(&dir);
                        }
                        None => {}
                    },
                    WM_HOTKEY_RELOAD => {
                        let (cfg, running, suspended) = {
                            let st = shared.0.lock().unwrap();
                            (st.cfg.clone(), st.running, st.hotkeys_suspended)
                        };
                        if !suspended {
                            let failed = register_hotkeys(&cfg, running);
                            if !failed.is_empty() {
                                emit_toast(
                                    &app,
                                    "warn",
                                    "Hotkey in use",
                                    &format!(
                                        "Another program is using {}. Choose a different one in Settings.",
                                        failed.join(", ")
                                    ),
                                );
                            }
                        }
                    }
                    WM_HOTKEY_SUSPEND => {
                        if msg.wParam.0 == 1 {
                            unregister_all_hotkeys();
                        } else {
                            let (cfg, running) = {
                                let st = shared.0.lock().unwrap();
                                (st.cfg.clone(), st.running)
                            };
                            let failed = register_hotkeys(&cfg, running);
                            if !failed.is_empty() {
                                emit_toast(
                                    &app,
                                    "warn",
                                    "Hotkey in use",
                                    &format!(
                                        "Another program is using {}. Choose a different one in Settings.",
                                        failed.join(", ")
                                    ),
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
            unregister_all_hotkeys();
        });
    tid_rx.recv().unwrap_or(0)
}

fn spawn_event_thread(shared: AppShared, app: AppHandle, ev_rx: mpsc::Receiver<Event>) {
    let _ = std::thread::Builder::new()
        .name("clip-events".into())
        .spawn(move || {
            for ev in ev_rx {
                match ev {
                    Event::Started(enc) => {
                        {
                            let mut st = shared.0.lock().unwrap();
                            st.error = None;
                            st.encoder = Some(enc);
                            sync_tray_locked(&st);
                        }
                        emit_state(&app, &shared);
                    }
                    Event::Game(g) => {
                        {
                            let mut st = shared.0.lock().unwrap();
                            st.game = g;
                            sync_tray_locked(&st);
                        }
                        emit_state(&app, &shared);
                    }
                    Event::Saved(path) => {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string());
                        emit_toast(&app, "info", "Clip saved", &name);
                        refresh_clips_count(&shared);
                        emit_state(&app, &shared);
                        let _ = app.emit("clips-changed", ());

                        // The recorder reports the path it wrote, which may be verbatim or relative;
                        // the share store and the gallery key on the canonical plain path.
                        let clip_path = canonicalize_plain(&path).unwrap_or_else(|| strip_verbatim(path));
                        let size_bytes = std::fs::metadata(&clip_path).map(|m| m.len()).unwrap_or(0);
                        let share = ShareCtx::capture(&app, &shared);
                        if share::should_auto_upload(size_bytes, &share.share_url, &share.share_key) {
                            let app = app.clone();
                            let _ = std::thread::Builder::new()
                                .name("clip-autoupload".into())
                                .spawn(move || match upload_or_get_clip(&share, &clip_path) {
                                    Ok((res, already_uploaded)) => {
                                        let _ = app.emit("clips-changed", ());
                                        if !already_uploaded {
                                            emit_toast(&app, "info", "Clip uploaded", &res.url);
                                        }
                                    }
                                    Err(e) => {
                                        log(&format!(
                                            "auto-upload failed for {}: {e}",
                                            clip_path.display()
                                        ));
                                        emit_toast(&app, "warn", "Auto-upload failed", &e);
                                    }
                                });
                        }
                    }
                    Event::SaveFailed(e) => {
                        emit_toast(&app, "error", "Couldn't save the clip", &e);
                    }
                    Event::Error(e) => {
                        let is_new = {
                            let mut st = shared.0.lock().unwrap();
                            let new = st.error.is_none();
                            st.error = Some(e.clone());
                            sync_tray_locked(&st);
                            new
                        };
                        emit_state(&app, &shared);
                        if is_new {
                            emit_toast(
                                &app,
                                "error",
                                "clip can't record",
                                &format!("{e}. Retrying automatically."),
                            );
                        }
                    }
                }
            }
        });
}

#[tauri::command]
fn get_config(state: State<'_, AppShared>) -> FrontendConfig {
    FrontendConfig::from_config(&state.0.lock().unwrap().cfg)
}

#[tauri::command]
async fn set_config(
    app: AppHandle,
    state: State<'_, AppShared>,
    mut cfg: Config,
) -> Result<FrontendConfig, String> {
    // `output_dir` changes only through `pick_folder` and `share_key` only through
    // `set_share_key`: neither can be redirected/overwritten from the webview.
    {
        let st = state.0.lock().unwrap();
        cfg.output_dir = st.cfg.output_dir.clone();
        cfg.share_key = st.cfg.share_key.clone();
    }
    cfg.sanitize();
    if cfg.share_url.trim().is_empty() {
        cfg.share_url.clear();
    } else {
        cfg.share_url = share::normalize_base_url(&cfg.share_url)?;
    }
    let (cfg, saved) = tauri::async_runtime::spawn_blocking(move || {
        let res = config::save(&cfg);
        (cfg, res)
    })
    .await
    .map_err(|e| format!("Save task failed: {e}"))?;
    saved.map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg.clone(), None);
    Ok(FrontendConfig::from_config(&cfg))
}

/// Write-only upload key: the value is never sent back to the webview, only `has_share_key`.
#[tauri::command]
async fn set_share_key(
    app: AppHandle,
    state: State<'_, AppShared>,
    share_key: String,
) -> Result<FrontendConfig, String> {
    let key = share_key.trim();
    if key.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        return Err("Upload key must be a single line.".into());
    }
    let mut cfg = { state.0.lock().unwrap().cfg.clone() };
    cfg.share_key = key.to_string();
    cfg.sanitize();
    let (cfg, saved) = tauri::async_runtime::spawn_blocking(move || {
        let res = config::save(&cfg);
        (cfg, res)
    })
    .await
    .map_err(|e| format!("Save task failed: {e}"))?;
    saved.map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg.clone(), None);
    Ok(FrontendConfig::from_config(&cfg))
}

/// Both halves of the share target, or the error every R2 admin call should surface.
fn require_share_target(shared: &AppShared) -> Result<(String, String), String> {
    let st = shared.0.lock().unwrap();
    if !is_share_configured(&st.cfg) {
        return Err("Set Share URL and Upload key first.".into());
    }
    Ok((st.cfg.share_url.clone(), st.cfg.share_key.clone()))
}

#[tauri::command]
async fn get_r2_stats(state: State<'_, AppShared>) -> Result<R2Stats, String> {
    let (share_url, share_key) = require_share_target(state.inner())?;
    tauri::async_runtime::spawn_blocking(move || share::fetch_r2_stats(&share_url, &share_key))
        .await
        .map_err(|e| format!("R2 stats task failed: {e}"))?
}

#[tauri::command]
async fn set_r2_ttl(state: State<'_, AppShared>, ttl_seconds: u64) -> Result<u64, String> {
    let (share_url, share_key) = require_share_target(state.inner())?;
    tauri::async_runtime::spawn_blocking(move || {
        share::set_r2_ttl(&share_url, &share_key, ttl_seconds)
    })
    .await
    .map_err(|e| format!("R2 settings task failed: {e}"))?
}

#[tauri::command]
async fn delete_all_r2_clips(
    app: AppHandle,
    state: State<'_, AppShared>,
    scope: Option<String>,
) -> Result<PurgeR2Result, String> {
    // Default to the caller's own clips: "all" is an Owner-only extra.
    let scope = scope.unwrap_or_else(|| "mine".to_string());
    let (share_url, share_key, base_url, shares) = {
        let st = state.0.lock().unwrap();
        if !is_share_configured(&st.cfg) {
            return Err("Set Share URL and Upload key first.".into());
        }
        (
            st.cfg.share_url.clone(),
            st.cfg.share_key.clone(),
            normalized_share_base(&st.cfg.share_url),
            st.shares.clone(),
        )
    };
    let result = tauri::async_runtime::spawn_blocking(move || {
        let result = share::delete_all_r2_clips(&share_url, &share_key, &scope)?;
        // Every clip this PC uploaded belongs to the caller, so both scopes invalidate the cached
        // links here. Local .mp4 files stay either way.
        shares.remove_for_base(&base_url);
        Ok::<_, String>(result)
    })
    .await
    .map_err(|e| format!("R2 delete task failed: {e}"))??;
    // Card share state is cached, so the gallery has to re-list to fall back to "Share link".
    let _ = app.emit("clips-changed", ());
    Ok(result)
}

/// Imports a `clip-cf:` friend invite: saves its base URL and key, without echoing the key back.
#[tauri::command]
async fn import_cf_invite(
    app: AppHandle,
    state: State<'_, AppShared>,
    invite: String,
) -> Result<FrontendConfig, String> {
    let (share_url, share_key) = share::decode_invite_code(&invite)?;
    let mut cfg = { state.0.lock().unwrap().cfg.clone() };
    cfg.share_url = share_url;
    cfg.share_key = share_key;
    cfg.sanitize();
    let (cfg, saved) = tauri::async_runtime::spawn_blocking(move || {
        let res = config::save(&cfg);
        (cfg, res)
    })
    .await
    .map_err(|e| format!("Save task failed: {e}"))?;
    saved.map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg.clone(), None);
    Ok(FrontendConfig::from_config(&cfg))
}

/// Mints a friend invite; the raw token stays here and only the invite code reaches the clipboard.
#[tauri::command]
async fn create_r2_invite(
    state: State<'_, AppShared>,
    label: String,
    quota_bytes: u64,
) -> Result<R2Friend, String> {
    let (share_url, share_key) = require_share_target(state.inner())?;
    let created = tauri::async_runtime::spawn_blocking(move || {
        share::create_r2_invite(&share_url, &share_key, &label, quota_bytes)
    })
    .await
    .map_err(|e| format!("R2 invite task failed: {e}"))??;
    share::copy_to_clipboard(&created.invite_code)?;
    Ok(created.friend)
}

/// Revokes a friend invite, optionally deleting the clips that friend uploaded.
#[tauri::command]
async fn revoke_r2_invite(
    state: State<'_, AppShared>,
    id: String,
    delete_clips: bool,
) -> Result<RevokeInviteResult, String> {
    let (share_url, share_key) = require_share_target(state.inner())?;
    tauri::async_runtime::spawn_blocking(move || {
        share::revoke_r2_invite(&share_url, &share_key, &id, delete_clips)
    })
    .await
    .map_err(|e| format!("R2 revoke task failed: {e}"))?
}

/// Forgets the saved Worker credentials, leaving the bucket and cached links untouched.
#[tauri::command]
async fn disconnect_r2(
    app: AppHandle,
    state: State<'_, AppShared>,
) -> Result<FrontendConfig, String> {
    let mut cfg = { state.0.lock().unwrap().cfg.clone() };
    cfg.share_url.clear();
    cfg.share_key.clear();
    cfg.sanitize();
    let (cfg, saved) = tauri::async_runtime::spawn_blocking(move || {
        let res = config::save(&cfg);
        (cfg, res)
    })
    .await
    .map_err(|e| format!("Save task failed: {e}"))?;
    saved.map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg.clone(), None);
    Ok(FrontendConfig::from_config(&cfg))
}

#[tauri::command]
async fn recorder_state(state: State<'_, AppShared>) -> Result<RecorderState, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        refresh_clips_count(&shared);
        let auto = autostart::enabled();
        let mut st = shared.0.lock().unwrap();
        st.autostart = auto;
        build_state_snapshot(&st)
    })
    .await
    .map_err(|e| format!("State task failed: {e}"))
}

#[tauri::command]
fn toggle_recording(app: AppHandle, state: State<'_, AppShared>) -> RecorderState {
    perform_toggle_recording(&state, &app, false)
}

#[tauri::command]
fn start_recorder(app: AppHandle, state: State<'_, AppShared>) -> Result<RecorderState, String> {
    let mut cfg = {
        let st = state.0.lock().unwrap();
        st.cfg.clone()
    };
    cfg.enabled = true;
    cfg.sanitize();
    config::save(&cfg).map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg, Some(true));
    let st = state.0.lock().unwrap();
    Ok(build_state_snapshot(&st))
}

#[tauri::command]
fn pause_resume(app: AppHandle, state: State<'_, AppShared>) -> Result<RecorderState, String> {
    let mut cfg = {
        let st = state.0.lock().unwrap();
        st.cfg.clone()
    };
    cfg.enabled = !cfg.enabled;
    cfg.sanitize();
    config::save(&cfg).map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg, Some(true));
    let st = state.0.lock().unwrap();
    Ok(build_state_snapshot(&st))
}

#[tauri::command]
fn open_folder(state: State<'_, AppShared>, target: Option<String>) {
    if state.0.lock().unwrap().needs_setup {
        return;
    }
    if target.as_deref() == Some("data") {
        open_explorer(&app_dir());
    } else {
        let dir = state.0.lock().unwrap().cfg.output_dir.clone();
        open_explorer(&dir);
    }
}

#[tauri::command]
async fn pick_folder(
    app: AppHandle,
    state: State<'_, AppShared>,
) -> Result<Option<FrontendConfig>, String> {
    let cur_dir = state.0.lock().unwrap().cfg.output_dir.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut dlg = rfd::FileDialog::new().set_title("Folder for clips");
        if cur_dir.exists() {
            dlg = dlg.set_directory(&cur_dir);
        }
        dlg.pick_folder()
    })
    .await
    .map_err(|e| format!("Folder picker failed: {e}"))?;
    let Some(path) = picked else {
        return Ok(None);
    };
    // `output_dir` is writable here only (plus `set_config` preserving it).
    let mut cfg = { state.0.lock().unwrap().cfg.clone() };
    cfg.output_dir = path;
    cfg.sanitize();
    let (cfg, saved) = tauri::async_runtime::spawn_blocking(move || {
        let res = config::save(&cfg);
        (cfg, res)
    })
    .await
    .map_err(|e| format!("Save task failed: {e}"))?;
    saved.map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg.clone(), None);
    Ok(Some(FrontendConfig::from_config(&cfg)))
}

#[tauri::command]
async fn list_mics() -> Vec<MicDevice> {
    tauri::async_runtime::spawn_blocking(|| {
        // `capture_devices` initializes MTA COM on this worker thread itself.
        audio::capture_devices()
            .into_iter()
            .map(|(id, name)| MicDevice { id, name })
            .collect()
    })
    .await
    .unwrap_or_default()
}

#[tauri::command]
async fn set_mic_device(
    app: AppHandle,
    state: State<'_, AppShared>,
    device_id: String,
) -> Result<FrontendConfig, String> {
    let mut cfg = { state.0.lock().unwrap().cfg.clone() };
    cfg.mic_device = device_id;
    cfg.sanitize();
    let (cfg, saved) = tauri::async_runtime::spawn_blocking(move || {
        let res = config::save(&cfg);
        (cfg, res)
    })
    .await
    .map_err(|e| format!("Save task failed: {e}"))?;
    saved.map_err(|e| format!("Failed to save config: {e}"))?;
    apply_config(&state, &app, cfg.clone(), None);
    Ok(FrontendConfig::from_config(&cfg))
}

#[tauri::command]
fn test_sound(state: State<'_, AppShared>, volume: Option<u32>) {
    let vol = volume.unwrap_or_else(|| state.0.lock().unwrap().cfg.sound_volume);
    sound::play_saved(vol);
}

#[tauri::command]
async fn get_clips_count(state: State<'_, AppShared>) -> Result<usize, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        refresh_clips_count(&shared);
        shared.0.lock().unwrap().clips_count
    })
    .await
    .map_err(|e| format!("Count task failed: {e}"))
}

#[tauri::command]
async fn list_clips(app: AppHandle, state: State<'_, AppShared>) -> Result<Vec<ClipEntry>, String> {
    let shared = state.inner().clone();
    let (dir, share) = {
        let st = state.0.lock().unwrap();
        (st.cfg.output_dir.clone(), ShareCtx::from_state(&app, &st))
    };
    // Canonicalize the folder once per scan: thumbnail keys and entry paths are then already
    // canonical, so no per-file `canonicalize`/`is_file` syscalls are needed.
    tauri::async_runtime::spawn_blocking(move || {
        let Some(scan_dir) = canonicalize_plain(&dir) else { return Vec::new() };
        let clips = scan_clips(&scan_dir, &share);
        // This scan already enumerated the folder: refresh the cached count here instead of
        // making the frontend trigger a second, separate folder scan.
        let mut st = shared.0.lock().unwrap();
        if st.cfg.output_dir == dir {
            st.clips_count = clips.len();
        }
        clips
    })
    .await
    .map_err(|e| format!("Clip scan failed: {e}"))
}

/// Preview payload with the clip's cached metadata; `thumb_url` stays `None` when no cover exists.
fn clip_preview(path: String, thumb_url: Option<String>, meta: &ThumbMeta) -> ClipPreview {
    ClipPreview {
        path,
        thumb_url,
        duration_ms: Some(meta.duration_ms),
        width: Some(meta.width),
        height: Some(meta.height),
        codec: Some(meta.codec.clone()),
        error: None,
    }
}

#[tauri::command]
async fn get_clip_preview(state: State<'_, AppShared>, path: String) -> Result<ClipPreview, String> {
    let (output_dir, thumbs) = {
        let st = state.0.lock().unwrap();
        (st.cfg.output_dir.clone(), st.thumbs.clone())
    };
    let clip_path = resolve_clip_in_dir(&output_dir, &path)?;
    tauri::async_runtime::spawn_blocking(move || {
        // Fast path: a valid cache entry needs no decode, and for a cached cover the PNG is not read
        // either — the webview fetches it through the `thumb` protocol.
        let cached = std::fs::metadata(&clip_path)
            .ok()
            .and_then(|m| thumbs.cached(&clip_path, m.len(), modified_ms_of(&m)));
        if let Some(meta) = cached {
            let png_p = thumb_cache_paths(&clip_path).1;
            if !meta.has_thumb {
                return clip_preview(path, None, &meta);
            }
            if png_p.is_file() {
                touch_thumb_if_stale(&png_p);
                let url = clip_thumb_url(&clip_path, meta.modified_ms);
                return clip_preview(path, Some(url), &meta);
            }
        }
        match read_clip_thumb(&clip_path, &thumbs) {
            Ok((png, meta)) => {
                let url = png.map(|_| clip_thumb_url(&clip_path, meta.modified_ms));
                clip_preview(path, url, &meta)
            }
            Err(e) => ClipPreview {
                path,
                thumb_url: None,
                duration_ms: None,
                width: None,
                height: None,
                codec: None,
                error: Some(e),
            },
        }
    })
    .await
    .map_err(|e| format!("Preview task failed: {e}"))
}

#[tauri::command]
async fn rename_clip(
    app: AppHandle,
    state: State<'_, AppShared>,
    path: String,
    new_name: String,
) -> Result<ClipEntry, String> {
    let (output_dir, share) = {
        let st = state.0.lock().unwrap();
        (st.cfg.output_dir.clone(), ShareCtx::from_state(&app, &st))
    };
    let entry = tauri::async_runtime::spawn_blocking(move || {
        let old_path = resolve_clip_in_dir(&output_dir, &path)?;
        let (old_meta, old_png) = thumb_cache_paths(&old_path);
        let new_path = library::rename_clip(&old_path, &new_name)?;
        let new_path = canonicalize_plain(&new_path).unwrap_or(new_path);
        migrate_thumb_cache(&old_meta, &old_png, &new_path);
        share.shares.migrate(&old_path, &new_path);
        share.thumbs.forget(&old_path);
        build_clip_entry_at(&new_path, &share)
            .ok_or_else(|| "Renamed clip could not be read.".to_string())
    })
    .await
    .map_err(|e| format!("Rename task failed: {e}"))??;
    refresh_clips_count(&state);
    emit_state(&app, &state);
    Ok(entry)
}

#[tauri::command]
async fn delete_clips(
    app: AppHandle,
    state: State<'_, AppShared>,
    paths: Vec<String>,
) -> Result<DeleteClipsResult, String> {
    let (output_dir, thumbs, shares) = {
        let st = state.0.lock().unwrap();
        (
            st.cfg.output_dir.clone(),
            st.thumbs.clone(),
            st.shares.clone(),
        )
    };
    let mut targets = Vec::with_capacity(paths.len());
    let mut failed = Vec::new();
    for raw in paths {
        match resolve_clip_in_dir(&output_dir, &raw) {
            Ok(p) => targets.push((raw, p)),
            Err(e) => {
                let name = library::pretty_name(Path::new(&raw));
                failed.push(DeleteClipFailure { path: raw, name, error: e });
            }
        }
    }

    let (deleted, rec_failed) = tauri::async_runtime::spawn_blocking(move || {
        let mut deleted = Vec::new();
        let mut rec_failed = Vec::new();
        for (raw, p) in targets {
            match library::recycle(&p) {
                Ok(()) => {
                    remove_thumb_cache(&p);
                    thumbs.forget(&p);
                    shares.remove(&p);
                    deleted.push(raw);
                }
                Err(e) => {
                    rec_failed.push(DeleteClipFailure {
                        path: raw,
                        name: library::pretty_name(&p),
                        error: e.to_string(),
                    });
                }
            }
        }
        (deleted, rec_failed)
    })
    .await
    .map_err(|e| format!("Delete task failed: {e}"))?;

    failed.extend(rec_failed);
    refresh_clips_count(&state);
    emit_state(&app, &state);
    Ok(DeleteClipsResult { deleted, failed })
}

#[tauri::command]
fn reveal_clip(state: State<'_, AppShared>, path: String) -> Result<(), String> {
    let output_dir = state.0.lock().unwrap().cfg.output_dir.clone();
    let clip_path = resolve_clip_in_dir(&output_dir, &path)?;
    std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", clip_path.display()))
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Failed to open File Explorer: {e}"))
}

#[tauri::command]
async fn copy_clip_file(state: State<'_, AppShared>, path: String) -> Result<(), String> {
    let output_dir = state.0.lock().unwrap().cfg.output_dir.clone();
    let clip_path = resolve_clip_in_dir(&output_dir, &path)?;
    tauri::async_runtime::spawn_blocking(move || share::copy_file_to_clipboard(&clip_path))
        .await
        .map_err(|e| format!("Copy task failed: {e}"))?
}

#[tauri::command]
async fn share_clip(
    app: AppHandle,
    state: State<'_, AppShared>,
    path: String,
) -> Result<ShareClipResult, String> {
    let (output_dir, share) = {
        let st = state.0.lock().unwrap();
        (st.cfg.output_dir.clone(), ShareCtx::from_state(&app, &st))
    };
    if !share.configured {
        return Err("Set Share URL and Upload key in the Cloudflare tab first.".into());
    }
    let clip_path = resolve_clip_in_dir(&output_dir, &path)?;
    let (res, already_uploaded, copied) = tauri::async_runtime::spawn_blocking(move || {
        let (res, already_uploaded) = upload_or_get_clip(&share, &clip_path)?;
        let copied = share::copy_to_clipboard(&res.url).is_ok();
        Ok::<_, String>((res, already_uploaded, copied))
    })
    .await
    .map_err(|e| format!("Share task failed: {e}"))??;
    Ok(ShareClipResult {
        url: res.url,
        video_url: res.video_url,
        slug: res.slug,
        copied,
        already_uploaded,
    })
}

#[tauri::command]
async fn set_autostart(app: AppHandle, state: State<'_, AppShared>, on: bool) -> Result<bool, String> {
    let shared = state.inner().clone();
    let enabled = tauri::async_runtime::spawn_blocking(move || {
        autostart::set(on);
        autostart::enabled()
    })
    .await
    .map_err(|e| format!("Autostart task failed: {e}"))?;
    shared.0.lock().unwrap().autostart = enabled;
    emit_state(&app, &shared);
    Ok(enabled)
}

#[tauri::command]
fn begin_hotkey_capture(app: AppHandle, state: State<'_, AppShared>) {
    let shared = state.inner().clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let tid = {
        let mut st = shared.0.lock().unwrap();
        if let Some(prev) = st.capture_cancel.take() {
            prev.store(true, Ordering::Relaxed);
        }
        st.capture_cancel = Some(cancel.clone());
        st.hotkeys_suspended = true;
        st.hotkey_tid
    };
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_HOTKEY_SUSPEND, WPARAM(1), LPARAM(0));
        }
    }

    std::thread::spawn(move || {
        // Wait briefly for any previously held keys to release so they don't bind immediately.
        let release_deadline = Instant::now() + Duration::from_millis(250);
        while keys::pressed_key().is_some() && Instant::now() < release_deadline {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }

        let mut captured: Option<(u32, u32)> = None;
        while !cancel.load(Ordering::Relaxed) {
            if let Some((vk, mods)) = keys::pressed_key() {
                captured = Some((vk, mods));
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }

        let cancelled_externally = cancel.load(Ordering::Relaxed);
        let resume_tid = {
            let mut st = shared.0.lock().unwrap();
            st.capture_cancel = None;
            st.hotkeys_suspended = false;
            st.hotkey_tid
        };

        if cancelled_externally {
            if resume_tid != 0 {
                unsafe {
                    let _ = PostThreadMessageW(resume_tid, WM_HOTKEY_SUSPEND, WPARAM(0), LPARAM(0));
                }
            }
            return;
        }

        match captured {
            Some((0x1B, _)) | None => {
                if resume_tid != 0 {
                    unsafe {
                        let _ = PostThreadMessageW(resume_tid, WM_HOTKEY_SUSPEND, WPARAM(0), LPARAM(0));
                    }
                }
                let _ = app.emit(
                    "hotkey-captured",
                    HotkeyCapturedPayload {
                        cancelled: true,
                        vk: None,
                        mods: None,
                        label: None,
                        note: None,
                    },
                );
            }
            Some((vk, mods)) => {
                let mut note = None;
                let mut new_cfg = shared.0.lock().unwrap().cfg.clone();
                for other in GlobalAction::ALL {
                    if other != GlobalAction::SaveClip && new_cfg.hotkey(other) == (vk, mods) {
                        let (v, m) = new_cfg.hotkey_mut(other);
                        *v = 0;
                        *m = 0;
                        note = Some(format!(
                            "“{}” is now unassigned because {} was already in use.",
                            other.label(),
                            hotkey_name(vk, mods)
                        ));
                    }
                }
                let (v, m) = new_cfg.hotkey_mut(GlobalAction::SaveClip);
                *v = vk;
                *m = mods;
                new_cfg.sanitize();
                let _ = config::save(&new_cfg);
                apply_config(&shared, &app, new_cfg, None);

                if resume_tid != 0 {
                    unsafe {
                        let _ = PostThreadMessageW(resume_tid, WM_HOTKEY_SUSPEND, WPARAM(0), LPARAM(0));
                    }
                }
                let _ = app.emit(
                    "hotkey-captured",
                    HotkeyCapturedPayload {
                        cancelled: false,
                        vk: Some(vk),
                        mods: Some(mods),
                        label: Some(hotkey_name(vk, mods)),
                        note,
                    },
                );
            }
        }
    });
}

#[tauri::command]
fn cancel_hotkey_capture(state: State<'_, AppShared>) {
    let (cancel, tid) = {
        let mut st = state.0.lock().unwrap();
        st.hotkeys_suspended = false;
        (st.capture_cancel.take(), st.hotkey_tid)
    };
    if let Some(c) = cancel {
        c.store(true, Ordering::Relaxed);
    }
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_HOTKEY_SUSPEND, WPARAM(0), LPARAM(0));
        }
    }
}

// Tauri/tao's `set_icon` only sets ICON_SMALL, so the taskbar falls back to the class
// (or cached) icon and can flicker against it. Point both WM_SETICON slots and the class
// icons at the embedded 32512 resource (`icons/icon.ico`) so every surface shows the same
// bitmap. Icons are loaded LR_SHARED, so Windows owns the handles and nothing leaks.
unsafe fn apply_window_icons(hwnd: HWND) {
    let Ok(module) = GetModuleHandleW(None) else {
        return;
    };
    let instance = HINSTANCE(module.0);
    // GetDpiForWindow returns 0 when it cannot resolve the window: fall back to system metrics.
    let dpi = GetDpiForWindow(hwnd);
    let (big_cx, big_cy, small_cx, small_cy) = if dpi != 0 {
        (
            GetSystemMetricsForDpi(SM_CXICON, dpi),
            GetSystemMetricsForDpi(SM_CYICON, dpi),
            GetSystemMetricsForDpi(SM_CXSMICON, dpi),
            GetSystemMetricsForDpi(SM_CYSMICON, dpi),
        )
    } else {
        (
            GetSystemMetrics(SM_CXICON),
            GetSystemMetrics(SM_CYICON),
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
        )
    };
    let big = load_shared_app_icon(instance, big_cx, big_cy);
    let small = load_shared_app_icon(instance, small_cx, small_cy);
    if !big.is_invalid() {
        let _ = SendMessageW(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_BIG as usize),
            LPARAM(big.0 as isize),
        );
        SetClassLongPtrW(hwnd, GCLP_HICON, big.0 as isize);
    }
    if !small.is_invalid() {
        let _ = SendMessageW(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_SMALL as usize),
            LPARAM(small.0 as isize),
        );
        SetClassLongPtrW(hwnd, GCLP_HICONSM, small.0 as isize);
    }
}

unsafe fn load_shared_app_icon(instance: HINSTANCE, cx: i32, cy: i32) -> HANDLE {
    if cx <= 0 || cy <= 0 {
        return HANDLE::default();
    }
    // 32512 == MAKEINTRESOURCE(IDI_APPLICATION); tauri-build embeds icons/icon.ico there.
    LoadImageW(
        instance,
        windows::core::PCWSTR(32512 as *const u16),
        IMAGE_ICON,
        cx,
        cy,
        LR_SHARED,
    )
    .unwrap_or_default()
}

fn apply_window_height(hwnd_addr: isize, height_px: i32, wa_top: i32, wa_bottom: i32) {
    unsafe {
        let hwnd = HWND(hwnd_addr as *mut std::ffi::c_void);
        if IsZoomed(hwnd).as_bool() {
            return;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            let width_px = rect.right - rect.left;
            let overflows_bottom = rect.top + height_px > wa_bottom;
            if rect.bottom - rect.top != height_px || overflows_bottom {
                if overflows_bottom {
                    let new_y = (wa_bottom - height_px).max(wa_top);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rect.left,
                        new_y,
                        width_px,
                        height_px,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                } else {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        width_px,
                        height_px,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
        }
    }
}

#[tauri::command]
async fn fit_window_height(
    app: AppHandle,
    state: State<'_, AppShared>,
    height: f64,
    animate: bool,
) -> Result<(), String> {
    if !height.is_finite() || height <= 0.0 {
        return Ok(());
    }
    let Some(win) = app.get_webview_window("main") else {
        return Ok(());
    };
    if win.is_maximized().unwrap_or(false) {
        return Ok(());
    }

    let scale = win.scale_factor().unwrap_or(1.0);
    let min_px = (200.0 * scale).round() as i32;
    let wa = win.current_monitor().ok().flatten().map(|m| *m.work_area());
    let wa_top = wa.map(|w| w.position.y).unwrap_or(i32::MIN);
    let wa_bottom = wa
        .map(|w| w.position.y + w.size.height as i32)
        .unwrap_or(i32::MAX);
    let max_px = wa
        .map(|w| w.size.height as i32)
        .unwrap_or(i32::MAX)
        .max(min_px);
    let hwnd_addr = win.hwnd().map_err(|e| e.to_string())?.0 as isize;
    let hwnd = HWND(hwnd_addr as *mut std::ffi::c_void);
    let mut wr = RECT::default();
    let mut cr = RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut wr).is_err() || GetClientRect(hwnd, &mut cr).is_err() {
            return Ok(());
        }
    }
    let start_h = wr.bottom - wr.top;
    let nc_h = ((wr.bottom - wr.top) - (cr.bottom - cr.top)).max(0);
    let target_px = ((height * scale).round() as i32 + nc_h).clamp(min_px + nc_h, max_px);

    let anim_id = {
        let mut st = state.0.lock().unwrap();
        st.height_anim_id = st.height_anim_id.wrapping_add(1);
        st.height_anim_id
    };

    if (target_px - start_h).abs() <= 1 {
        if target_px != start_h || wr.top + target_px > wa_bottom {
            let _ = app.run_on_main_thread(move || apply_window_height(hwnd_addr, target_px, wa_top, wa_bottom));
        }
        return Ok(());
    }

    if !animate || clip::ui_kit::theme::reduced_motion() {
        let (tx, rx) = mpsc::sync_channel(1);
        let _ = app.run_on_main_thread(move || {
            apply_window_height(hwnd_addr, target_px, wa_top, wa_bottom);
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(Duration::from_millis(50));
        return Ok(());
    }

    let shared = state.inner().clone();
    let app_handle = app.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        let duration_secs = clip::ui_kit::theme::T_TOGGLE.max(0.01);
        let started = Instant::now();
        let mut last_h = start_h;

        loop {
            if unsafe { DwmFlush() }.is_err() {
                std::thread::sleep(Duration::from_millis(8));
            }
            if shared.0.lock().unwrap().height_anim_id != anim_id {
                break;
            }
            if app_handle
                .get_webview_window("main")
                .is_some_and(|w| w.is_maximized().unwrap_or(false))
            {
                break;
            }

            let t = (started.elapsed().as_secs_f32() / duration_secs).clamp(0.0, 1.0);
            let eased = clip::ui_kit::theme::ease_in_out(t);
            let h = if t >= 1.0 {
                target_px
            } else {
                (start_h as f64 + (target_px - start_h) as f64 * eased as f64).round() as i32
            };

            if h != last_h {
                last_h = h;
                let (tx, rx) = mpsc::sync_channel(1);
                let _ = app_handle.run_on_main_thread(move || {
                    apply_window_height(hwnd_addr, h, wa_top, wa_bottom);
                    let _ = tx.send(());
                });
                let _ = rx.recv_timeout(Duration::from_millis(50));
            }

            if t >= 1.0 {
                break;
            }
        }
    })
    .await;

    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle, state: State<'_, AppShared>) {
    shutdown_app(&state, &app);
}

fn is_setup_pending(shared: &AppShared) -> bool {
    shared.0.lock().unwrap().needs_setup
}

#[tauri::command]
fn get_setup_state(state: State<'_, AppShared>) -> SetupState {
    SetupState {
        needs_setup: state.0.lock().unwrap().needs_setup,
        default_install_dir: install::default_install_dir().display().to_string(),
        current_exe_dir: exe_dir().display().to_string(),
    }
}

#[tauri::command]
async fn pick_install_folder(current: Option<String>) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut dlg = rfd::FileDialog::new().set_title("Install clip in\u{2026}");
        if let Some(dir) = current
            .as_deref()
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| p.is_dir())
        {
            dlg = dlg.set_directory(dir);
        }
        dlg.pick_folder().map(|p| install::normalize_install_dir(&p).display().to_string())
    })
    .await
    .ok()
    .flatten()
}

/// Closes the single-instance mutex so the installed copy can acquire it during handoff.
fn release_instance_mutex(shared: &AppShared) {
    let raw = shared.0.lock().unwrap().instance_mutex.take();
    if let Some(raw) = raw {
        unsafe {
            let _ = CloseHandle(HANDLE(raw as *mut std::ffi::c_void));
        }
    }
}

#[tauri::command]
async fn install_app(
    app: AppHandle,
    state: State<'_, AppShared>,
    install_dir: String,
    create_shortcut: bool,
    start_with_windows: bool,
) -> Result<InstallResponse, String> {
    let shared = state.inner().clone();
    let current_exe = std::env::current_exe().map_err(|e| format!("Could not locate clip.exe: {e}"))?;
    let target = PathBuf::from(install_dir.trim());
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        install::install_into(&current_exe, &target, create_shortcut, start_with_windows)
    })
    .await
    .map_err(|e| format!("Install task failed: {e}"))??;

    let install_dir = outcome.installed_dir.display().to_string();

    if outcome.in_place {
        let cfg = outcome.config.clone();
        {
            let mut st = shared.0.lock().unwrap();
            st.needs_setup = false;
            st.cfg = cfg.clone();
            st.running = cfg.enabled;
            st.clips_count = count_clips(&outcome.clips_dir);
            st.autostart = autostart::enabled();
            // The wizard ran with recording controls disabled; turn them back on.
            if let Some(item) = &st.menu_save {
                let _ = item.set_enabled(true);
            }
            if let Some(item) = &st.menu_toggle {
                let _ = item.set_enabled(true);
            }
            if let Some(item) = &st.menu_mic {
                let _ = item.set_enabled(true);
            }
            sync_tray_locked(&st);
        }
        let tid = spawn_hotkey_thread(shared.clone(), app.clone());
        shared.0.lock().unwrap().hotkey_tid = tid;
        if cfg.enabled {
            let notify_tx = shared.0.lock().unwrap().ev_tx.clone();
            let rec = Recorder::start(cfg.clone(), make_notify(notify_tx));
            shared.0.lock().unwrap().rec = Some(rec);
        }
        log("clip (Tauri) installed & started");
        emit_config_and_state(&app, &shared);
        // The setup path skipped the updater (nothing was installed yet); start it now.
        spawn_update_checker(app.clone(), shared.clone());
        let key = hotkey_name(cfg.hotkey_vk, cfg.hotkey_mods);
        emit_toast(
            &app,
            "info",
            "clip installed",
            &format!("Ready in {install_dir}. Press {key} to save a clip."),
        );
        return Ok(InstallResponse { relaunched: false, install_dir });
    }

    // Launch the installed copy first: it retries the instance mutex for a moment, so this copy
    // can release it right after and exit without a handoff race.
    std::process::Command::new(&outcome.installed_exe)
        .arg("--installed")
        .current_dir(&outcome.installed_dir)
        .spawn()
        .map_err(|e| {
            format!(
                "clip was installed, but launching {} failed: {e}",
                outcome.installed_exe.display()
            )
        })?;
    release_instance_mutex(&shared);
    begin_exit(&shared);
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let exit_handle = handle.clone();
        let _ = handle.run_on_main_thread(move || exit_handle.exit(0));
    });
    Ok(InstallResponse { relaunched: true, install_dir })
}

/// Tells the background update checker to stop waiting and exit its thread.
fn signal_update_shutdown(shared: &AppShared) {
    let shutdown = shared.0.lock().unwrap().updates.shutdown.clone();
    let (lock, wake) = &*shutdown;
    *lock.lock().unwrap() = true;
    wake.notify_all();
}

/// Waits up to `timeout` for shutdown. `true` means clip is exiting.
fn wait_for_update_shutdown(shutdown: &Arc<(Mutex<bool>, Condvar)>, timeout: Duration) -> bool {
    let (lock, wake) = &**shutdown;
    let guard = lock.lock().unwrap();
    match wake.wait_timeout(guard, timeout) {
        Ok((guard, _)) => *guard,
        // A panicked checker must not keep this thread alive.
        Err(_) => true,
    }
}

fn emit_update_status(app: &AppHandle, shared: &AppShared) {
    let status = shared.0.lock().unwrap().updates.status.clone();
    let _ = app.emit("update-status", status);
}

/// Runs one release check. The network call happens with no lock held, and the result is stored
/// and emitted before returning.
///
/// Background checks stay silent on `404`, `401`/`403` and offline failures: a private repository
/// or a missing network is not a problem the user needs to see every six hours.
fn perform_update_check(app: &AppHandle, shared: &AppShared, manual: bool) -> update::UpdateStatus {
    let (etag, current_version) = {
        let mut st = shared.0.lock().unwrap();
        if st.needs_setup || st.updates.busy {
            return st.updates.status.clone();
        }
        st.updates.busy = true;
        if manual {
            st.updates.status.phase = update::UpdatePhase::Checking;
            st.updates.status.error = None;
        }
        (st.updates.etag.clone(), st.updates.status.current_version.clone())
    };
    if manual {
        emit_update_status(app, shared);
    }

    let result = update::check_for_update(
        update::RELEASES_LATEST_URL,
        &current_version,
        etag.as_deref(),
        false,
    );

    let status = {
        let mut st = shared.0.lock().unwrap();
        let manager = &mut st.updates;
        manager.busy = false;
        match result {
            Ok(outcome) => {
                manager.etag = outcome.etag;
                match outcome.status {
                    update::CheckStatus::NotModified => {
                        if manager.info.is_some() {
                            // A 304 means the release we already found is still current; keep
                            // offering it instead of downgrading the status to UpToDate.
                            manager.status.phase = update::UpdatePhase::Available;
                            manager.status.error = None;
                        } else if manual {
                            manager.status.phase = update::UpdatePhase::UpToDate;
                            manager.status.error = None;
                        }
                    }
                    update::CheckStatus::UpToDate { latest } => {
                        manager.info = None;
                        manager.status.phase = update::UpdatePhase::UpToDate;
                        manager.status.latest_version = Some(latest);
                        manager.status.notes_url = None;
                        manager.status.error = None;
                    }
                    update::CheckStatus::Available(info) => {
                        manager.status.phase = update::UpdatePhase::Available;
                        manager.status.latest_version = Some(info.version.clone());
                        manager.status.notes_url = Some(info.notes_url.clone());
                        manager.status.error = None;
                        manager.info = Some(info);
                    }
                    update::CheckStatus::NotPublished => {
                        if manual {
                            manager.status.phase = update::UpdatePhase::Error;
                            manager.status.error =
                                Some("No clip release is published yet.".into());
                        }
                    }
                    update::CheckStatus::Restricted => {
                        if manual {
                            manager.status.phase = update::UpdatePhase::Error;
                            manager.status.error = Some(
                                "GitHub did not allow the update check (the repository may be private)."
                                    .into(),
                            );
                        }
                    }
                }
            }
            Err(e) => {
                if manual {
                    manager.status.phase = update::UpdatePhase::Error;
                    manager.status.error = Some(e);
                }
            }
        }
        manager.status.clone()
    };

    let _ = app.emit("update-status", status.clone());
    status
}

/// Downloads, verifies and installs the pending update. Blocking: call it off the UI thread.
fn apply_update_blocking(
    app: &AppHandle,
    shared: &AppShared,
    current_exe: &std::path::Path,
) -> Result<update::UpdateStatus, String> {
    let info = {
        let mut st = shared.0.lock().unwrap();
        if st.needs_setup {
            return Err("Finish setting up clip first.".into());
        }
        if st.updates.busy {
            return Err("An update is already in progress.".into());
        }
        let Some(info) = st.updates.info.clone() else {
            return Err("No update is available yet.".into());
        };
        st.updates.busy = true;
        st.updates.status.phase = update::UpdatePhase::Downloading;
        st.updates.status.error = None;
        info
    };
    emit_update_status(app, shared);

    let outcome = update::download_and_apply_update(current_exe, &info, false);

    let status = {
        let mut st = shared.0.lock().unwrap();
        let manager = &mut st.updates;
        manager.busy = false;
        match &outcome {
            Ok(version) => {
                manager.status.phase = update::UpdatePhase::Ready;
                manager.status.latest_version = Some(version.clone());
                manager.status.error = None;
            }
            Err(e) => {
                manager.status.phase = update::UpdatePhase::Error;
                manager.status.error = Some(e.clone());
            }
        }
        manager.status.clone()
    };
    let _ = app.emit("update-status", status.clone());
    match outcome {
        Ok(_) => Ok(status),
        Err(e) => Err(e),
    }
}

/// Checks shortly after launch and then every [`update::CHECK_INTERVAL`], waking at once on
/// shutdown.
fn spawn_update_checker(app: AppHandle, shared: AppShared) {
    let shutdown = shared.0.lock().unwrap().updates.shutdown.clone();
    let _ = std::thread::Builder::new()
        .name("clip-update-check".into())
        .spawn(move || {
            if wait_for_update_shutdown(&shutdown, update::STARTUP_CHECK_DELAY) {
                return;
            }
            loop {
                perform_update_check(&app, &shared, false);
                if wait_for_update_shutdown(&shutdown, update::CHECK_INTERVAL) {
                    return;
                }
            }
        });
}

#[tauri::command]
fn get_update_status(state: State<'_, AppShared>) -> update::UpdateStatus {
    state.0.lock().unwrap().updates.status.clone()
}

/// Manual "Check for updates": reports a clear status (or error) instead of failing silently.
#[tauri::command]
async fn check_for_updates(
    app: AppHandle,
    state: State<'_, AppShared>,
) -> Result<update::UpdateStatus, String> {
    let shared = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if is_setup_pending(&shared) {
            return Err("Finish setting up clip first.".to_string());
        }
        Ok(perform_update_check(&app, &shared, true))
    })
    .await
    .map_err(|e| format!("Update check failed: {e}"))?
}

/// Downloads the pending update, verifies it, swaps `clip.exe` and restarts into the new build.
#[tauri::command]
async fn apply_update(
    app: AppHandle,
    state: State<'_, AppShared>,
) -> Result<update::UpdateStatus, String> {
    let shared = state.inner().clone();
    // Resolve before `swap_exe_files` renames the running image to `clip.exe.old`.
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Could not locate clip.exe: {e}"))?;
    let status = {
        let app = app.clone();
        let shared = shared.clone();
        let current_exe = current_exe.clone();
        tauri::async_runtime::spawn_blocking(move || {
            apply_update_blocking(&app, &shared, &current_exe)
        })
        .await
        .map_err(|e| format!("Update failed: {e}"))?
    }?;

    // Launch first: the new copy retries the instance mutex for a moment, so this copy can
    // release it right after and exit without a handoff race.
    std::process::Command::new(&current_exe)
        .arg("--updated")
        .current_dir(exe_dir())
        .spawn()
        .map_err(|e| format!("clip was updated, but restarting it failed: {e}"))?;
    release_instance_mutex(&shared);
    // Go through the normal shutdown path so the recorder, capture cancel flag and hotkeys stop
    // cleanly instead of leaving the old copy recording while the new one starts.
    let exit_shared = shared.clone();
    let exit_handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let call_handle = exit_handle.clone();
        let _ = exit_handle.run_on_main_thread(move || shutdown_app(&exit_shared, &call_handle));
    });
    Ok(status)
}

fn acquire_instance_mutex() -> Option<HANDLE> {
    unsafe {
        let handle = CreateMutexW(None, true, TRAY_MUTEX).ok();
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Some(h) = handle {
                let _ = CloseHandle(h);
            }
            None
        } else {
            handle
        }
    }
}

/// Acquires the single-instance mutex. The freshly installed copy retries briefly because the
/// downloads copy releases the mutex right after spawning it.
fn acquire_instance_mutex_with_retry(handoff: bool) -> Option<HANDLE> {
    let attempts = if handoff { 40 } else { 1 };
    for attempt in 0..attempts {
        if let Some(handle) = acquire_instance_mutex() {
            return Some(handle);
        }
        if attempt + 1 < attempts {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    None
}

fn focus_running_instance() {
    if let Some(hwnd) = find_own_window(Window::Settings.title()) {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
        return;
    }
    // The running copy closed its window to the tray, so no HWND exists: ask it to rebuild one.
    unsafe {
        if let Ok(h) = OpenEventW(EVENT_MODIFY_STATE, false, SHOW_EVENT) {
            let _ = SetEvent(h);
            let _ = CloseHandle(h);
        }
    }
}

fn main() {
    #[cfg(feature = "devtools")]
    {
        let args: Vec<String> = std::env::args().collect();
        if let Some(code) = clip::devtools::run(&args) {
            std::process::exit(code);
        }
    }
    let launched_installed = std::env::args().any(|a| a == "--installed");
    let launched_updated = std::env::args().any(|a| a == "--updated");
    // A handoff from the installer or from an in-app update: the previous copy releases the
    // mutex a moment after spawning this one.
    let handoff = launched_installed || launched_updated;
    let Some(instance_mutex) = acquire_instance_mutex_with_retry(handoff) else {
        focus_running_instance();
        return;
    };
    let instance_mutex = instance_mutex.0 as usize;

    // Only this process owns clip.exe now, so leftovers from an interrupted update are stale.
    if let Ok(current_exe) = std::env::current_exe() {
        update::cleanup_old_exe(&current_exe);
        if launched_updated {
            // The previous copy sleeps ~150 ms before shutting down, so retry removing its
            // renamed `clip.exe.old` once it releases the image lock.
            std::thread::spawn(move || {
                for _ in 0..10 {
                    if update::cleanup_backup_exe(&current_exe) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
        }
    }

    // No `data\config.json` next to the executable yet: show the install wizard and write
    // nothing to disk until the user picks a folder and installs.
    let needs_setup = !config::exists();
    if !needs_setup {
        autostart::repair();
    }
    let cfg = config::load();
    let background = std::env::args().any(|a| a == "--background");

    let (ev_tx, ev_rx) = mpsc::channel();
    let shared = AppShared(Arc::new(Mutex::new(CoreState {
        cfg: cfg.clone(),
        running: !needs_setup,
        rec: None,
        error: None,
        game: None,
        encoder: None,
        clips_count: if needs_setup { 0 } else { count_clips(&cfg.output_dir) },
        autostart: autostart::enabled(),
        thumbs: ThumbStore::default(),
        shares: ShareStore::load(app_dir().join("shares.json")),
        uploads: UploadTracker::default(),
        hotkey_tid: 0,
        hotkeys_suspended: false,
        capture_cancel: None,
        tray: None,
        menu_toggle: None,
        menu_mic: None,
        menu_save: None,
        ev_tx,
        height_anim_id: 0,
        needs_setup,
        instance_mutex: Some(instance_mutex),
        exiting: Arc::new(AtomicBool::new(false)),
        updates: UpdateManagerState::new(),
    })));

    let setup_shared = shared.clone();
    let event_shared = shared.clone();
    let protocol_shared = shared.clone();
    let window_shared = shared.clone();
    let shared_exiting = shared.0.lock().unwrap().exiting.clone();

    let res = tauri::Builder::default()
        .manage(shared)
        .register_asynchronous_uri_scheme_protocol("thumb", move |_ctx, request, responder| {
            let shared = protocol_shared.clone();
            // Decoding a cover can take ~100 ms: never block the webview's protocol thread.
            tauri::async_runtime::spawn_blocking(move || {
                responder.respond(thumb_protocol_response(&request, &shared))
            });
        })
        .on_window_event(move |window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if is_setup_pending(&window_shared) {
                    // No install yet: nothing runs in the tray, so closing must exit cleanly.
                    api.prevent_close();
                    shutdown_app(&window_shared, window.app_handle());
                }
                // Otherwise the close proceeds: the webview is destroyed and the tray (which
                // still records) keeps the process alive until the window is opened again.
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();

            // Tray-only launches build no window at all: WebView2, its GPU compositor and the
            // background thumbnail/DOM refreshes stay dead until the UI is opened.
            if needs_setup || !background {
                let _ = create_main_window(&handle);
            }

            // A second instance has no HWND to focus once the window has been destroyed, so it
            // signals this event and the running copy reopens its window on demand.
            if let Ok(ev) = unsafe { CreateEventW(None, false, false, SHOW_EVENT) } {
                let ev = ev.0 as usize;
                let exiting = setup_shared.0.lock().unwrap().exiting.clone();
                let show_handle = handle.clone();
                let _ = std::thread::Builder::new().name("clip-show".into()).spawn(move || {
                    let ev = HANDLE(ev as *mut std::ffi::c_void);
                    loop {
                        unsafe { WaitForSingleObject(ev, INFINITE) };
                        if exiting.load(Ordering::Relaxed) {
                            break;
                        }
                        let h = show_handle.clone();
                        let _ = show_handle.run_on_main_thread(move || show_main_window(&h));
                    }
                    let _ = unsafe { CloseHandle(ev) };
                });
            }

            let save_clip = MenuItem::with_id(app, "save_clip", "Save clip now", cfg.enabled && !needs_setup, None::<&str>)?;
            let open_fld = MenuItem::with_id(app, "open_folder", "Open clips folder", true, None::<&str>)?;
            let toggle_rec = CheckMenuItem::with_id(app, "toggle_rec", "Recording", !needs_setup, cfg.enabled, None::<&str>)?;
            let toggle_mic = CheckMenuItem::with_id(app, "toggle_mic", "Microphone", !needs_setup, cfg.mic_audio, None::<&str>)?;
            let open_win = MenuItem::with_id(app, "open_window", "Open clip", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;

            let menu = MenuBuilder::new(app)
                .item(&save_clip)
                .item(&open_fld)
                .separator()
                .item(&toggle_rec)
                .item(&toggle_mic)
                .item(&open_win)
                .separator()
                .item(&quit_item)
                .build()?;

            let tray_shared = setup_shared.clone();
            let tray = TrayIconBuilder::with_id("main-tray")
                .icon(tray_status_icon())
                .tooltip(tray_tooltip(&cfg, !needs_setup, None, None))
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "open_window" => show_main_window(app),
                    "save_clip" => trigger_save(&tray_shared),
                    "open_folder" => {
                        if !is_setup_pending(&tray_shared) {
                            let dir = tray_shared.0.lock().unwrap().cfg.output_dir.clone();
                            open_explorer(&dir);
                        }
                    }
                    "toggle_rec" => {
                        if !is_setup_pending(&tray_shared) {
                            perform_toggle_recording(&tray_shared, app, false);
                        }
                    }
                    "toggle_mic" => {
                        if is_setup_pending(&tray_shared) {
                            return;
                        }
                        let mut c = tray_shared.0.lock().unwrap().cfg.clone();
                        c.mic_audio = !c.mic_audio;
                        c.sanitize();
                        let _ = config::save(&c);
                        apply_config(&tray_shared, app, c, None);
                    }
                    "quit" => shutdown_app(&tray_shared, app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            {
                let mut st = setup_shared.0.lock().unwrap();
                st.tray = Some(tray);
                st.menu_toggle = Some(toggle_rec);
                st.menu_mic = Some(toggle_mic);
                st.menu_save = Some(save_clip);
            }

            spawn_event_thread(setup_shared.clone(), handle.clone(), ev_rx);

            // First run: nothing is recorded, logged or pruned until the wizard installs clip into
            // the chosen folder (the UI shows `#modal-install-wizard` instead).
            if !needs_setup {
                let tid = spawn_hotkey_thread(setup_shared.clone(), handle.clone());
                {
                    let mut st = setup_shared.0.lock().unwrap();
                    st.hotkey_tid = tid;
                }

                if cfg.enabled {
                    let notify_tx = setup_shared.0.lock().unwrap().ev_tx.clone();
                    let rec = Recorder::start(cfg.clone(), make_notify(notify_tx));
                    setup_shared.0.lock().unwrap().rec = Some(rec);
                }
            }

            if launched_installed && !needs_setup {
                let key = hotkey_name(cfg.hotkey_vk, cfg.hotkey_mods);
                emit_toast(
                    &handle,
                    "info",
                    "clip installed",
                    &format!("Ready in {}. Press {key} to save a clip.", exe_dir().display()),
                );
            }
            if launched_updated && !needs_setup {
                emit_toast(
                    &handle,
                    "info",
                    "clip updated",
                    &format!("Now running v{}.", env!("CARGO_PKG_VERSION")),
                );
            }
            if !needs_setup {
                std::thread::spawn(prune_thumb_cache);
                // First check ~15 s after launch, then every 6 hours until shutdown.
                spawn_update_checker(handle.clone(), setup_shared.clone());
                log("clip (Tauri) started");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            set_config,
            set_share_key,
            get_r2_stats,
            set_r2_ttl,
            delete_all_r2_clips,
            disconnect_r2,
            import_cf_invite,
            create_r2_invite,
            revoke_r2_invite,
            recorder_state,
            toggle_recording,
            start_recorder,
            pause_resume,
            open_folder,
            pick_folder,
            list_mics,
            set_mic_device,
            test_sound,
            get_clips_count,
            list_clips,
            get_clip_preview,
            rename_clip,
            delete_clips,
            reveal_clip,
            share_clip,
            copy_clip_file,
            set_autostart,
            begin_hotkey_capture,
            cancel_hotkey_capture,
            fit_window_height,
            quit_app,
            get_setup_state,
            pick_install_folder,
            install_app,
            get_update_status,
            check_for_updates,
            apply_update,
        ])
        .build(tauri::generate_context!());

    match res {
        Ok(app) => {
            // The window can be destroyed while the recorder keeps running, so "exit when the
            // last window closes" is replaced by waiting for `begin_exit`.
            let run_exiting = shared_exiting.clone();
            app.run(move |_app, event| {
                if let tauri::RunEvent::ExitRequested { api, .. } = event {
                    if !run_exiting.load(Ordering::Relaxed) {
                        api.prevent_exit();
                    }
                }
            });
        }
        Err(e) => log(&format!("Tauri error: {e}")),
    }

    // Ensure recorder and hotkeys are stopped cleanly on exit.
    let (old_rec, tid) = {
        let mut st = event_shared.0.lock().unwrap();
        (st.rec.take(), st.hotkey_tid)
    };
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    drop(old_rec);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.capture_mode = CaptureMode::Screen;
        cfg
    }

    #[test]
    fn active_recording_reports_typical_ram() {
        let cfg = recording_cfg();
        assert_eq!(estimated_ram_mb(&cfg, true, false, false), cfg.typical_ram_mb());
        assert!(cfg.typical_ram_mb() > 0);
    }

    #[test]
    fn games_only_idle_waiting_holds_no_buffer() {
        let cfg = recording_cfg();
        assert_eq!(estimated_ram_mb(&cfg, true, true, false), 0);
    }

    #[test]
    fn paused_holds_no_buffer() {
        let mut cfg = recording_cfg();
        cfg.enabled = false;
        assert_eq!(estimated_ram_mb(&cfg, true, false, false), 0);
    }

    #[test]
    fn stopped_holds_no_buffer() {
        let cfg = recording_cfg();
        assert_eq!(estimated_ram_mb(&cfg, false, false, false), 0);
    }

    #[test]
    fn capture_error_holds_no_buffer() {
        let cfg = recording_cfg();
        assert_eq!(estimated_ram_mb(&cfg, true, false, true), 0);
    }
}
