//! Monitor capture via DXGI Desktop Duplication. The target monitor is chosen by `recorder`
//! (the active game's monitor, or the active window's monitor) and can switch without dropping the buffer.
//! Frames never leave the GPU: each frame is copied straight to an owned texture.
//! No game injection (anticheat-safe, e.g., Vanguard).

use anyhow::{anyhow, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID, POINT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};

use crate::log::log;

/// Retry interval when opening duplication fails (100 ns units = 5 s).
const RETRY: i64 = 50_000_000;

/// Result of `Capture::aim`.
pub enum Aim {
    /// Unchanged (already on this monitor, or cannot switch without restarting and didn't need to).
    Same,
    Switched,
    /// Restart required: monitor is on another GPU or the video must adopt its native resolution.
    Restart,
}

/// Latest monitor frame (BGRA, unrotated) on the GPU.
pub struct Frame {
    pub tex: ID3D11Texture2D,
    pub rotation: DXGI_MODE_ROTATION,
}

/// Output display as seen from a GPU adapter.
struct Output {
    monitor: HMONITOR,
    output: IDXGIOutput1,
    name: String,
    /// Desktop dimensions (already rotated), rounded down to even for H.264/HEVC.
    width: u32,
    height: u32,
}

pub struct Capture {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    /// Fixed session video size; other monitors are scaled to fit without distortion.
    pub width: u32,
    pub height: u32,
    /// Replaced with a new texture whenever size or rotation changes.
    pub latest: Option<Frame>,
    /// Incremented on every copied frame; lets `Converter` reuse its last output for unchanged frames.
    pub frame_seq: u64,
    adapter: IDXGIAdapter1,
    gpu: String,
    out: Output,
    dup: Option<IDXGIOutputDuplication>,
    rotation: DXGI_MODE_ROTATION,
    retry_at: i64,
    /// Avoid repeating failed-duplication logs every 5 s.
    failing: bool,
    /// Last monitor skipped due to cross-GPU/mismatch (logged once).
    skipped: Option<HMONITOR>,
}

