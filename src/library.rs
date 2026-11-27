//! Clip gallery helpers: video detection, display names, file naming/validation, Recycle Bin
//! deletion, and reading duration/dimensions/codec plus an RGBA cover thumbnail.
//!
//! Covers are decoded in one frame: CPU RGB32 `IMFSourceReader` first (~60 ms), GPU decode as a
//! fallback (handles codecs the CPU path rejects), and Windows Shell thumbnails last (handles
//! codecs like HEVC without the Media Foundation extension).

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use windows::core::{Interface, GUID, HSTRING, PROPVARIANT};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Media::MediaFoundation::*;

use crate::mf::Mf;

pub const VIDEO_EXTS: [&str; 6] = ["mp4", "mov", "m4v", "mkv", "wmv", "avi"];

/// Characters disallowed by Windows in file or folder names.
pub const BAD_NAME: &str = "<>:\"/\\|?*";

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
const EOS: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;

/// Largest frame dimension accepted from a decoder (protects `to_rgba` allocations).
const MAX_DIM: u32 = 16_384;

// --- Names and files -------------------------------------------------------------------

pub fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// `"clip_2026-11-15_23-38-30"` (or legacy `"Clippy_..."`) -> `"15/11 · 23:38:30"`.
pub fn pretty_name(p: &Path) -> String {
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let rest = stem.strip_prefix("clip_").or_else(|| stem.strip_prefix("Clippy_"));
    if let Some(rest) = rest {
        let parts: Vec<&str> = rest.split(['_', '-']).collect();
        if parts.len() >= 6 && parts[..6].iter().all(|x| x.chars().all(|c| c.is_ascii_digit())) {
            let extra = if parts.len() > 6 { format!(" ({})", parts[6..].join(" ")) } else { String::new() };
            return format!("{}/{} · {}:{}:{}{}", parts[2], parts[1], parts[3], parts[4], parts[5], extra);
        }
    }
    stem
}

/// Valid Windows folder name: non-empty, no trailing period, and no forbidden characters.
pub fn folder_name_ok(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && !n.ends_with('.') && !n.chars().any(|c| BAD_NAME.contains(c) || c.is_control())
}

/// Valid Windows filename stem: passes `folder_name_ok` and rejects reserved DOS device names
/// (`CON`, `PRN`, `AUX`, `NUL`, `COM1`..`9`, `LPT1`..`9`).
pub fn filename_ok(name: &str) -> bool {
    if !folder_name_ok(name) {
        return false;
    }
    let trimmed = name.trim();
    let base = trimmed.split('.').next().unwrap_or(trimmed).to_ascii_uppercase();
    !matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Renames a clip in place keeping its folder and extension.
pub fn rename_clip(from: &Path, new_name: &str) -> Result<PathBuf, String> {
    let parent = from.parent().ok_or_else(|| "Clip has no parent folder.".to_string())?;
    let ext = from.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut stem = new_name.trim();
    if !ext.is_empty() {
        if let Some((before, suffix)) = stem.rsplit_once('.') {
            if suffix.eq_ignore_ascii_case(ext) {
                stem = before.trim();
            }
        }
    }
    if !filename_ok(stem) {
        return Err("Enter a valid name without < > : \" / \\ | ? * or a trailing period.".into());
    }
    let file_name = if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    };
    let to = parent.join(&file_name);
    if to == from {
        return Ok(to);
    }
    let case_only = from
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(&file_name));
    if !case_only && to.exists() {
        return Err("A clip with that name already exists.".into());
    }
    retry(|| std::fs::rename(from, &to))
        .map_err(|e| format!("Couldn't rename clip (it may be open in another program): {e}"))?;
    Ok(to)
}

/// Retries `op` briefly in case a thumbnail reader still has the file handle open.
fn retry<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut left = 20;
    loop {
        match op() {
            Err(_) if left > 0 => {
                left -= 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

/// Sends a file or folder to the Recycle Bin (Windows prompts if the drive has no Recycle Bin).
pub fn recycle(p: &Path) -> std::io::Result<()> {
    use windows::Win32::UI::Shell::*;
    let from: Vec<u16> = p.as_os_str().encode_wide().chain([0, 0]).collect();
    retry(|| {
        let mut op = SHFILEOPSTRUCTW {
            wFunc: FO_DELETE,
            pFrom: windows::core::PCWSTR(from.as_ptr()),
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING).0 as u16,
            ..Default::default()
        };
        let r = unsafe { SHFileOperationW(&mut op) };
        if r == 0 && !op.fAnyOperationsAborted.as_bool() && !p.exists() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("couldn't send to Recycle Bin ({r})")))
        }
    })
}

