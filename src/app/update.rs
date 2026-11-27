//! In-app updates: release discovery over WinHTTP and verified self-replacement.
//!
//! `kai8484/clip` publishes `clip.exe` plus a `clip.exe.sha256` sidecar on every `v*` tag, so an
//! installed copy can fetch the latest release, verify the SHA-256, and swap its own executable
//! without an installer. Nothing here ever sends credentials: the GitHub API call is anonymous,
//! and every download URL (including each redirect hop) must be HTTPS on GitHub's domains.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

/// `GET /releases/latest` for the clip repository. Anonymous, read-only.
pub const RELEASES_LATEST_URL: &str = "https://api.github.com/repos/kai8484/clip/releases/latest";
/// Periodic background check: cheap (ETag-conditional), so a few requests a day is plenty.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// First background check after launch: long enough not to compete with startup work.
pub const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(15);
/// Hard ceiling for a downloaded `clip.exe`; a build that large is a bug or an attack.
pub const MAX_DOWNLOAD_BYTES: usize = 128 * 1024 * 1024;
/// The release metadata JSON is a few KB; anything past this is not a release document.
const MAX_JSON_BYTES: usize = 1024 * 1024;
/// `clip.exe.sha256` is one line.
const MAX_SIDECAR_BYTES: usize = 64 * 1024;
const MAX_REDIRECTS: usize = 5;
const ASSET_NAME: &str = "clip.exe";
const SIDECAR_NAME: &str = "clip.exe.sha256";
const OLD_EXE_NAME: &str = "clip.exe.old";
const NEW_EXE_NAME: &str = "clip.exe.new";

/// The version this build reports, used for the user agent and version comparisons.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `clip/<version>`, shared by update checks and uploads.
fn user_agent() -> PCWSTR {
    static AGENT: OnceLock<HSTRING> = OnceLock::new();
    PCWSTR::from_raw(
        AGENT
            .get_or_init(|| HSTRING::from(concat!("clip/", env!("CARGO_PKG_VERSION"))))
            .as_ptr(),
    )
}

// ---------------------------------------------------------------------------------------------
// Version parsing
// ---------------------------------------------------------------------------------------------

