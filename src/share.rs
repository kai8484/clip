//! Clip sharing via Cloudflare Worker + R2 (WinHTTP streaming upload + Windows clipboard copy).

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GlobalFree, BOOL, HANDLE, HGLOBAL, POINT};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Shell::DROPFILES;

const CHUNK_BYTES: usize = 64 * 1024;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
/// `DROPEFFECT_COPY` from `oleidl.h`, written to the `Preferred DropEffect` clipboard format.
const DROPEFFECT_COPY: u32 = 1;
const CLIPBOARD_OPEN_ATTEMPTS: u32 = 10;
const CLIPBOARD_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Clips below this size are shared automatically instead of waiting for an explicit upload.
pub const AUTO_UPLOAD_MAX_BYTES: u64 = 10 * 1024 * 1024;

// 9 random bytes encode to exactly 12 base64url characters (72 bits of entropy).
const SHARE_SLUG_BYTES: usize = 9;
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(h: *mut c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
}

pub struct ShareMeta<'a> {
    pub title: &'a str,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub thumb_png: Option<&'a [u8]>,
}

#[derive(Clone, Debug)]
pub struct ShareResult {
    pub url: String,
    pub video_url: String,
    pub slug: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedUrl {
    https: bool,
    host: String,
    port: u16,
    base_path: String,
    origin: String,
}

#[derive(Deserialize)]
struct WorkerUploadResponse {
    url: Option<String>,
    #[serde(rename = "videoUrl")]
    video_url: Option<String>,
    id: Option<String>,
    error: Option<String>,
}

struct HttpHandle(*mut c_void);

impl Drop for HttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// Converts a clip filename stem into a URL-safe Worker slug matching `[A-Za-z0-9_-]{1,80}`.
pub fn slugify(stem: &str) -> String {
    let mut out = String::with_capacity(stem.len().min(72));
    let mut prev_dash = false;
    for ch in stem.chars() {
        if out.len() >= 72 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            prev_dash = false;
        } else if (ch == '-' || ch == ' ' || ch == '.') && !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches(['-', '_']);
    if trimmed.is_empty() {
        "clip".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Generates an unguessable share slug from 72 bits of CSPRNG entropy, base64url-encoded without
/// padding. Always 12 characters matching `[A-Za-z0-9_-]{12}`, so it satisfies the Worker's
/// `SLUG_RE` and cannot be enumerated like a filename stem.
pub fn random_share_slug() -> Result<String, String> {
    let mut buf = [0u8; SHARE_SLUG_BYTES];
    // SAFETY: `buf` is a valid, uniquely borrowed buffer of `buf.len()` bytes, and a null
    // algorithm handle is required when using the system-preferred RNG.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err("Windows could not generate a secure share link.".into());
    }
    Ok(base64url_no_pad(&buf))
}

fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 0x3f] as char);
        }
    }
    out
}

/// Validates and normalizes a Worker base URL (`https://...`, or `http://localhost` for local dev).
pub fn normalize_base_url(raw: &str) -> Result<String, String> {
    let parsed = parse_base_url(raw)?;
    Ok(format!("{}{}", parsed.origin, parsed.base_path))
}