// --- Media Foundation probe and cover ---------------------------------------------------

/// What is known about a file without decoding it.
struct Probe {
    duration: i64,
    width: u32,
    height: u32,
    subtype: GUID,
}

impl Probe {
    fn codec(&self) -> &'static str {
        if self.subtype == MFVideoFormat_H264 {
            "H.264"
        } else if self.subtype == MFVideoFormat_HEVC {
            "HEVC"
        } else {
            "other"
        }
    }
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

/// `low_latency`: the decoder outputs each frame as soon as it has it (for single frames).
unsafe fn reader(path: &Path, video_processing: bool, d3d: Option<&IMFDXGIDeviceManager>, low_latency: bool) -> Result<IMFSourceReader> {
    let mut a = None;
    MFCreateAttributes(&mut a, 4)?;
    let a = a.unwrap();
    if video_processing {
        a.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
    }
    if low_latency {
        a.SetUINT32(&MF_LOW_LATENCY, 1)?;
    }
    if let Some(m) = d3d {
        a.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, m)?;
        a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
    }
    Ok(MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &a)?)
}

/// GPU device for Media Foundation: decode and scale without going through the CPU.
unsafe fn d3d_manager() -> Result<IMFDXGIDeviceManager> {
    let mut device = None;
    D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None,
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        None,
    )?;
    let device = device.ok_or_else(|| anyhow!("no D3D11"))?;
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let mut token = 0u32;
    let mut mgr = None;
    MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
    let mgr = mgr.unwrap();
    mgr.ResetDevice(&device, token)?;
    Ok(mgr)
}

unsafe fn seek(r: &IMFSourceReader, t: i64) -> Result<()> {
    r.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(t.max(0)))?;
    Ok(())
}

fn probe(path: &Path) -> Result<Probe> {
    unsafe {
        let r = reader(path, false, None, false)?;
        let duration = r
            .GetPresentationAttribute(SOURCE, &MF_PD_DURATION)
            .ok()
            .and_then(|p| u64::try_from(&p).ok())
            .unwrap_or(0) as i64;
        let vt = r.GetNativeMediaType(VIDEO, 0).map_err(|_| anyhow!("the file has no video"))?;
        let (width, height) = unpack(vt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0));
        Ok(Probe { duration, width, height, subtype: vt.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default() })
    }
}

/// Basic metadata and decoded cover thumbnail for the clips gallery.
pub struct ClipMedia {
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub codec: &'static str,
    pub cover_rgba: Option<(u32, u32, Vec<u8>)>,
}

/// Reads duration, dimensions, codec, and extracts an RGBA8 cover thumbnail up to `max_w` px wide.
pub fn probe_and_cover(p: &Path, max_w: u32) -> Result<ClipMedia, String> {
    let _mf = Mf::init();
    let pr = probe(p).map_err(|e| e.to_string())?;
    Ok(ClipMedia {
        duration_ms: (pr.duration.max(0) / 10_000) as u64,
        width: pr.width,
        height: pr.height,
        codec: pr.codec(),
        cover_rgba: decode_cover(p, &pr, max_w),
    })
}

/// Cover thumbnail: keyframe just before 1 s (or midpoint if shorter), decoded in one frame.
fn decode_cover(p: &Path, pr: &Probe, max_w: u32) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h) = fit(pr, max_w);
    Video::open_cpu(p, w, h)
        .or_else(|_| Video::open(p, w, h, true))
        .ok()
        .and_then(|mut v| v.key_frame_before((pr.duration / 2).min(10_000_000)).ok().flatten())
        .or_else(|| shell_cover_image(p, max_w))
}

fn fit(p: &Probe, w: u32) -> (u32, u32) {
    let w = w.min(p.width.max(2)) & !1;
    let h = ((w as u64 * p.height as u64 / p.width.max(1) as u64) as u32).max(2) & !1;
    (w, h)
}

struct Video {
    r: IMFSourceReader,
    w: u32,
    h: u32,
    stride: i32,
}

impl Video {
    /// CPU decoder for a single frame (cover thumbnails): opens in ~60 ms vs ~300 ms for GPU.
    fn open_cpu(path: &Path, w: u32, h: u32) -> Result<Video> {
        unsafe { Self::configure(path, w, h, None, true) }
    }