/// Parses `v0.1`, `0.1.0` or `v1.2.3` into a comparable triple.
///
/// Pre-release strings (`0.2.0-beta`) are rejected on purpose: clip releases are plain versions,
/// and refusing anything else keeps an unverifiable tag from being treated as newer.
pub fn parse_version(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim();
    let without_prefix = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    if without_prefix.is_empty() || without_prefix.contains(['-', '+']) {
        return None;
    }
    let mut parts = without_prefix.split('.');
    let major = parse_component(parts.next()?)?;
    let minor = parse_component(parts.next()?)?;
    let patch = match parts.next() {
        Some(p) => parse_component(p)?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn parse_component(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>().ok()
}

/// `0.1.0` style string for a parsed triple.
fn format_version(v: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

// ---------------------------------------------------------------------------------------------
// SHA-256 (pure Rust: no extra dependency, and unit-tested against NIST vectors)
// ---------------------------------------------------------------------------------------------

#[rustfmt::skip]
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(bytes.len() + 72);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for start in (0..padded.len()).step_by(64) {
        let chunk = &padded[start..start + 64];
        let mut w = [0u32; 64];
        for (i, slot) in w.iter_mut().take(16).enumerate() {
            let j = i * 4;
            *slot = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        let (mut e, mut f, mut g, mut h) = (state[4], state[5], state[6], state[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut out = String::with_capacity(64);
    for word in state {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// Normalizes a checksum string: exactly 64 hex digits, lowercase.
fn normalize_sha256(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let hex = trimmed.get(..64)?;
    // `get(..64)` would happily split a 65+ character hex string; only accept an exact 64-digit
    // value, optionally followed by whitespace.
    if trimmed.len() != 64 && !trimmed.as_bytes()[64].is_ascii_whitespace() {
        return None;
    }
    if hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// URL parsing and allow-listing
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct UrlParts {
    https: bool,
    host: String,
    port: u16,
    /// Path plus query string, always starting with `/`.
    path: String,
}

fn split_url(raw: &str) -> Option<UrlParts> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return None,
    };
    let rest = rest.split('#').next().unwrap_or("");
    let (authority, path) = match rest.find(['/', '?']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') {
        return None;
    }

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (inner, tail) = after_bracket.split_once(']')?;
        if inner.is_empty() {
            return None;
        }
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None if tail.is_empty() => default_port(https),
            None => return None,
        };
        (format!("[{inner}]"), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        // A bare hostname never contains ':', so a split here is always an explicit port.
        if h.is_empty() {
            return None;
        }
        (h.to_string(), p.parse::<u16>().ok()?)
    } else {
        (authority.to_string(), default_port(https))
    };
    if host.is_empty() {
        return None;
    }

    let path = if path.is_empty() {
        "/".to_string()
    } else if let Some(query) = path.strip_prefix('?') {
        format!("/{query}")
    } else {
        path.to_string()
    };

    Some(UrlParts { https, host, port, path })
}

fn default_port(https: bool) -> u16 {
    if https {
        443
    } else {
        80
    }
}

/// True for the loopback hosts a sandboxed test server uses.
fn is_loopback_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    matches!(lower.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// Whether `raw` is an acceptable place to download a clip update from: HTTPS on `github.com` or
/// `*.githubusercontent.com`. Plain HTTP is only allowed for loopback hosts, and only when the
/// caller opts in (unit tests), so a redirect can never downgrade the binary to cleartext.
pub fn is_allowed_download_url(raw: &str, allow_localhost: bool) -> bool {
    let Some(parts) = split_url(raw) else {
        return false;
    };
    if allow_localhost && is_loopback_host(&parts.host) {
        return true;
    }
    if !parts.https {
        return false;
    }
    let host = parts.host.to_ascii_lowercase();
    host == "github.com"
        || host == "githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

/// Resolves a `Location` header against the URL that produced it.
fn resolve_redirect(base: &str, location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.is_empty() {
        return None;
    }
    if loc.starts_with("https://") || loc.starts_with("http://") {
        return Some(loc.to_string());
    }
    let parts = split_url(base)?;
    let scheme = if parts.https { "https" } else { "http" };
    if let Some(rest) = loc.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let authority = if parts.port == default_port(parts.https) {
        parts.host.clone()
    } else {
        format!("{}:{}", parts.host, parts.port)
    };
    if loc.starts_with('/') {
        return Some(format!("{scheme}://{authority}{loc}"));
    }
    let base_path = parts.path.split('?').next().unwrap_or("/");
    let dir = &base_path[..base_path.rfind('/').map(|i| i + 1).unwrap_or(1)];
    Some(format!("{scheme}://{authority}{dir}{loc}"))
}

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

struct HttpHandle(*mut std::ffi::c_void);

impl Drop for HttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

struct HttpResponse {
    status: u32,
    etag: Option<String>,
    location: Option<String>,
    body: Vec<u8>,
}

/// Reads one response header as text (`WINHTTP_QUERY_*` info level).
unsafe fn query_header(req: *mut std::ffi::c_void, level: u32) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * std::mem::size_of::<u16>()) as u32;
    let mut index = 0u32;
    WinHttpQueryHeaders(
        req,
        level,
        PCWSTR::null(),
        Some(buf.as_mut_ptr().cast()),
        &mut len,
        &mut index,
    )
    .ok()?;
    // WinHTTP's returned length sometimes includes the terminating NUL and sometimes does not,
    // so trim both the NUL and surrounding whitespace instead of trusting an off-by-one.
    let units = (len as usize / std::mem::size_of::<u16>()).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..units]);
    let trimmed = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// One request: connect, send, read status/ETag/Location/body. Redirects are the caller's job.
fn http_get_once(
    url: &str,
    headers: &[(&str, &str)],
    max_body: usize,
    allow_localhost: bool,
) -> Result<HttpResponse, String> {
    let parts = split_url(url).ok_or_else(|| format!("Invalid update URL: {url}"))?;
    // A sandboxed test server lives on loopback; never route that through the system proxy.
    let no_proxy = allow_localhost && is_loopback_host(&parts.host);
    let access = if no_proxy {
        WINHTTP_ACCESS_TYPE_NO_PROXY
    } else {
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
    };

    unsafe {
        let session = HttpHandle(WinHttpOpen(
            user_agent(),
            access,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err(format!(
                "Failed to initialize the HTTP client ({})",
                windows::core::Error::from_win32()
            ));
        }
        // Short timeouts: an update check must never stall shutdown or the UI.
        let _ = WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 15_000);

        let conn = HttpHandle(WinHttpConnect(
            session.0,
            &HSTRING::from(parts.host.as_str()),
            parts.port,
            0,
        ));
        if conn.0.is_null() {
            return Err(format!(
                "Could not connect to {} ({})",
                parts.host,
                windows::core::Error::from_win32()
            ));
        }

        let flags = if parts.https {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let req = HttpHandle(WinHttpOpenRequest(
            conn.0,
            w!("GET"),
            &HSTRING::from(parts.path.as_str()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ));
        if req.0.is_null() {
            return Err(format!(
                "Could not create the update request ({})",
                windows::core::Error::from_win32()
            ));
        }

        // Manual redirects (every hop is validated against the download allow-list) and no
        // connection reuse: each request is a one-shot, so a pooled socket can only go stale.
        for feature in [WINHTTP_DISABLE_REDIRECTS, WINHTTP_DISABLE_KEEP_ALIVE] {
            WinHttpSetOption(
                Some(req.0 as *const std::ffi::c_void),
                WINHTTP_OPTION_DISABLE_FEATURE,
                Some(&feature.to_ne_bytes()),
            )
            .map_err(|e| format!("Could not configure the update request: {e}"))?;
        }

        let mut raw = String::new();
        for (name, value) in headers {
            raw.push_str(name);
            raw.push_str(": ");
            raw.push_str(value);
            raw.push_str("\r\n");
        }
        if !raw.is_empty() {
            let wide: Vec<u16> = raw.encode_utf16().collect();
            WinHttpAddRequestHeaders(
                req.0,
                &wide,
                WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
            )
            .map_err(|e| format!("Could not set update request headers: {e}"))?;
        }

        WinHttpSendRequest(req.0, None, None, 0, 0, 0)
            .map_err(|e| format!("Could not send the update request: {e}"))?;
        WinHttpReceiveResponse(req.0, std::ptr::null_mut())
            .map_err(|e| format!("No response from GitHub: {e}"))?;

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
        .map_err(|e| format!("Could not read the update response status: {e}"))?;

        let etag = query_header(req.0, WINHTTP_QUERY_ETAG);
        let location = query_header(req.0, WINHTTP_QUERY_LOCATION);

        let mut body = Vec::new();
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(req.0, &mut avail).is_err() || avail == 0 {
                break;
            }
            if body.len() + avail as usize > max_body {
                return Err("The update response was larger than clip expects.".into());
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            if WinHttpReadData(req.0, chunk.as_mut_ptr().cast(), avail, &mut read).is_err() || read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }

        Ok(HttpResponse { status, etag, location, body })
    }
}

/// Follows up to [`MAX_REDIRECTS`] hops, validating each target before it is requested.
fn http_get(
    url: &str,
    headers: &[(&str, &str)],
    max_body: usize,
    allow_localhost: bool,
) -> Result<HttpResponse, String> {
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let response = http_get_once(&current, headers, max_body, allow_localhost)?;
        if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        let location = response
            .location
            .ok_or_else(|| "The update server redirected without a Location header.".to_string())?;
        let next = resolve_redirect(&current, &location)
            .ok_or_else(|| "The update server sent an invalid redirect.".to_string())?;
        if !is_allowed_download_url(&next, allow_localhost) {
            return Err(format!(
                "Refused to download an update from an untrusted address: {next}"
            ));
        }
        current = next;
    }
    Err("The update server redirected too many times.".into())
}

// ---------------------------------------------------------------------------------------------
// Release JSON
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// One published release, reduced to what the updater needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    /// Normalized `major.minor.patch`.
    pub version: String,
    pub draft: bool,
    pub prerelease: bool,
    pub notes_url: String,
    pub asset_url: Option<String>,
    pub sha256_url: Option<String>,
    pub expected_sha256: Option<String>,
    pub size: u64,
}

/// Parses `/releases/latest` JSON. Returns `None` when the payload is not a usable release (no
/// parsable tag), so a schema change can never be mistaken for an update.
pub fn parse_release_json(json: &str) -> Option<ReleaseInfo> {
    let release: GhRelease = serde_json::from_str(json).ok()?;
    let triple = parse_version(&release.tag_name)?;

    let mut asset_url = None;
    let mut sha256_url = None;
    let mut expected_sha256 = None;
    let mut size = 0u64;
    for asset in &release.assets {
        if asset.name.eq_ignore_ascii_case(ASSET_NAME) {
            if !asset.browser_download_url.is_empty() {
                asset_url = Some(asset.browser_download_url.clone());
                size = asset.size;
            }
            // GitHub can compute digests itself: `sha256:<hex>`.
            if let Some(digest) = asset.digest.as_deref() {
                let hex = digest
                    .strip_prefix("sha256:")
                    .or_else(|| digest.strip_prefix("SHA256:"))
                    .unwrap_or(digest);
                expected_sha256 = normalize_sha256(hex);
            }
        } else if asset.name.eq_ignore_ascii_case(SIDECAR_NAME)
            && !asset.browser_download_url.is_empty()
        {
            sha256_url = Some(asset.browser_download_url.clone());
        }
    }

    Some(ReleaseInfo {
        tag: release.tag_name,
        version: format_version(triple),
        draft: release.draft,
        prerelease: release.prerelease,
        notes_url: release.html_url,
        asset_url,
        sha256_url,
        expected_sha256,
        size,
    })
}

// ---------------------------------------------------------------------------------------------
// Update check
// ---------------------------------------------------------------------------------------------

/// A verified update, ready to download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub tag: String,
    pub version: String,
    pub notes_url: String,
    pub asset_url: String,
    pub sha256_url: Option<String>,
    pub expected_sha256: Option<String>,
    pub size: u64,
}

/// Outcome of one check against `/releases/latest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    /// `304 Not Modified`: the cached release is still the latest.
    NotModified,
    UpToDate { latest: String },
    Available(UpdateInfo),
    /// `404`: no release published, or the repository is not visible to anonymous requests.
    NotPublished,
    /// `401`/`403`: private repository or rate limiting.
    Restricted,
}

/// A check result plus the ETag to cache for the next conditional request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    pub status: CheckStatus,
    pub etag: Option<String>,
}

/// Queries the latest release and returns it only when it is newer than `current_ver`.
///
/// Drafts and pre-releases are skipped, and the asset URL must pass the download allow-list.
pub fn check_for_update(
    api_url: &str,
    current_ver: &str,
    etag: Option<&str>,
    allow_localhost: bool,
) -> Result<CheckOutcome, String> {
    let current = parse_version(current_ver)
        .ok_or_else(|| format!("Unrecognized current version: {current_ver}"))?;

    // The API host is not a download host, so it only has to be HTTPS (or loopback in tests).
    let api_parts = split_url(api_url).ok_or_else(|| format!("Invalid update URL: {api_url}"))?;
    if !api_parts.https && !(allow_localhost && is_loopback_host(&api_parts.host)) {
        return Err("Update checks require an HTTPS API URL.".into());
    }

    let agent = format!("clip/{current_ver}");
    let mut headers = vec![
        ("Accept", "application/vnd.github+json"),
        ("User-Agent", agent.as_str()),
        ("Connection", "close"),
    ];
    if let Some(tag) = etag {
        headers.push(("If-None-Match", tag));
    }

    let response = http_get(api_url, &headers, MAX_JSON_BYTES, allow_localhost)?;
    let cached_etag = response.etag.clone().or_else(|| etag.map(str::to_string));
    match response.status {
        304 => {
            return Ok(CheckOutcome {
                status: CheckStatus::NotModified,
                etag: cached_etag,
            })
        }
        404 => return Ok(CheckOutcome { status: CheckStatus::NotPublished, etag: None }),
        401 | 403 => return Ok(CheckOutcome { status: CheckStatus::Restricted, etag: None }),
        200 => {}
        other => return Err(format!("GitHub returned HTTP {other} while checking for updates.")),
    }

    let json = String::from_utf8_lossy(&response.body);
    let Some(release) = parse_release_json(&json) else {
        return Err("GitHub returned a release clip could not read.".into());
    };
    if release.draft || release.prerelease {
        return Ok(CheckOutcome {
            status: CheckStatus::UpToDate { latest: current_ver.to_string() },
            etag: cached_etag,
        });
    }
    let Some(latest) = parse_version(&release.version) else {
        return Err("GitHub returned a release tag clip could not read.".into());
    };
    if latest <= current {
        return Ok(CheckOutcome {
            status: CheckStatus::UpToDate { latest: release.version },
            etag: cached_etag,
        });
    }

    let Some(asset_url) = release.asset_url else {
        return Err(format!("Release {} has no clip.exe asset.", release.tag));
    };
    if !is_allowed_download_url(&asset_url, allow_localhost) {
        return Err("The release download URL is not on GitHub.".into());
    }
    let sha256_url = release
        .sha256_url
        .filter(|url| is_allowed_download_url(url, allow_localhost));
    let mut expected_sha256 = release.expected_sha256;
    if expected_sha256.is_none() {
        if let Some(url) = sha256_url.as_deref() {
            // A missing sidecar is not fatal here: the download step retries and then refuses.
            expected_sha256 = fetch_sha256_sidecar(url, allow_localhost).ok();
        }
    }

    Ok(CheckOutcome {
        status: CheckStatus::Available(UpdateInfo {
            tag: release.tag,
            version: release.version,
            notes_url: release.notes_url,
            asset_url,
            sha256_url,
            expected_sha256,
            size: release.size,
        }),
        etag: cached_etag,
    })
}

/// Downloads a `clip.exe.sha256` sidecar (`<hex>  clip.exe`) and returns its hash.
fn fetch_sha256_sidecar(url: &str, allow_localhost: bool) -> Result<String, String> {
    if !is_allowed_download_url(url, allow_localhost) {
        return Err("The checksum URL is not on GitHub.".into());
    }
    let response = http_get(url, &[], MAX_SIDECAR_BYTES, allow_localhost)?;
    if response.status != 200 {
        return Err(format!(
            "Could not download the update checksum (HTTP {}).",
            response.status
        ));
    }
    let text = String::from_utf8_lossy(&response.body);
    normalize_sha256(&text).ok_or_else(|| "The update checksum is not a SHA-256 hash.".to_string())
}

// ---------------------------------------------------------------------------------------------
// Download and self-replacement
// ---------------------------------------------------------------------------------------------

/// The staging path for a downloaded binary, next to the running executable.
fn new_exe_path(current_exe: &Path) -> PathBuf {
    current_exe.with_file_name(NEW_EXE_NAME)
}

/// The backup path of the running binary during a swap.
fn old_exe_path(current_exe: &Path) -> PathBuf {
    current_exe.with_file_name(OLD_EXE_NAME)
}

/// Downloads, verifies and installs `info` over `current_exe`, returning the new version.
///
/// The running `clip.exe` is only touched after the download exceeds the size, PE header and
/// SHA-256 checks; any failure leaves the installed binary exactly as it was.
pub fn download_and_apply_update(
    current_exe: &Path,
    info: &UpdateInfo,
    allow_localhost: bool,
) -> Result<String, String> {
    if !is_allowed_download_url(&info.asset_url, allow_localhost) {
        return Err("The update download URL is not on GitHub.".into());
    }
    let expected = match info.expected_sha256.as_deref().and_then(normalize_sha256) {
        Some(sha) => sha,
        None => match info.sha256_url.as_deref() {
            Some(url) => fetch_sha256_sidecar(url, allow_localhost)?,
            None => {
                return Err(
                    "This release has no SHA-256 checksum, so clip will not install it.".into(),
                )
            }
        },
    };

    let response = http_get(&info.asset_url, &[], MAX_DOWNLOAD_BYTES, allow_localhost)?;
    if response.status != 200 {
        return Err(format!(
            "Downloading the update failed (HTTP {}).",
            response.status
        ));
    }
    let bytes = response.body;
    if bytes.is_empty() {
        return Err("The update download was empty.".into());
    }
    if info.size > 0 && bytes.len() as u64 != info.size {
        return Err(format!(
            "The update download is incomplete ({} of {} bytes).",
            bytes.len(),
            info.size
        ));
    }
    if !bytes.starts_with(b"MZ") {
        return Err("The update download is not a Windows executable.".into());
    }
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(format!(
            "The update failed its security check (SHA-256 {actual}, expected {expected})."
        ));
    }

    let new_exe = new_exe_path(current_exe);
    // Written next to clip.exe (never to %TEMP%) so the final rename stays on one volume.
    std::fs::write(&new_exe, &bytes)
        .map_err(|e| format!("Could not save the update to {}: {e}", new_exe.display()))?;
    if let Err(e) = swap_exe_files(current_exe, &new_exe) {
        let _ = std::fs::remove_file(&new_exe);
        return Err(e);
    }
    Ok(info.version.clone())
}

/// Replaces `current_exe` with an already-verified `new_exe`, keeping a rollback copy.
///
/// Windows lets a running executable be renamed, which is what makes this swap possible: the
/// running file becomes `clip.exe.old` and the verified download takes its place.
pub fn swap_exe_files(current_exe: &Path, new_exe: &Path) -> Result<(), String> {
    if !new_exe.is_file() {
        return Err(format!("The update file {} is missing.", new_exe.display()));
    }
    let old_exe = old_exe_path(current_exe);
    if old_exe.exists() {
        let _ = std::fs::remove_file(&old_exe);
    }
    std::fs::rename(current_exe, &old_exe).map_err(|e| {
        format!(
            "Could not move {} aside for the update: {e}",
            current_exe.display()
        )
    })?;
    match std::fs::rename(new_exe, current_exe) {
        Ok(()) => Ok(()),
        Err(e) => {
            let restored = std::fs::rename(&old_exe, current_exe);
            match restored {
                Ok(()) => Err(format!(
                    "Could not replace clip.exe ({e}); the previous version was restored."
                )),
                Err(rollback) => Err(format!(
                    "Could not replace clip.exe ({e}) and could not restore the previous version ({rollback}). Start {} to recover.",
                    old_exe.display()
                )),
            }
        }
    }
}

/// Removes `clip.exe.old` / `clip.exe.new` left behind by a successful or interrupted update.
pub fn cleanup_old_exe(current_exe: &Path) {
    let _ = std::fs::remove_file(old_exe_path(current_exe));
    let _ = std::fs::remove_file(new_exe_path(current_exe));
}

// ---------------------------------------------------------------------------------------------
// UI-facing status
// ---------------------------------------------------------------------------------------------

/// Update state shared with the webview.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    /// No check has run yet.
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    /// Downloaded and verified: clip is restarting into the new version.
    Ready,
    Error,
}