fn parse_base_url(raw: &str) -> Result<ParsedUrl, String> {
    let mut s = raw.trim().trim_end_matches('/').to_string();
    if s.is_empty() {
        return Err("Set Share URL in Settings first.".into());
    }
    if !s.contains("://") {
        s = format!("https://{s}");
    }
    let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("Share URL must start with https://.".into());
    };

    let (authority, raw_path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace) {
        return Err("Share URL is invalid.".into());
    }

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let end = after_bracket.find(']').ok_or_else(|| "Share URL host is invalid.".to_string())?;
        let h = &after_bracket[..end];
        let rem = &after_bracket[end + 1..];
        let p = if let Some(port_str) = rem.strip_prefix(':') {
            port_str.parse::<u16>().map_err(|_| "Share URL port is invalid.".to_string())?
        } else if rem.is_empty() {
            if https { 443 } else { 80 }
        } else {
            return Err("Share URL is invalid.".into());
        };
        (format!("[{h}]"), p)
    } else if let Some((h, port_str)) = authority.rsplit_once(':') {
        let p = port_str.parse::<u16>().map_err(|_| "Share URL port is invalid.".to_string())?;
        (h.to_string(), p)
    } else {
        (authority.to_string(), if https { 443 } else { 80 })
    };

    if host.is_empty() {
        return Err("Share URL host is empty.".into());
    }
    if !https && host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
        return Err("Share URL must use https:// (http:// is only allowed for localhost).".into());
    }

    let base_path = raw_path.trim_end_matches('/').to_string();
    let origin = if (https && port == 443) || (!https && port == 80) {
        format!("{}://{}", if https { "https" } else { "http" }, host)
    } else {
        format!("{}://{}:{}", if https { "https" } else { "http" }, host, port)
    };

    Ok(ParsedUrl {
        https,
        host: host.trim_matches(['[', ']']).to_string(),
        port,
        base_path,
        origin,
    })
}