    /// With `still` enabled, the decoder emits each frame immediately without buffering,
    /// saving ~20 ms per seek.
    fn open(path: &Path, w: u32, h: u32, still: bool) -> Result<Video> {
        unsafe {
            // Try GPU decode first; fall back to CPU on any failure.
            if let Ok(mgr) = d3d_manager() {
                if let Ok(mut v) = Self::configure(path, w, h, Some(&mgr), still) {
                    if v.next().is_ok() {
                        seek(&v.r, 0)?;
                        return Ok(v);
                    }
                }
            }
            Self::configure(path, w, h, None, still)
        }
    }

    unsafe fn configure(path: &Path, w: u32, h: u32, mgr: Option<&IMFDXGIDeviceManager>, still: bool) -> Result<Video> {
        let r = reader(path, true, mgr, still)?;
        r.SetStreamSelection(MF_SOURCE_READER_ANY_STREAM.0 as u32, false)?;
        r.SetStreamSelection(VIDEO, true)?;
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        r.SetCurrentMediaType(VIDEO, None, &t).map_err(|_| {
            anyhow!("Windows can't decode this video. If it's HEVC, install \"HEVC Video Extensions\" from Microsoft Store.")
        })?;
        let cur = r.GetCurrentMediaType(VIDEO)?;
        let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or((w * 4) as i32);
        Ok(Video { r, w, h, stride })
    }

    /// Re-reads the frame size and stride after a mid-stream format change.
    fn refresh_type(&mut self) -> Result<()> {
        unsafe {
            let cur = self.r.GetCurrentMediaType(VIDEO)?;
            let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(pack(self.w, self.h));
            let (w, h) = ((size >> 32) as u32, size as u32);
            if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
                return Err(anyhow!("invalid video frame size {w}x{h}"));
            }
            self.w = w;
            self.h = h;
            self.stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or((w * 4) as i32);
        }
        Ok(())
    }

    /// Next decoded frame `(pts, duration, sample)`, or `None` at EOS.
    fn next(&mut self) -> Result<Option<(i64, i64, IMFSample)>> {
        unsafe {
            loop {
                let (mut flags, mut ts, mut s) = (0u32, 0i64, None);
                self.r.ReadSample(VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut s))?;
                // A decoder can switch output format mid-stream; refresh the geometry before use.
                if flags & (MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32) != 0 {
                    self.refresh_type()?;
                }
                if let Some(s) = s {
                    let d = s.GetSampleDuration().unwrap_or(166_666);
                    return Ok(Some((ts, d, s)));
                }
                if flags & EOS != 0 {
                    return Ok(None);
                }
            }
        }
    }

    /// Converts a decoded RGB32 sample into straight RGBA8 pixels.
    fn to_rgba(&self, s: &IMFSample) -> Result<(u32, u32, Vec<u8>)> {
        let (w, h) = (self.w as usize, self.h as usize);
        if w == 0 || h == 0 || w > MAX_DIM as usize || h > MAX_DIM as usize {
            return Err(anyhow!("invalid video frame size {w}x{h}"));
        }
        let count = w.checked_mul(h).ok_or_else(|| anyhow!("video frame size overflow"))?;
        let out_len = count.checked_mul(4).ok_or_else(|| anyhow!("video frame size overflow"))?;
        let row = w.checked_mul(4).ok_or_else(|| anyhow!("video row size overflow"))?;
        let mut rgba = vec![0u8; out_len];
        unsafe {
            let b = s.ConvertToContiguousBuffer()?;
            // Copies `h` rows of RGB32 pixels; `pitch` may be negative for bottom-up buffers.
            let copy = |base: *const u8, pitch: i32, out: &mut [u8]| -> Result<()> {
                for y in 0..h {
                    let src = std::slice::from_raw_parts(base.offset(y as isize * pitch as isize), row);
                    for (d, s) in out[y * row..(y + 1) * row].as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
                        d[0] = s[2];
                        d[1] = s[1];
                        d[2] = s[0];
                        d[3] = 255;
                    }
                }
                Ok(())
            };
            if let Ok(b2) = b.cast::<IMF2DBuffer2>() {
                let (mut p, mut pitch, mut start, mut len) =
                    (std::ptr::null_mut(), 0i32, std::ptr::null_mut(), 0u32);
                b2.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut p, &mut pitch, &mut start, &mut len)?;
                let r = check_rows(p, pitch, start, len as usize, row, h).and_then(|_| copy(p, pitch, &mut rgba));
                let unlock = b2.Unlock2D();
                r?;
                unlock?;
            } else if let Ok(b2) = b.cast::<IMF2DBuffer>() {
                let mut p = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut p, &mut pitch)?;
                // Lock2D does not expose the buffer start, so measure the plane from `p` itself.
                let len = b.GetCurrentLength().or_else(|_| b.GetMaxLength()).unwrap_or(0) as usize;
                let r = check_rows(p, pitch, p, len, row, h).and_then(|_| copy(p, pitch, &mut rgba));
                let unlock = b2.Unlock2D();
                r?;
                unlock?;
            } else {
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                b.Lock(&mut p, None, Some(&mut len))?;
                let r = check_contiguous(p, self.stride, len as usize, row, h)
                    .and_then(|base| copy(base, self.stride, &mut rgba));
                let unlock = b.Unlock();
                r?;
                unlock?;
            }
        }
        Ok((self.w, self.h, rgba))
    }

    /// Decodes the keyframe at or before `t` in a single frame read.
    fn key_frame_before(&mut self, t: i64) -> Result<Option<(u32, u32, Vec<u8>)>> {
        unsafe { seek(&self.r, t)? };
        self.next()?.map(|(_, _, s)| self.to_rgba(&s)).transpose()
    }
}