/// Everything the Settings row and the titlebar pill need to render.
#[derive(Clone, Debug, Serialize)]
pub struct UpdateStatus {
    pub phase: UpdatePhase,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub notes_url: Option<String>,
    pub error: Option<String>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            phase: UpdatePhase::Idle,
            current_version: current_version().to_string(),
            latest_version: None,
            notes_url: None,
            error: None,
        }
    }
}

/// Emitted as `update-available` when a background check finds a version the user has not been
/// told about yet.
#[derive(Clone, Debug, Serialize)]
pub struct UpdateAvailableEvent {
    pub version: String,
    pub tag: String,
    pub notes_url: String,
}

impl From<&UpdateInfo> for UpdateAvailableEvent {
    fn from(info: &UpdateInfo) -> Self {
        Self {
            version: info.version.clone(),
            tag: info.tag.clone(),
            notes_url: info.notes_url.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    // -----------------------------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------------------------

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clip-update-test-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_exe(tag: &str) -> Vec<u8> {
        let mut bytes = b"MZ".to_vec();
        bytes.extend_from_slice(tag.as_bytes());
        bytes.extend_from_slice(b" fake clip.exe payload");
        bytes
    }

    struct MockResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl MockResponse {
        fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self { status, headers: Vec::new(), body: body.into() }
        }

        fn json(status: u16, body: &str) -> Self {
            Self::new(status, body.as_bytes().to_vec())
                .header("Content-Type", "application/json")
        }

        fn header(mut self, name: &str, value: impl Into<String>) -> Self {
            self.headers.push((name.to_string(), value.into()));
            self
        }
    }

    /// A loopback HTTP server; each connection is answered by `handler(path, headers)`.
    struct MockServer {
        addr: SocketAddr,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<(String, String)>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl MockServer {
        fn start<F>(handler: F) -> Self
        where
            F: Fn(&str, &str) -> MockResponse + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let handler = Arc::new(handler);
            let (thread_stop, thread_requests) = (stop.clone(), requests.clone());
            let handle = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // Windows inherits the listener's non-blocking mode on accept.
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                            let Some((path, headers)) = read_request(&mut stream) else {
                                continue;
                            };
                            thread_requests.lock().unwrap().push((path.clone(), headers.clone()));
                            let response = handler(&path, &headers);
                            let _ = write_response(&mut stream, response);
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
            Self { addr, stop, requests, handle: Some(handle) }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{}", self.addr, path)
        }

        fn requests(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let head = text.split("\r\n\r\n").next()?;
        let mut lines = head.split("\r\n");
        let path = lines.next()?.split(' ').nth(1)?.to_string();
        let headers = lines.collect::<Vec<_>>().join("\r\n");
        Some((path, headers))
    }

    fn write_response(stream: &mut TcpStream, response: MockResponse) -> std::io::Result<()> {
        let reason = match response.status {
            200 => "OK",
            302 => "Found",
            304 => "Not Modified",
            403 => "Forbidden",
            404 => "Not Found",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            response.status,
            reason,
            response.body.len()
        );
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(&response.body)?;
        stream.flush()
    }

    fn release_json(tag: &str, asset_url: &str, size: u64, sha: Option<&str>) -> String {
        let digest = match sha {
            Some(sha) => format!(r#""digest": "sha256:{sha}","#),
            None => String::new(),
        };
        format!(
            r#"{{"tag_name":"{tag}","draft":false,"prerelease":false,"html_url":"https://github.com/kai8484/clip/releases/tag/{tag}","assets":[{{"name":"clip.exe","browser_download_url":"{asset_url}","size":{size},{digest}"content_type":"application/octet-stream"}}]}}"#
        )
    }

    fn update_info(asset_url: &str, bytes: &[u8]) -> UpdateInfo {
        UpdateInfo {
            tag: "v0.1.1".into(),
            version: "0.1.1".into(),
            notes_url: "https://github.com/kai8484/clip/releases/tag/v0.1.1".into(),
            asset_url: asset_url.to_string(),
            sha256_url: None,
            expected_sha256: Some(sha256_hex(bytes)),
            size: bytes.len() as u64,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Versions
    // -----------------------------------------------------------------------------------------

    #[test]
    fn parse_version_accepts_short_and_full_forms() {
        assert_eq!(parse_version("v0.1"), Some((0, 1, 0)));
        assert_eq!(parse_version("v0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("0.1.1"), Some((0, 1, 1)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("  v10.20.30 "), Some((10, 20, 30)));
        assert_eq!(parse_version("V0.1"), Some((0, 1, 0)));
    }

    #[test]
    fn parse_version_rejects_invalid_and_prerelease_tags() {
        for raw in [
            "",
            "v",
            "clip",
            "0.1.0-beta",
            "v0.1.0-rc.1",
            "0.1.0+build5",
            "0.1.0.1",
            "0..1",
            "0.1.",
            ".1.0",
            "v-1.0.0",
            "one.two.three",
        ] {
            assert_eq!(parse_version(raw), None, "expected {raw:?} to be rejected");
        }
    }

    // -----------------------------------------------------------------------------------------
    // SHA-256
    // -----------------------------------------------------------------------------------------

    #[test]
    fn sha256_hex_matches_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_hex_hashes_every_padding_length() {
        // 55/56/64 bytes straddle the padding boundary where a naive implementation breaks.
        for len in 0..=70usize {
            let hash = sha256_hex(&vec![b'a'; len]);
            assert_eq!(hash.len(), 64, "length {len}");
            assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()), "length {len}");
        }
        let million = sha256_hex(&vec![b'a'; 1_000_000]);
        assert_eq!(
            million,
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    // -----------------------------------------------------------------------------------------
    // URL allow-listing
    // -----------------------------------------------------------------------------------------

    #[test]
    fn allowed_download_urls_are_github_over_https() {
        assert!(is_allowed_download_url(
            "https://github.com/kai8484/clip/releases/download/v0.1.0/clip.exe",
            false
        ));
        assert!(is_allowed_download_url(
            "https://objects.githubusercontent.com/github-production-release-asset/1/clip.exe",
            false
        ));
        assert!(is_allowed_download_url("https://github.com", false));
    }

    #[test]
    fn disallowed_download_urls_are_rejected() {
        for raw in [
            "http://github.com/kai8484/clip/clip.exe",
            "https://example.com/clip.exe",
            "https://evil-githubusercontent.com/clip.exe",
            "https://github.com.evil.com/clip.exe",
            "https://user@github.com/clip.exe",
            "ftp://github.com/clip.exe",
            "https://raw.githubusercontent .com/clip.exe",
            "",
            "not a url",
        ] {
            assert!(
                !is_allowed_download_url(raw, false),
                "expected {raw:?} to be rejected"
            );
        }
    }

    #[test]
    fn localhost_downloads_need_an_explicit_opt_in() {
        assert!(is_allowed_download_url("http://127.0.0.1:8080/clip.exe", true));
        assert!(is_allowed_download_url("http://localhost:9000/clip.exe", true));
        assert!(!is_allowed_download_url("http://127.0.0.1:8080/clip.exe", false));
        assert!(!is_allowed_download_url("http://localhost/clip.exe", false));
    }

    #[test]
    fn redirects_resolve_relative_locations() {
        assert_eq!(
            resolve_redirect("https://api.github.com/repos/x/y", "/a/b").as_deref(),
            Some("https://api.github.com/a/b")
        );
        assert_eq!(
            resolve_redirect("https://api.github.com/repos/x/y", "z").as_deref(),
            Some("https://api.github.com/repos/x/z")
        );
        assert_eq!(
            resolve_redirect("https://api.github.com/repos/x/y", "https://objects.githubusercontent.com/a")
                .as_deref(),
            Some("https://objects.githubusercontent.com/a")
        );
        assert_eq!(resolve_redirect("https://github.com/a", ""), None);
    }

    // -----------------------------------------------------------------------------------------
    // Release JSON
    // -----------------------------------------------------------------------------------------

    #[test]
    fn parse_release_json_reads_asset_and_inline_digest() {
        let bytes = fake_exe("release");
        let sha = sha256_hex(&bytes);
        let json = release_json("v0.1.1", "https://github.com/x/clip.exe", bytes.len() as u64, Some(&sha));
        let release = parse_release_json(&json).unwrap();
        assert_eq!(release.tag, "v0.1.1");
        assert_eq!(release.version, "0.1.1");
        assert!(!release.draft);
        assert!(!release.prerelease);
        assert_eq!(release.asset_url.as_deref(), Some("https://github.com/x/clip.exe"));
        assert_eq!(release.expected_sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(release.size, bytes.len() as u64);
        assert!(release.sha256_url.is_none());
    }

    #[test]
    fn parse_release_json_reads_sidecar_asset_when_digest_missing() {
        let json = r#"{
            "tag_name": "v0.2",
            "html_url": "https://github.com/kai8484/clip/releases/tag/v0.2",
            "assets": [
                {"name": "clip.exe", "browser_download_url": "https://github.com/x/clip.exe", "size": 12},
                {"name": "clip.exe.sha256", "browser_download_url": "https://github.com/x/clip.exe.sha256", "size": 78}
            ]
        }"#;
        let release = parse_release_json(json).unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.expected_sha256, None);
        assert_eq!(
            release.sha256_url.as_deref(),
            Some("https://github.com/x/clip.exe.sha256")
        );
        assert_eq!(release.size, 12);
    }

    #[test]
    fn parse_release_json_ignores_short_or_bogus_digests() {
        let json = r#"{"tag_name":"v0.1.1","assets":[{"name":"clip.exe","browser_download_url":"https://github.com/x/clip.exe","digest":"sha256:abc"}]}"#;
        assert_eq!(parse_release_json(json).unwrap().expected_sha256, None);
        let json = r#"{"tag_name":"v0.1.1","assets":[{"name":"clip.exe","browser_download_url":"https://github.com/x/clip.exe","digest":"md5:deadbeef"}]}"#;
        assert_eq!(parse_release_json(json).unwrap().expected_sha256, None);
        // 65 hex digits must not be silently truncated to the first 64.
        let long = format!("sha256:{}0", sha256_hex(b"x"));
        let json = format!(
            r#"{{"tag_name":"v0.1.1","assets":[{{"name":"clip.exe","browser_download_url":"https://github.com/x/clip.exe","digest":"{}"}}]}}"#,
            long
        );
        assert_eq!(parse_release_json(&json).unwrap().expected_sha256, None);
        let sha = sha256_hex(b"x");
        let json = format!(
            r#"{{"tag_name":"v0.1.1","assets":[{{"name":"clip.exe","browser_download_url":"https://github.com/x/clip.exe","digest":"sha256:{}"}}]}}"#,
            sha.to_uppercase()
        );
        assert_eq!(
            parse_release_json(&json).unwrap().expected_sha256.as_deref(),
            Some(sha.as_str())
        );
    }

    #[test]
    fn parse_release_json_rejects_unusable_documents() {
        assert!(parse_release_json("not json").is_none());
        assert!(parse_release_json("{}").is_none());
        assert!(parse_release_json(r#"{"tag_name":"nightly"}"#).is_none());
        // A release without assets parses, but has nothing to install.
        let release = parse_release_json(r#"{"tag_name":"v0.3.0"}"#).unwrap();
        assert!(release.asset_url.is_none());
    }

    // -----------------------------------------------------------------------------------------
    // Swap / cleanup
    // -----------------------------------------------------------------------------------------

    #[test]
    fn swap_exe_files_replaces_current_and_keeps_a_backup() {
        let dir = temp_dir("swap");
        let current = dir.join("clip.exe");
        let new = dir.join(NEW_EXE_NAME);
        std::fs::write(&current, b"MZ old").unwrap();
        std::fs::write(&new, b"MZ new").unwrap();

        swap_exe_files(&current, &new).unwrap();

        assert_eq!(std::fs::read(&current).unwrap(), b"MZ new");
        assert_eq!(std::fs::read(dir.join(OLD_EXE_NAME)).unwrap(), b"MZ old");
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_exe_files_rolls_back_when_the_new_binary_is_gone() {
        let dir = temp_dir("swap-rollback");
        let current = dir.join("clip.exe");
        std::fs::write(&current, b"MZ old").unwrap();

        let err = swap_exe_files(&current, &dir.join(NEW_EXE_NAME)).unwrap_err();
        assert!(err.contains("missing"), "unexpected error: {err}");
        assert_eq!(std::fs::read(&current).unwrap(), b"MZ old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_exe_files_restores_clip_exe_when_the_final_rename_fails() {
        use windows::Win32::Foundation::{CloseHandle, GENERIC_READ};
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
        };

        let dir = temp_dir("swap-rollback-rename");
        let current = dir.join("clip.exe");
        let new = dir.join(NEW_EXE_NAME);
        std::fs::write(&current, b"MZ old").unwrap();
        std::fs::write(&new, b"MZ new").unwrap();

        // An open handle without FILE_SHARE_DELETE makes the final rename fail, which is exactly
        // the moment the old binary has to be put back.
        let held = unsafe {
            CreateFileW(
                &HSTRING::from(new.as_os_str()),
                GENERIC_READ.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .expect("clip.exe.new should open");

        let err = swap_exe_files(&current, &new).unwrap_err();
        unsafe {
            let _ = CloseHandle(held);
        }

        assert!(
            err.contains("previous version was restored"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&current).unwrap(), b"MZ old");
        assert_eq!(std::fs::read(&new).unwrap(), b"MZ new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_old_exe_removes_leftovers() {
        let dir = temp_dir("cleanup");
        let current = dir.join("clip.exe");
        std::fs::write(&current, b"MZ new").unwrap();
        std::fs::write(dir.join(OLD_EXE_NAME), b"MZ old").unwrap();
        std::fs::write(dir.join(NEW_EXE_NAME), b"MZ partial").unwrap();

        cleanup_old_exe(&current);

        assert!(!dir.join(OLD_EXE_NAME).exists());
        assert!(!dir.join(NEW_EXE_NAME).exists());
        assert_eq!(std::fs::read(&current).unwrap(), b"MZ new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------------------------
    // End-to-end over a loopback HTTP server
    // -----------------------------------------------------------------------------------------

    #[test]
    fn check_for_update_reports_a_newer_release() {
        let bytes = fake_exe("check");
        let sha = sha256_hex(&bytes);
        let size = bytes.len() as u64;
        let served_sha = sha.clone();
        let server = MockServer::start(move |path, _| match path {
            "/releases/latest" => MockResponse::json(
                200,
                &release_json(
                    "v0.1.1",
                    "https://github.com/kai8484/clip/releases/download/v0.1.1/clip.exe",
                    size,
                    Some(&served_sha),
                ),
            )
            .header("ETag", "\"etag-1\""),
            _ => MockResponse::new(404, ""),
        });

        let outcome = check_for_update(&server.url("/releases/latest"), "0.1.0", None, true).unwrap();
        assert_eq!(outcome.etag.as_deref(), Some("\"etag-1\""));
        match outcome.status {
            CheckStatus::Available(info) => {
                assert_eq!(info.version, "0.1.1");
                assert_eq!(info.expected_sha256.as_deref(), Some(sha.as_str()));
                assert!(info.asset_url.starts_with("https://github.com/"));
            }
            other => panic!("expected an available update, got {other:?}"),
        }
    }

    #[test]
    fn check_for_update_sends_the_cached_etag_and_handles_304() {
        let server = MockServer::start(|path, headers| match path {
            "/releases/latest" if headers.to_ascii_lowercase().contains("if-none-match: \"etag-1\"") => {
                MockResponse::new(304, "").header("ETag", "\"etag-1\"")
            }
            "/releases/latest" => MockResponse::json(200, "{}"),
            _ => MockResponse::new(404, ""),
        });

        let outcome = check_for_update(
            &server.url("/releases/latest"),
            "0.1.0",
            Some("\"etag-1\""),
            true,
        )
        .unwrap();
        assert_eq!(outcome.status, CheckStatus::NotModified);
        assert_eq!(outcome.etag.as_deref(), Some("\"etag-1\""));
    }

    #[test]
    fn check_for_update_treats_older_releases_and_drafts_as_up_to_date() {
        let bytes = fake_exe("same");
        let sha = sha256_hex(&bytes);
        let same = release_json("v0.1.0", "https://github.com/x/clip.exe", bytes.len() as u64, Some(&sha));
        let draft = same.replace(r#""draft":false"#, r#""draft":true"#);
        let prerelease = same.replace(r#""prerelease":false"#, r#""prerelease":true"#);

        for json in [same, draft, prerelease] {
            let served = json.clone();
            let server = MockServer::start(move |_, _| MockResponse::json(200, served.as_str()));
            let outcome = check_for_update(&server.url("/releases/latest"), "0.1.0", None, true).unwrap();
            assert!(
                matches!(outcome.status, CheckStatus::UpToDate { .. }),
                "expected up to date, got {:?}",
                outcome.status
            );
        }
    }

    #[test]
    fn check_for_update_distinguishes_missing_and_restricted_releases() {
        let missing = MockServer::start(|_, _| MockResponse::json(404, r#"{"message":"Not Found"}"#));
        assert_eq!(
            check_for_update(&missing.url("/releases/latest"), "0.1.0", None, true)
                .unwrap()
                .status,
            CheckStatus::NotPublished
        );
        let restricted = MockServer::start(|_, _| MockResponse::json(403, r#"{"message":"Forbidden"}"#));
        assert_eq!(
            check_for_update(&restricted.url("/releases/latest"), "0.1.0", None, true)
                .unwrap()
                .status,
            CheckStatus::Restricted
        );
    }

    #[test]
    fn check_for_update_fetches_the_sidecar_when_the_digest_is_missing() {
        let bytes = fake_exe("sidecar-check");
        let sha = sha256_hex(&bytes);
        let sidecar_body = format!("{sha}  clip.exe\n");
        // The asset URLs must point at the mock server, whose port is only known after it binds.
        let base = Arc::new(Mutex::new(String::new()));
        let base_for_handler = base.clone();
        let server = MockServer::start(move |path, _| match path {
            "/releases/latest" => {
                let base = base_for_handler.lock().unwrap();
                MockResponse::json(
                    200,
                    &format!(
                        r#"{{"tag_name":"v0.1.1","assets":[{{"name":"clip.exe","browser_download_url":"{base}/clip.exe","size":{}}},{{"name":"clip.exe.sha256","browser_download_url":"{base}/clip.exe.sha256"}}]}}"#,
                        bytes.len()
                    ),
                )
            }
            "/clip.exe.sha256" => MockResponse::new(200, sidecar_body.clone().into_bytes()),
            _ => MockResponse::new(404, ""),
        });
        *base.lock().unwrap() = server.url("");

        let outcome = check_for_update(&server.url("/releases/latest"), "0.1.0", None, true).unwrap();
        match outcome.status {
            CheckStatus::Available(info) => {
                assert_eq!(info.version, "0.1.1");
                assert_eq!(info.expected_sha256.as_deref(), Some(sha.as_str()));
                assert_eq!(info.sha256_url.as_deref(), Some(server.url("/clip.exe.sha256").as_str()));
            }
            other => panic!("expected an available update, got {other:?}"),
        }
    }

    #[test]
    fn download_follows_a_redirect_and_verifies_the_binary() {
        let bytes = fake_exe("download");
        let served = bytes.clone();
        let server = MockServer::start(move |path, _| match path {
            "/rel" => MockResponse::new(302, "").header("Location", "/bin"),
            "/bin" => MockResponse::new(200, served.clone()),
            _ => MockResponse::new(404, ""),
        });

        let dir = temp_dir("download");
        let current = dir.join("clip.exe");
        std::fs::write(&current, b"MZ old build").unwrap();

        let info = update_info(&server.url("/rel"), &bytes);
        let version = download_and_apply_update(&current, &info, true).unwrap();

        assert_eq!(version, "0.1.1");
        assert_eq!(std::fs::read(&current).unwrap(), bytes);
        assert_eq!(std::fs::read(dir.join(OLD_EXE_NAME)).unwrap(), b"MZ old build");
        // Exactly one hop: /rel redirected to /bin, and both were fetched over loopback.
        let fetched: Vec<String> = server.requests().into_iter().map(|(path, _)| path).collect();
        assert_eq!(fetched, vec!["/rel".to_string(), "/bin".to_string()]);
        assert!(!dir.join(NEW_EXE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_rejects_a_redirect_off_github() {
        let bytes = fake_exe("evil");
        let server = MockServer::start(move |_path, _| {
            MockResponse::new(302, "").header("Location", "http://example.com/clip.exe")
        });
        let dir = temp_dir("evil-redirect");
        let current = dir.join("clip.exe");
        std::fs::write(&current, b"MZ old build").unwrap();

        let err = download_and_apply_update(&current, &update_info(&server.url("/rel"), &bytes), true)
            .unwrap_err();
        assert!(err.contains("untrusted"), "unexpected error: {err}");
        assert_eq!(std::fs::read(&current).unwrap(), b"MZ old build");
        assert!(!dir.join(NEW_EXE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_rejects_a_bad_hash_without_touching_the_installed_binary() {
        let served = fake_exe("tampered");
        let server = MockServer::start(move |_, _| MockResponse::new(200, served.clone()));
        let dir = temp_dir("bad-hash");
        let current = dir.join("clip.exe");
        std::fs::write(&current, b"MZ old build").unwrap();

        // Same size and PE header as the served file, so the hash is the only failing check.
        let mut info = update_info(&server.url("/clip.exe"), &fake_exe("tampered"));
        info.expected_sha256 = Some(sha256_hex(b"something else entirely"));
        let err = download_and_apply_update(&current, &info, true).unwrap_err();

        assert!(err.contains("security check"), "unexpected error: {err}");
        assert_eq!(std::fs::read(&current).unwrap(), b"MZ old build");
        assert!(!dir.join(NEW_EXE_NAME).exists());
        assert!(!dir.join(OLD_EXE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_requires_a_checksum_a_pe_header_and_the_announced_size() {
        let server = MockServer::start(|_, _| MockResponse::new(200, b"MZ payload".to_vec()));
        let dir = temp_dir("guardrails");
        let current = dir.join("clip.exe");
        std::fs::write(&current, b"MZ old build").unwrap();
        let good = update_info(&server.url("/clip.exe"), b"MZ payload");

        let mut no_checksum = good.clone();
        no_checksum.expected_sha256 = None;
        let err = download_and_apply_update(&current, &no_checksum, true).unwrap_err();
        assert!(err.contains("no SHA-256"), "unexpected error: {err}");

        let mut wrong_size = good.clone();
        wrong_size.size = 999;
        let err = download_and_apply_update(&current, &wrong_size, true).unwrap_err();
        assert!(err.contains("incomplete"), "unexpected error: {err}");

        // A ZIP (or any non-PE) is rejected even when its hash matches the release metadata.
        let not_exe = b"PK\x03\x04 not an executable".to_vec();
        let served = not_exe.clone();
        let not_exe_server = MockServer::start(move |_, _| MockResponse::new(200, served.clone()));
        let mut info = update_info(&not_exe_server.url("/clip.exe"), &not_exe);
        info.size = 0;
        let err = download_and_apply_update(&current, &info, true).unwrap_err();
        assert!(err.contains("not a Windows executable"), "unexpected error: {err}");

        assert_eq!(std::fs::read(&current).unwrap(), b"MZ old build");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_fetches_the_sidecar_when_the_info_has_no_inline_hash() {
        let bytes = fake_exe("sidecar-download");
        let sha = sha256_hex(&bytes);
        let sidecar = format!("{sha}  clip.exe\n");
        let served = bytes.clone();
        let server = MockServer::start(move |path, _| match path {
            "/clip.exe" => MockResponse::new(200, served.clone()),
            "/clip.exe.sha256" => MockResponse::new(200, sidecar.clone().into_bytes()),
            _ => MockResponse::new(404, ""),
        });

        let dir = temp_dir("sidecar-download");
        let current = dir.join("clip.exe");
        std::fs::write(&current, b"MZ old build").unwrap();

        let mut info = update_info(&server.url("/clip.exe"), &bytes);
        info.sha256_url = Some(server.url("/clip.exe.sha256"));
        info.expected_sha256 = None;
        assert_eq!(download_and_apply_update(&current, &info, true).unwrap(), "0.1.1");
        assert_eq!(std::fs::read(&current).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