fn percent_encode_header(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn winhttp_put_file(
    session: *mut c_void,
    parsed: &ParsedUrl,
    upload_key: &str,
    path_suffix: &str,
    headers: &[String],
    file_path: Option<&Path>,
    mem_bytes: Option<&[u8]>,
) -> Result<(u32, String), String> {
    let req_path = format!("{}{}", parsed.base_path, path_suffix);
    let total_len_u64 = match (file_path, mem_bytes) {
        (Some(p), None) => std::fs::metadata(p).map_err(|e| format!("Failed to read clip size: {e}"))?.len(),
        (None, Some(b)) => b.len() as u64,
        _ => return Err("Internal upload error.".into()),
    };
    let total_len = u32::try_from(total_len_u64)
        .map_err(|_| "Clip is too large to upload in one request (over 4 GB).".to_string())?;

    unsafe {
        let conn = HttpHandle(WinHttpConnect(session, &HSTRING::from(parsed.host.as_str()), parsed.port, 0));
        if conn.0.is_null() {
            return Err(format!("Connection failed ({})", windows::core::Error::from_win32()));
        }

        let flags = if parsed.https { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let req = HttpHandle(WinHttpOpenRequest(
            conn.0,
            w!("PUT"),
            &HSTRING::from(req_path.as_str()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ));
        if req.0.is_null() {
            return Err(format!("Failed to create HTTP request ({})", windows::core::Error::from_win32()));
        }

        let mut all_headers = String::new();
        all_headers.push_str(&format!("Authorization: Bearer {upload_key}\r\n"));
        for h in headers {
            all_headers.push_str(h);
            all_headers.push_str("\r\n");
        }
        let headers_wide: Vec<u16> = all_headers.encode_utf16().collect();
        WinHttpAddRequestHeaders(
            req.0,
            &headers_wide,
            WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
        )
        .map_err(|e| format!("Failed to set request headers: {e}"))?;

        WinHttpSendRequest(req.0, None, None, 0, total_len, 0)
            .map_err(|e| format!("Failed to send upload request: {e}"))?;

        if let Some(p) = file_path {
            let mut f = File::open(p).map_err(|e| format!("Failed to open clip file: {e}"))?;
            let mut buf = vec![0u8; CHUNK_BYTES];
            loop {
                let n = f.read(&mut buf).map_err(|e| format!("Failed to read clip file: {e}"))?;
                if n == 0 {
                    break;
                }
                write_all_winhttp(req.0, &buf[..n])?;
            }
        } else if let Some(b) = mem_bytes {
            for chunk in b.chunks(CHUNK_BYTES) {
                write_all_winhttp(req.0, chunk)?;
            }
        }

        WinHttpReceiveResponse(req.0, std::ptr::null_mut())
            .map_err(|e| format!("Failed to receive upload response: {e}"))?;

        let mut status: u32 = 0;
        let mut status_len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut status_len,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("Failed to read HTTP status: {e}"))?;

        let mut resp_bytes = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(req.0, &mut avail).is_err() || avail == 0 {
                break;
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            if WinHttpReadData(req.0, chunk.as_mut_ptr().cast(), avail, &mut read).is_err() || read == 0 {
                break;
            }
            resp_bytes.extend_from_slice(&chunk[..read as usize]);
        }

        Ok((status, String::from_utf8_lossy(&resp_bytes).into_owned()))
    }
}

unsafe fn write_all_winhttp(req: *mut c_void, mut data: &[u8]) -> Result<(), String> {
    while !data.is_empty() {
        let mut written: u32 = 0;
        WinHttpWriteData(req, Some(data.as_ptr().cast()), data.len() as u32, &mut written)
            .map_err(|e| format!("Failed while uploading data: {e}"))?;
        if written == 0 {
            return Err("Upload connection closed unexpectedly.".into());
        }
        data = &data[written as usize..];
    }
    Ok(())
}

/// Whether a just-saved clip is small enough to share automatically.
pub fn should_auto_upload(size_bytes: u64, share_url: &str, share_key: &str) -> bool {
    size_bytes > 0
        && size_bytes < AUTO_UPLOAD_MAX_BYTES
        && !share_url.trim().is_empty()
        && !share_key.trim().is_empty()
}

/// Uploads a clip (and optional thumbnail) to the configured Cloudflare Worker without re-encoding.
pub fn upload_clip(
    base_url: &str,
    upload_key: &str,
    clip_path: &Path,
    meta: &ShareMeta<'_>,
) -> Result<ShareResult, String> {
    let key = upload_key.trim();
    if key.is_empty() {
        return Err("Set Upload key in Settings first.".into());
    }
    if key.chars().any(|c| c == '\r' || c == '\n') {
        return Err("Upload key contains invalid characters.".into());
    }

    let parsed = parse_base_url(base_url)?;
    // Each share gets a fresh unguessable slug; the clip date/time stays the display title.
    let slug = random_share_slug()?;

    let session = unsafe {
        HttpHandle(WinHttpOpen(
            w!("clip/0.3"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))
    };
    if session.0.is_null() {
        return Err(format!("Failed to initialize HTTP client ({})", windows::core::Error::from_win32()));
    }
    unsafe {
        let _ = WinHttpSetTimeouts(session.0, 10_000, 15_000, 60_000, 120_000);
    }

    let mut has_thumb = false;
    if let Some(png) = meta.thumb_png.filter(|b| !b.is_empty()) {
        let thumb_headers = vec!["Content-Type: image/png".to_string()];
        if let Ok((st, _)) = winhttp_put_file(
            session.0,
            &parsed,
            key,
            &format!("/upload/{slug}.png"),
            &thumb_headers,
            None,
            Some(png),
        ) {
            has_thumb = (200..300).contains(&st);
        }
    }

    let mut headers = vec![
        "Content-Type: video/mp4".to_string(),
        format!("X-Clip-Title: {}", percent_encode_header(meta.title)),
    ];
    if let (Some(w), Some(h)) = (meta.width, meta.height) {
        if w > 0 && h > 0 {
            headers.push(format!("X-Clip-Width: {w}"));
            headers.push(format!("X-Clip-Height: {h}"));
        }
    }
    if has_thumb {
        headers.push("X-Clip-Has-Thumb: 1".to_string());
    }

    let (status, body) = winhttp_put_file(
        session.0,
        &parsed,
        key,
        &format!("/upload/{slug}.mp4"),
        &headers,
        Some(clip_path),
        None,
    )?;

    let parsed_resp = serde_json::from_str::<WorkerUploadResponse>(&body).ok();
    if status == 401 || status == 403 {
        return Err("Upload rejected: check Upload key in Settings.".into());
    }
    if !(200..300).contains(&status) {
        let detail = parsed_resp
            .and_then(|r| r.error)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| body.trim().chars().take(160).collect());
        return Err(if detail.is_empty() {
            format!("Upload failed with HTTP {status}.")
        } else {
            format!("Upload failed (HTTP {status}): {detail}")
        });
    }

    let final_slug = parsed_resp
        .as_ref()
        .and_then(|r| r.id.clone())
        .unwrap_or_else(|| slug.clone());
    let base = format!("{}{}", parsed.origin, parsed.base_path);
    let url = parsed_resp
        .as_ref()
        .and_then(|r| r.url.clone())
        .unwrap_or_else(|| format!("{base}/{final_slug}"));
    let video_url = parsed_resp
        .and_then(|r| r.video_url)
        .unwrap_or_else(|| format!("{base}/{final_slug}.mp4"));

    Ok(ShareResult {
        url,
        video_url,
        slug: final_slug,
    })
}

/// Opens the Windows clipboard, retrying while another process holds it, and closes it afterwards.
fn with_clipboard<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let mut opened = false;
    for _ in 0..CLIPBOARD_OPEN_ATTEMPTS {
        // SAFETY: `None` associates the clipboard with the calling task.
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(CLIPBOARD_RETRY_DELAY);
    }
    if !opened {
        return Err("Clipboard is busy.".into());
    }

    let res = f();
    // SAFETY: the clipboard was opened above and is closed exactly once here.
    let _ = unsafe { CloseClipboard() };
    res
}

/// Copies `bytes` into a moveable `HGLOBAL` that is ready for `SetClipboardData`.
fn alloc_clipboard_hglobal(bytes: &[u8]) -> Result<HGLOBAL, String> {
    if bytes.is_empty() {
        return Err("Clipboard payload is empty.".into());
    }
    unsafe {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
            .map_err(|e| format!("Failed to allocate clipboard memory: {e}"))?;
        let ptr = GlobalLock(hmem);
        if ptr.is_null() {
            let _ = GlobalFree(hmem);
            return Err("Failed to lock clipboard memory.".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(hmem);
        Ok(hmem)
    }
}

/// Hands `bytes` to the open clipboard as `format`, freeing the block if Windows rejects it.
fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
    let hmem = alloc_clipboard_hglobal(bytes)?;
    // SAFETY: `hmem` was just allocated above; on success the clipboard owns it.
    if let Err(e) = unsafe { SetClipboardData(format, HANDLE(hmem.0)) } {
        // SAFETY: the clipboard rejected the block, so it is still ours to free.
        let _ = unsafe { GlobalFree(hmem) };
        return Err(format!("Failed to write clipboard: {e}"));
    }
    Ok(())
}

/// Copies UTF-8 text to the Windows clipboard (`CF_UNICODETEXT`).
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    // SAFETY: `u16` has no padding, so the vector holds exactly `wide.len() * 2` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
    with_clipboard(|| {
        // SAFETY: the clipboard is open, as `EmptyClipboard` requires.
        unsafe { EmptyClipboard() }.map_err(|e| format!("Failed to clear clipboard: {e}"))?;
        set_clipboard_bytes(CF_UNICODETEXT, bytes)
    })
}

/// Removes a Win32 verbatim (`\\?\`) prefix so paths round-trip through shell APIs.
fn strip_verbatim_prefix(path: &str) -> Cow<'_, str> {
    if path.get(..8).is_some_and(|p| p.eq_ignore_ascii_case(r"\\?\UNC\")) {
        return Cow::Owned(format!(r"\\{}", &path[8..]));
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) => Cow::Borrowed(rest),
        None => Cow::Borrowed(path),
    }
}