/// Validates that `h` rows of `row` bytes at signed `pitch`, starting at `p`, stay inside the
/// locked region `[start, start + len)`.
unsafe fn check_rows(p: *const u8, pitch: i32, start: *const u8, len: usize, row: usize, h: usize) -> Result<()> {
    if p.is_null() || start.is_null() || pitch == 0 {
        return Err(anyhow!("decoder returned an invalid 2D buffer"));
    }
    let stride = pitch.unsigned_abs() as usize;
    if stride < row {
        return Err(anyhow!("decoder returned an invalid RGB32 pitch ({pitch} < {row})"));
    }
    let (p, start) = (p as usize, start as usize);
    let offset = p.checked_sub(start).ok_or_else(|| anyhow!("2D buffer pointer precedes its start"))?;
    let dist = (h - 1).checked_mul(stride).ok_or_else(|| anyhow!("video frame dimensions overflow"))?;
    let end = offset.checked_add(dist).and_then(|v| v.checked_add(row)).ok_or_else(|| anyhow!("video frame dimensions overflow"))?;
    // Positive pitch walks forward from `p`; negative pitch walks backward but each row still reads forward.
    let ok = if pitch > 0 {
        end <= len
    } else {
        offset >= dist && offset.checked_add(row).is_some_and(|v| v <= len)
    };
    if !ok {
        return Err(anyhow!("decoded frame is shorter than its dimensions"));
    }
    Ok(())
}

/// Validates a plain contiguous RGB32 buffer and returns the pointer of row 0.
unsafe fn check_contiguous(p: *const u8, stride: i32, len: usize, row: usize, h: usize) -> Result<*const u8> {
    if p.is_null() || stride == 0 || (stride.unsigned_abs() as usize) < row {
        return Err(anyhow!("decoder returned an invalid RGB32 stride"));
    }
    let pitch = stride.unsigned_abs() as usize;
    let dist = (h - 1).checked_mul(pitch).ok_or_else(|| anyhow!("video frame dimensions overflow"))?;
    let span = dist.checked_add(row).ok_or_else(|| anyhow!("video frame dimensions overflow"))?;
    if span > len {
        return Err(anyhow!("decoded frame is shorter than its dimensions ({len} < {span})"));
    }
    // Bottom-up buffers store the last row first, so start `dist` bytes into the buffer.
    Ok(if stride < 0 { p.add(dist) } else { p })
}