impl Capture {
    /// Captures `monitor` (or primary if missing/disconnected). Tries every adapter that sees it:
    /// on dual-GPU laptops the first adapter enumerating an output cannot always duplicate it.
    pub fn new(monitor: Option<HMONITOR>) -> Result<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut all = Vec::new();
        let mut i = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            all.extend(outputs(&adapter).into_iter().map(|o| (adapter.clone(), o)));
        }
        let primary = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let want = monitor.filter(|m| all.iter().any(|(_, o)| o.monitor == *m)).unwrap_or(primary);
        let mut last = anyhow!("no monitor found to record");
        for (adapter, out) in all.into_iter().filter(|(_, o)| o.monitor == want) {
            let name = out.name.clone();
            match Self::open_on(adapter, out) {
                Ok(c) => return Ok(c),
                Err(e) => {
                    log(&format!("failed to capture {name}: {e}"));
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn open_on(adapter: IDXGIAdapter1, out: Output) -> Result<Self> {
        unsafe {
            let adesc = adapter.GetDesc1()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.ok_or_else(|| anyhow!("missing D3D11 device"))?;
            let context = context.ok_or_else(|| anyhow!("missing D3D11 context"))?;
            // Media Foundation's encoder shares this D3D11 device from its worker threads.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut cap = Capture {
                device,
                context,
                luid: adesc.AdapterLuid,
                width: out.width,
                height: out.height,
                latest: None,
                adapter,
                gpu: wide(&adesc.Description),
                out,
                dup: None,
                rotation: DXGI_MODE_ROTATION::default(),
                retry_at: 0,
                failing: false,
                skipped: None,
                frame_seq: 0,
            };
            cap.open()?;
            Ok(cap)
        }
    }

    /// `"\\\\.\\DISPLAY2 (NVIDIA GeForce RTX 2060)"`, used for logging.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.out.name, self.gpu)
    }

    fn open(&mut self) -> Result<()> {
        self.dup = None;
        let dup = unsafe { self.out.output.DuplicateOutput(&self.device)? };
        self.rotation = unsafe { dup.GetDesc() }.Rotation;
        self.dup = Some(dup);
        Ok(())
    }

    /// Switches capture to `monitor`. When `native` is true (a game monitor), resolution changes
    /// require a restart; otherwise mismatched monitors scale inside the session and cross-GPU
    /// monitors are skipped so the ring buffer is preserved.
    pub fn aim(&mut self, monitor: HMONITOR, native: bool) -> Aim {
        let same_size = |o: &Output| (o.width, o.height) == (self.width, self.height);
        if monitor == self.out.monitor && (!native || same_size(&self.out)) {
            return Aim::Same;
        }
        let Some(out) = outputs(&self.adapter).into_iter().find(|o| o.monitor == monitor) else {
            return self.skip(monitor, native, "is on another GPU");
        };
        if native && !same_size(&out) {
            log(&format!("{} has a different resolution ({}x{}): restarting capture", out.name, out.width, out.height));
            return Aim::Restart;
        }
        let prev = std::mem::replace(&mut self.out, out);
        match self.open() {
            Ok(()) => {
                log(&format!("recording {}", self.describe()));
                self.failing = false;
                self.skipped = None;
                Aim::Switched
            }
            Err(e) => {
                // Fall back to the previous monitor; `poll` will reopen its duplication.
                self.out = prev;
                self.dup = None;
                self.retry_at = 0;
                self.skip(monitor, native, &format!("couldn't be captured on this GPU ({e})"))
            }
        }
    }

    fn skip(&mut self, monitor: HMONITOR, native: bool, why: &str) -> Aim {
        if native {
            log(&format!("game monitor {why}: restarting capture"));
            return Aim::Restart;
        }
        if self.skipped != Some(monitor) {
            self.skipped = Some(monitor);
            log(&format!("active window is on a monitor that {why}: keeping {}", self.out.name));
        }
        Aim::Same
    }

    /// Grabs the latest frame if the screen updated (non-blocking).
    pub fn poll(&mut self, now: i64) -> Result<()> {
        if self.dup.is_none() {
            if now < self.retry_at {
                return Ok(());
            }
            // Secure desktop (UAC, lock screen), mode change, or another app holding duplication.
            if let Err(e) = self.open() {
                self.retry_at = now + RETRY;
                if !self.failing {
                    self.failing = true;
                    log(&format!("cannot capture {} ({e}): video frozen, retrying every 5 s", self.out.name));
                }
                return Ok(());
            }
            if std::mem::take(&mut self.failing) {
                log(&format!("recovered capture of {}", self.out.name));
            }
        }
        let dup = self.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(0, &mut info, &mut res) } {
            Ok(()) => {
                let r = if info.LastPresentTime != 0 {
                    match res {
                        Some(res) => res.cast::<ID3D11Texture2D>().map_err(Into::into).and_then(|t| self.copy(&t)),
                        None => Ok(()),
                    }
                } else {
                    Ok(())
                };
                unsafe {
                    let _ = dup.ReleaseFrame();
                }
                r?;
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
            Err(e) => {
                if e.code() != DXGI_ERROR_ACCESS_LOST {
                    log(&format!("duplication error on {}: {e}", self.out.name));
                }
                self.dup = None;
                self.retry_at = now;
            }
        }
        Ok(())
    }

    fn copy(&mut self, frame: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            frame.GetDesc(&mut d);
            let fits = self.latest.as_ref().is_some_and(|f| {
                let mut l = D3D11_TEXTURE2D_DESC::default();
                f.tex.GetDesc(&mut l);
                f.rotation == self.rotation && (l.Width, l.Height, l.Format) == (d.Width, d.Height, d.Format)
            });
            if !fits {
                d.MipLevels = 1;
                d.ArraySize = 1;
                d.Usage = D3D11_USAGE_DEFAULT;
                d.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                d.CPUAccessFlags = 0;
                d.MiscFlags = 0;
                let mut t = None;
                self.device.CreateTexture2D(&d, None, Some(&mut t))?;
                let tex = t.ok_or_else(|| anyhow!("missing capture texture"))?;
                self.latest = Some(Frame { tex, rotation: self.rotation });
            }
            if let Some(f) = &self.latest {
                self.context.CopyResource(&f.tex, frame);
                self.frame_seq = self.frame_seq.wrapping_add(1);
            }
        }
        Ok(())
    }
}

/// Desktop-attached outputs visible from this GPU adapter.
fn outputs(adapter: &IDXGIAdapter1) -> Vec<Output> {
    let mut v = Vec::new();
    let mut j = 0;
    while let Ok(output) = unsafe { adapter.EnumOutputs(j) } {
        j += 1;
        let (Ok(d), Ok(output)) = (unsafe { output.GetDesc() }, output.cast::<IDXGIOutput1>()) else { continue };
        if !d.AttachedToDesktop.as_bool() {
            continue;
        }
        let r = d.DesktopCoordinates;
        v.push(Output {
            monitor: d.Monitor,
            output,
            name: wide(&d.DeviceName),
            width: (r.right - r.left) as u32 & !1,
            height: (r.bottom - r.top) as u32 & !1,
        });
    }
    v
}

fn wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}