/// Serializes a `CF_HDROP` payload: a `DROPFILES` header, the UTF-16LE path, then a double NUL.
pub fn build_hdrop_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let raw = path.to_string_lossy();
    if raw.trim().is_empty() {
        return Err("Clip path is empty.".into());
    }
    let normalized = strip_verbatim_prefix(&raw);

    let header = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: BOOL(0),
        fWide: BOOL(1),
    };
    // SAFETY: `DROPFILES` is `#[repr(C)]` plain data, so all of its bytes are initialized.
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            (&header as *const DROPFILES).cast::<u8>(),
            std::mem::size_of::<DROPFILES>(),
        )
    };

    let mut out = Vec::with_capacity(header_bytes.len() + normalized.len() * 2 + 4);
    out.extend_from_slice(header_bytes);
    for unit in normalized.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&[0u8; 4]);
    Ok(out)
}

/// Copies a clip file to the Windows clipboard as `CF_HDROP` so Explorer can paste it.
pub fn copy_file_to_clipboard(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err("Clip file not found.".into());
    }
    let payload = build_hdrop_bytes(path)?;
    let effect = DROPEFFECT_COPY.to_le_bytes();

    with_clipboard(|| {
        // SAFETY: the clipboard is open, as `EmptyClipboard` requires.
        unsafe { EmptyClipboard() }.map_err(|e| format!("Failed to clear clipboard: {e}"))?;
        set_clipboard_bytes(CF_HDROP, &payload)?;

        // Best effort: ask Explorer to copy rather than move or link the pasted file.
        // SAFETY: the format name is a NUL-terminated wide string literal.
        let effect_format = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
        if effect_format != 0 {
            let _ = set_clipboard_bytes(effect_format, &effect);
        }
        Ok(())
    })
}