/// Windows Shell thumbnail fallback: works for codecs Media Foundation cannot open.
fn shell_cover_image(p: &Path, max_w: u32) -> Option<(u32, u32, Vec<u8>)> {
    let path = p.to_path_buf();
    std::thread::spawn(move || unsafe {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::SIZE;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
            BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::Shell::{
            IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
            SIIGBF_THUMBNAILONLY,
        };

        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let res = (|| -> Option<(u32, u32, Vec<u8>)> {
            let factory: IShellItemImageFactory =
                SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).ok()?;
            let req_h = ((max_w as u64 * 9 / 16) as i32).max(1);
            let hbmp = factory
                .GetImage(
                    SIZE { cx: max_w as i32, cy: req_h },
                    SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK,
                )
                .ok()?;

            let mut bm = BITMAP::default();
            let got = GetObjectW(
                hbmp,
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut bm as *mut BITMAP).cast()),
            );
            if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
                let _ = DeleteObject(hbmp);
                return None;
            }
            let (sw, sh) = (bm.bmWidth as usize, bm.bmHeight as usize);
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: sw as i32,
                    biHeight: -(sh as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bgra = vec![0u8; sw * sh * 4];
            let hdc = CreateCompatibleDC(None);
            let lines = GetDIBits(
                hdc,
                hbmp,
                0,
                sh as u32,
                Some(bgra.as_mut_ptr().cast()),
                &mut bi,
                DIB_RGB_COLORS,
            );
            let _ = DeleteDC(hdc);
            let _ = DeleteObject(hbmp);
            if lines <= 0 {
                return None;
            }

            let target_w = max_w as usize;
            if sw <= target_w {
                let mut rgba = Vec::with_capacity(sw * sh * 4);
                for px in bgra.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
                return Some((sw as u32, sh as u32, rgba));
            }

            // Box-filter downscale so cached thumbnails stay crisp and lightweight.
            let dw = target_w.max(2);
            let dh = (dw * sh / sw).max(2);
            let mut rgba = Vec::with_capacity(dw * dh * 4);
            for y in 0..dh {
                let y0 = y * sh / dh;
                let y1 = ((y + 1) * sh / dh).max(y0 + 1).min(sh);
                for x in 0..dw {
                    let x0 = x * sw / dw;
                    let x1 = ((x + 1) * sw / dw).max(x0 + 1).min(sw);
                    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                    for sy in y0..y1 {
                        let row = sy * sw * 4;
                        for sx in x0..x1 {
                            let i = row + sx * 4;
                            b += bgra[i] as u32;
                            g += bgra[i + 1] as u32;
                            r += bgra[i + 2] as u32;
                            n += 1;
                        }
                    }
                    rgba.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8, 255]);
                }
            }
            Some((dw as u32, dh as u32, rgba))
        })();
        if com {
            CoUninitialize();
        }
        res
    })
    .join()
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_video_extensions_case_insensitively() {
        assert!(is_video(Path::new("clip.mp4")));
        assert!(is_video(Path::new("CLIP.MKV")));
        assert!(is_video(Path::new("a.b.avi")));
        assert!(!is_video(Path::new("clip.txt")));
        assert!(!is_video(Path::new("mp4")));
        assert!(!is_video(Path::new("clip.tmp")));
    }

    #[test]
    fn formats_recorder_names() {
        assert_eq!(pretty_name(Path::new("clip_2026-11-15_23-38-30.mp4")), "15/11 · 23:38:30");
        assert_eq!(pretty_name(Path::new("Clippy_2026-11-15_23-38-30_2.mp4")), "15/11 · 23:38:30 (2)");
        assert_eq!(pretty_name(Path::new("clip_2026-11-15.mp4")), "clip_2026-11-15");
        assert_eq!(pretty_name(Path::new("my clip.mp4")), "my clip");
    }

    #[test]
    fn validates_file_and_folder_names() {
        assert!(folder_name_ok("clips"));
        assert!(folder_name_ok("  clips  "));
        assert!(!folder_name_ok(""));
        assert!(!folder_name_ok("   "));
        assert!(!folder_name_ok("clips."));
        assert!(!folder_name_ok("a/b"));
        assert!(!folder_name_ok("a:b"));

        assert!(filename_ok("highlight"));
        assert!(filename_ok("highlight.v2"));
        assert!(!filename_ok("CON"));
        assert!(!filename_ok("nul.mp4"));
        assert!(!filename_ok("lpt9."));
        assert!(!filename_ok("bad?name"));
    }

    #[test]
    fn renames_clip_keeping_extension() {
        let dir = std::env::temp_dir().join(format!("clip-library-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let from = dir.join("clip_2026-11-15_23-38-30.mp4");
        std::fs::write(&from, b"x").unwrap();

        // A typed extension is stripped so it is never duplicated.
        let to = rename_clip(&from, "highlight.mp4").unwrap();
        assert_eq!(to, dir.join("highlight.mp4"));
        assert!(to.exists() && !from.exists());

        // Reserved names and existing targets are rejected.
        assert!(rename_clip(&to, "CON").is_err());
        std::fs::write(dir.join("taken.mp4"), b"y").unwrap();
        assert!(rename_clip(&to, "taken").is_err());
        assert!(to.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