/// A successfully uploaded clip, remembered so an unchanged file is never uploaded twice.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShareRecord {
    pub url: String,
    pub video_url: String,
    pub slug: String,
    pub base_url: String,
    pub size_bytes: u64,
    pub modified_ms: u64,
}

/// Persistent, reloadable index of uploaded clips keyed by normalized file path.
#[derive(Clone)]
pub struct ShareStore {
    inner: Arc<ShareStoreInner>,
}

struct ShareStoreInner {
    path: PathBuf,
    records: Mutex<HashMap<String, ShareRecord>>,
    /// Serializes disk writes so a stale snapshot can never overwrite a newer one.
    io_lock: Mutex<()>,
}

impl ShareStore {
    /// Loads the record map from `path`, starting empty when the file is missing or invalid.
    pub fn load(path: PathBuf) -> Self {
        let records = std::fs::read(&path)
            .ok()
            .and_then(|bytes| {
                let text = String::from_utf8_lossy(&bytes);
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                serde_json::from_str(text).ok()
            })
            .unwrap_or_default();
        Self {
            inner: Arc::new(ShareStoreInner {
                path,
                records: Mutex::new(records),
                io_lock: Mutex::new(()),
            }),
        }
    }

    /// Returns the record for `path` when the file and share target are both unchanged.
    pub fn get_matching(
        &self,
        path: &Path,
        size_bytes: u64,
        modified_ms: u64,
        base_url: &str,
    ) -> Option<ShareRecord> {
        let key = share_record_key(path);
        let wanted_base = base_url.trim().trim_end_matches('/');
        self.lock_records()
            .get(&key)
            .filter(|rec| {
                rec.size_bytes == size_bytes
                    && rec.modified_ms == modified_ms
                    && rec.base_url.trim_end_matches('/') == wanted_base
            })
            .cloned()
    }

    pub fn record(&self, path: &Path, record: ShareRecord) {
        self.lock_records().insert(share_record_key(path), record);
        self.persist();
    }

    pub fn remove(&self, path: &Path) {
        if self.lock_records().remove(&share_record_key(path)).is_some() {
            self.persist();
        }
    }

    pub fn migrate(&self, from: &Path, to: &Path) {
        let mut records = self.lock_records();
        let Some(record) = records.remove(&share_record_key(from)) else {
            return;
        };
        records.insert(share_record_key(to), record);
        drop(records);
        self.persist();
    }

    fn lock_records(&self) -> std::sync::MutexGuard<'_, HashMap<String, ShareRecord>> {
        self.inner
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Serializes the map, then atomically replaces the backing file.
    ///
    /// `io_lock` is held across serialize-write-rename so concurrent saves keep their order, while
    /// `records` is only locked long enough to snapshot the map. Readers never wait on disk I/O.
    fn persist(&self) {
        let _io = self
            .inner
            .io_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let json = {
            let records = self.lock_records();
            match serde_json::to_vec(&*records) {
                Ok(json) => json,
                Err(_) => return,
            }
        };

        if let Some(parent) = self.inner.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = temp_sibling_path(&self.inner.path);
        if std::fs::write(&tmp, &json).is_err() {
            return;
        }
        if std::fs::rename(&tmp, &self.inner.path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Normalizes a clip path into a stable, case-insensitive store key.
fn share_record_key(path: &Path) -> String {
    let raw = path.to_string_lossy();
    strip_verbatim_prefix(&raw).to_lowercase()
}

fn temp_sibling_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "share-records.json".into());
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_keeps_clean_slugs_and_sanitizes_spaces() {
        assert_eq!(slugify("clip_2026-11-21_17-46-21"), "clip_2026-11-21_17-46-21");
        assert_eq!(slugify("sick clutch 2.0"), "sick-clutch-2-0");
        assert_eq!(slugify("  ---hello___world!!  "), "hello___world");
        assert_eq!(slugify("???"), "clip");
    }

    #[test]
    fn base64url_no_pad_matches_rfc4648_vectors() {
        assert_eq!(base64url_no_pad(b""), "");
        assert_eq!(base64url_no_pad(b"f"), "Zg");
        assert_eq!(base64url_no_pad(b"fo"), "Zm8");
        assert_eq!(base64url_no_pad(b"foo"), "Zm9v");
        assert_eq!(base64url_no_pad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_no_pad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_no_pad(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url_no_pad(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn random_share_slug_is_url_safe_and_fixed_length() {
        let slug = random_share_slug().unwrap();
        assert_eq!(slug.len(), 12);
        assert!(slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(slugify(&slug).len() <= 80);
    }

    #[test]
    fn random_share_slug_is_unique_across_calls() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let slug = random_share_slug().unwrap();
            assert_eq!(slug.len(), 12);
            assert!(seen.insert(slug), "duplicate share slug generated");
        }
    }

    #[test]
    fn should_auto_upload_respects_size_boundaries() {
        let base = "https://clips.example.com";
        let key = "upload-key";
        assert!(!should_auto_upload(0, base, key));
        assert!(should_auto_upload(1, base, key));
        assert!(should_auto_upload(AUTO_UPLOAD_MAX_BYTES - 1, base, key));
        assert!(!should_auto_upload(AUTO_UPLOAD_MAX_BYTES, base, key));
        assert!(!should_auto_upload(AUTO_UPLOAD_MAX_BYTES + 1, base, key));
    }

    #[test]
    fn should_auto_upload_requires_share_target() {
        assert!(!should_auto_upload(1024, "", "upload-key"));
        assert!(!should_auto_upload(1024, "   ", "upload-key"));
        assert!(!should_auto_upload(1024, "https://clips.example.com", ""));
        assert!(!should_auto_upload(1024, "https://clips.example.com", " \t"));
        assert!(should_auto_upload(1024, " https://clips.example.com ", " upload-key "));
    }

    fn hdrop_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn hdrop_path(bytes: &[u8]) -> String {
        assert_eq!(hdrop_u32(bytes, 0), 20, "pFiles must point past the 20-byte header");
        let body = &bytes[20..bytes.len() - 4];
        let units: Vec<u16> = body
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn hdrop_bytes_have_dropfiles_header() {
        let path = Path::new(r"C:\clips\clip.mp4");
        let bytes = build_hdrop_bytes(path).unwrap();
        assert_eq!(hdrop_u32(&bytes, 0), 20); // pFiles
        assert_eq!(hdrop_u32(&bytes, 4), 0); // pt.x
        assert_eq!(hdrop_u32(&bytes, 8), 0); // pt.y
        assert_eq!(hdrop_u32(&bytes, 12), 0); // fNC
        assert_eq!(hdrop_u32(&bytes, 16), 1); // fWide
        let expected_len = 20 + path.to_string_lossy().encode_utf16().count() * 2 + 4;
        assert_eq!(bytes.len(), expected_len);
    }

    #[test]
    fn hdrop_bytes_strip_verbatim_prefixes() {
        let plain = build_hdrop_bytes(Path::new(r"C:\clips\clip.mp4")).unwrap();
        assert_eq!(hdrop_path(&plain), r"C:\clips\clip.mp4");

        let drive = build_hdrop_bytes(Path::new(r"\\?\C:\clips\clip.mp4")).unwrap();
        assert_eq!(hdrop_path(&drive), r"C:\clips\clip.mp4");
        assert_eq!(drive[20..], plain[20..]);

        let unc = build_hdrop_bytes(Path::new(r"\\?\UNC\server\share\clip.mp4")).unwrap();
        assert_eq!(hdrop_path(&unc), r"\\server\share\clip.mp4");
    }

    #[test]
    fn hdrop_bytes_encode_utf16le_with_double_nul() {
        let bytes = build_hdrop_bytes(Path::new(r"C:\clips\ü.mp4")).unwrap();
        assert_eq!(hdrop_path(&bytes), r"C:\clips\ü.mp4");
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);

        let expected: Vec<u8> = r"C:\clips\ü.mp4"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(&bytes[20..20 + expected.len()], expected.as_slice());
    }

    #[test]
    fn hdrop_bytes_reject_empty_path() {
        assert!(build_hdrop_bytes(Path::new("")).is_err());
        assert!(build_hdrop_bytes(Path::new("   ")).is_err());
    }

    #[test]
    fn normalize_base_url_handles_schemes_and_trailing_slashes() {
        assert_eq!(
            normalize_base_url("clips.example.com/").unwrap(),
            "https://clips.example.com"
        );
        assert_eq!(
            normalize_base_url("https://clips.example.com///").unwrap(),
            "https://clips.example.com"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:8787/").unwrap(),
            "http://127.0.0.1:8787"
        );
        assert!(normalize_base_url("http://clips.example.com").is_err());
        assert!(normalize_base_url("").is_err());
    }

    fn test_store_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clip-share-store-{}-{}-{}",
            std::process::id(),
            tag,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_record(base_url: &str) -> ShareRecord {
        ShareRecord {
            url: format!("{base_url}/abc123"),
            video_url: format!("{base_url}/abc123.mp4"),
            slug: "abc123".into(),
            base_url: base_url.into(),
            size_bytes: 1024,
            modified_ms: 1_700_000_000_000,
        }
    }

    fn matches(store: &ShareStore, path: &Path, base_url: &str) -> bool {
        store
            .get_matching(path, 1024, 1_700_000_000_000, base_url)
            .is_some()
    }

    #[test]
    fn share_store_load_missing_file_starts_empty() {
        let dir = test_store_dir("missing");
        let store = ShareStore::load(dir.join("share-records.json"));
        assert!(!matches(&store, &dir.join("clip.mp4"), "https://clips.example.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_store_load_tolerates_invalid_json() {
        let dir = test_store_dir("invalid");
        let path = dir.join("share-records.json");
        std::fs::write(&path, b"not json").unwrap();
        let store = ShareStore::load(path.clone());
        let clip = Path::new(r"C:\clips\clip.mp4");
        assert!(!matches(&store, clip, "https://clips.example.com"));

        store.record(clip, sample_record("https://clips.example.com"));
        assert!(matches(&ShareStore::load(path), clip, "https://clips.example.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_store_load_strips_utf8_bom() {
        let dir = test_store_dir("bom");
        let path = dir.join("share-records.json");
        let clip = Path::new(r"C:\clips\clip.mp4");
        let mut map = HashMap::new();
        map.insert(share_record_key(clip), sample_record("https://clips.example.com"));
        let mut json = b"\xEF\xBB\xBF".to_vec();
        json.extend_from_slice(&serde_json::to_vec(&map).unwrap());
        std::fs::write(&path, json).unwrap();

        assert!(matches(&ShareStore::load(path), clip, "https://clips.example.com"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_store_record_persists_across_reloads() {
        let dir = test_store_dir("persist");
        let path = dir.join("share-records.json");
        let clip = Path::new(r"C:\clips\clip.mp4");
        let store = ShareStore::load(path.clone());
        store.record(clip, sample_record("https://clips.example.com"));
        assert!(!path.with_file_name("share-records.json.tmp").exists());

        let reloaded = ShareStore::load(path);
        let record = reloaded
            .get_matching(clip, 1024, 1_700_000_000_000, "https://clips.example.com")
            .expect("record must survive a reload");
        assert_eq!(record.slug, "abc123");
        assert_eq!(record.url, "https://clips.example.com/abc123");
        assert_eq!(record.video_url, "https://clips.example.com/abc123.mp4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_store_get_matching_requires_unchanged_file_and_target() {
        let dir = test_store_dir("matching");
        let store = ShareStore::load(dir.join("share-records.json"));
        let clip = Path::new(r"C:\clips\clip.mp4");
        let base = "https://clips.example.com";
        store.record(clip, sample_record("https://clips.example.com/"));

        assert!(matches(&store, clip, base));
        assert!(matches(&store, clip, " https://clips.example.com/ "));
        assert!(store.get_matching(clip, 1025, 1_700_000_000_000, base).is_none());
        assert!(store.get_matching(clip, 1024, 1_700_000_000_001, base).is_none());
        assert!(store.get_matching(clip, 1024, 1_700_000_000_000, "https://other.example.com").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_store_migrate_moves_and_persists_record() {
        let dir = test_store_dir("migrate");
        let path = dir.join("share-records.json");
        let store = ShareStore::load(path.clone());
        let base = "https://clips.example.com";
        let from = Path::new(r"C:\clips\old.mp4");
        let to = Path::new(r"C:\clips\new.mp4");
        store.record(from, sample_record(base));
        store.migrate(from, to);

        assert!(!matches(&store, from, base));
        assert!(matches(&store, to, base));
        let reloaded = ShareStore::load(path);
        assert!(!matches(&reloaded, from, base));
        assert!(matches(&reloaded, to, base));

        store.migrate(Path::new(r"C:\clips\ghost.mp4"), Path::new(r"C:\clips\ghost2.mp4"));
        assert!(!matches(&store, Path::new(r"C:\clips\ghost2.mp4"), base));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_store_remove_deletes_and_persists() {
        let dir = test_store_dir("remove");
        let path = dir.join("share-records.json");
        let store = ShareStore::load(path.clone());
        let clip = Path::new(r"C:\clips\clip.mp4");
        let base = "https://clips.example.com";
        store.record(clip, sample_record(base));
        store.remove(clip);

        assert!(!matches(&store, clip, base));
        assert!(!matches(&ShareStore::load(path), clip, base));
        store.remove(clip);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn share_store_keys_ignore_case_and_verbatim_prefix() {
        let dir = test_store_dir("keys");
        let store = ShareStore::load(dir.join("share-records.json"));
        let base = "https://clips.example.com";
        store.record(Path::new(r"\\?\C:\Clips\Clip.MP4"), sample_record(base));

        assert!(matches(&store, Path::new(r"C:\clips\clip.mp4"), base));
        assert!(matches(&store, Path::new(r"\\?\C:\CLIPS\CLIP.mp4"), base));
        assert_eq!(share_record_key(Path::new(r"\\?\c:\Clips\Clip.MP4")), r"c:\clips\clip.mp4");
        assert_eq!(
            share_record_key(Path::new(r"\\?\UNC\Server\Share\Clip.MP4")),
            r"\\server\share\clip.mp4"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
