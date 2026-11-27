//! GPU BGRA -> NV12 conversion using the D3D11 video processor (no shaders or CPU copies).
//! Also rotates portrait monitors and letterboxes monitors of other resolutions.

use std::mem::ManuallyDrop;

use std::time::{Duration, Instant};

use anyhow::Result;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

use super::capture::Frame;

/// Max wait time for the GPU to finish converting a frame.
const CONVERT_WAIT: Duration = Duration::from_millis(50);

/// Poll interval on drivers without D3D11 fences (no busy spin, no per-poll driver flush).
const FALLBACK_POLL: Duration = Duration::from_micros(300);

/// GPU completion signal: a D3D11 fence signalled after each `VideoProcessorBlt`, waited on with a
/// Win32 event instead of polling a query (which flushed the driver command buffer every spin).
struct Fence {
    ctx: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    event: HANDLE,
    /// Monotonic value signalled after every conversion.
    value: u64,
}

impl Fence {
    fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Option<Self> {
        unsafe {
            let device = device.cast::<ID3D11Device5>().ok()?;
            let ctx = context.cast::<ID3D11DeviceContext4>().ok()?;
            let mut fence: Option<ID3D11Fence> = None;
            device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence).ok()?;
            let event = CreateEventW(None, false, false, PCWSTR::null()).ok()?;
            Some(Self { ctx, fence: fence?, event, value: 0 })
        }
    }

    /// Signals the fence, flushes, and blocks until the GPU reaches it. `false` on timeout or error.
    fn wait(&mut self) -> bool {
        unsafe {
            self.value += 1;
            let _ = ResetEvent(self.event);
            if self.ctx.Signal(&self.fence, self.value).is_err() {
                return false;
            }
            self.ctx.Flush();
            if self.fence.SetEventOnCompletion(self.value, self.event).is_err() {
                return false;
            }
            let deadline = Instant::now() + CONVERT_WAIT;
            loop {
                if self.fence.GetCompletedValue() >= self.value {
                    return true;
                }
                let rem = deadline.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    return false;
                }
                if WaitForSingleObject(self.event, rem.as_millis().max(1) as u32) != WAIT_OBJECT_0 {
                    return self.fence.GetCompletedValue() >= self.value;
                }
            }
        }
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

pub struct Converter {
    /// Input texture being converted (if `Capture` swaps textures, create a new `Converter`).
    src: ID3D11Texture2D,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Signals when the GPU completes the conversion (used on drivers without fences).
    done: ID3D11Query,
    /// Fence + event waiter; `None` when the driver lacks `ID3D11Device5`/`ID3D11DeviceContext4`.
    fence: Option<Fence>,
    vctx: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// Last converted `(frame_seq, texture)`: unchanged frames skip the video processor entirely.
    last: Option<(u64, ID3D11Texture2D)>,
    black: ID3D11Texture2D,
}

impl Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        frame: &Frame,
        w: u32,
        h: u32,
        fps: u32,
        pool_size: usize,
    ) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext = context.cast()?;
            let src = &frame.tex;
            let mut sd = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut sd);

            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: sd.Width,
                InputHeight: sd.Height,
                OutputFrameRate: rate,
                OutputWidth: w,
                OutputHeight: h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&cd)?;
            let vp = vdev.CreateVideoProcessor(&en, 0)?;

            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(src, &en, &ivd, Some(&mut input))?;

            let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut pool = Vec::with_capacity(pool_size);
            for _ in 0..pool_size {
                let mut td = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                if device.CreateTexture2D(&td, None, Some(&mut t)).is_err() {
                    td.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
                    device.CreateTexture2D(&td, None, Some(&mut t))?;
                }
                let t = t.unwrap();
                let mut ov = None;
                vdev.CreateVideoProcessorOutputView(&t, &en, &ovd, Some(&mut ov))?;
                pool.push((t, ov.unwrap()));
            }

            // Accurate colors: full-range RGB -> studio-range YUV BT.709.
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);

            // Portrait monitors arrive unrotated: DXGI_MODE_ROTATION is 1..=4, video processor is 0..=3.
            let quarter = frame.rotation.0 - 1;
            if quarter > 0 {
                vctx.VideoProcessorSetStreamRotation(&vp, 0, true, D3D11_VIDEO_PROCESSOR_ROTATION(quarter));
            }
            // Letterbox without stretching when switching to a monitor with a different aspect ratio.
            let (sw, sh) = if quarter % 2 == 1 { (sd.Height, sd.Width) } else { (sd.Width, sd.Height) };
            let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
            let dw = (sw as f64 * scale).round() as i32 & !1;
            let dh = (sh as f64 * scale).round() as i32 & !1;
            let (x, y) = (((w as i32 - dw) / 2) & !1, ((h as i32 - dh) / 2) & !1);
            let dest = RECT { left: x, top: y, right: x + dw, bottom: y + dh };
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dest));
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            vctx.VideoProcessorSetOutputBackgroundColor(&vp, false, &black);

            // Black NV12 frame (Y=16, UV=128) for "Games only" mode when no game is visible.
            let (bw, bh) = (w as usize, h as usize);
            let mut px = vec![16u8; bw * bh];
            px.resize(bw * bh * 3 / 2, 128);
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as *const _, SysMemPitch: w, SysMemSlicePitch: 0 };
            let bd = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut black = None;
            device.CreateTexture2D(&bd, Some(&init), Some(&mut black))?;

            let mut done = None;
            device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut done))?;

            let fence = Fence::new(device, context);
            Ok(Self {
                src: src.clone(),
                device: device.clone(),
                context: context.clone(),
                done: done.unwrap(),
                fence,
                vctx,
                vp, input: input.unwrap(), pool, next: 0, last: None, black: black.unwrap() })
        }
    }

    pub fn is_for(&self, frame: &Frame) -> bool {
        self.src == frame.tex
    }

    pub fn black(&self) -> ID3D11Texture2D {
        self.black.clone()
    }

    /// Converts the current frame into a pooled NV12 texture and waits for GPU completion;
    /// some encoders (notably AMD) otherwise read unwritten textures and output green frames.
    /// Frames already converted (`frame_seq` unchanged, e.g. the desktop was static) reuse the
    /// previous output and skip both the blit and the fence wait.
    pub fn convert(&mut self, frame_seq: u64) -> Result<ID3D11Texture2D> {
        if let Some((seq, tex)) = &self.last {
            if *seq == frame_seq {
                return Ok(tex.clone());
            }
        }
        let (tex, view) = &self.pool[self.next];
        self.next = (self.next + 1) % self.pool.len();
        unsafe {
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
                ..Default::default()
            }];
            let r = self.vctx.VideoProcessorBlt(&self.vp, view, 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            r?;
            if let Some(fence) = self.fence.as_mut() {
                // Preferred: block on the fence's event (no polling, no driver flushes).
                let _ = fence.wait();
            } else {
                // Older drivers: flush once, then poll the query without the implicit flush and
                // sleep between polls instead of spinning.
                self.context.End(&self.done);
                self.context.Flush();
                let start = Instant::now();
                let mut ready = BOOL(0);
                while start.elapsed() < CONVERT_WAIT {
                    let size = std::mem::size_of::<BOOL>() as u32;
                    let flags = D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32;
                    if self.context.GetData(&self.done, Some(&mut ready as *mut BOOL as *mut _), size, flags).is_ok() && ready.as_bool() {
                        break;
                    }
                    std::thread::sleep(FALLBACK_POLL);
                }
            }
        }
        self.last = Some((frame_seq, tex.clone()));
        Ok(tex.clone())
    }

    /// Average luma/chroma of captured and converted frames for logging (diagnoses black/green clips).
    pub fn probe(&self, converted: &ID3D11Texture2D) -> String {
        let src = self.read(&self.src, |d, m| {
            if d.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                return format!("format {:?}", d.Format);
            }
            let [b, g, r] = mean(d.Width / 16, d.Height / 16, |x, y, c| m.at(y * 16, x * 16 * 4 + c));
            format!("mean RGB ({r}, {g}, {b})")
        });
        let nv12 = self.read(converted, |d, m| {
            let [y] = mean(d.Width / 16, d.Height / 16, |x, y, _| m.at(y * 16, x * 16));
            // In NV12, interleaved UV chroma rows follow the Y plane.
            let [u, v] = mean(d.Width / 32, d.Height / 32, |x, y, c| m.at(d.Height + y * 16, x * 32 + c));
            format!("Y {y}, U {u}, V {v}")
        });
        format!("image check: captured {src}; converted {nv12}")
    }

    /// Copies `tex` to a CPU staging texture and runs `f`.
    fn read(&self, tex: &ID3D11Texture2D, f: impl Fn(&D3D11_TEXTURE2D_DESC, &Mapped) -> String) -> String {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut d);
            let mut sd = d;
            sd.MipLevels = 1;
            sd.ArraySize = 1;
            sd.Usage = D3D11_USAGE_STAGING;
            sd.BindFlags = 0;
            sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            sd.MiscFlags = 0;
            let mut staging = None;
            if let Err(e) = self.device.CreateTexture2D(&sd, None, Some(&mut staging)) {
                return format!("unreadable ({e})");
            }
            let staging = staging.unwrap();
            self.context.CopyResource(&staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(e) = self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)) {
                return format!("unreadable ({e})");
            }
            let s = f(&d, &Mapped { data: m.pData as *const u8, pitch: m.RowPitch as usize });
            self.context.Unmap(&staging, 0);
            s
        }
    }
}

struct Mapped {
    data: *const u8,
    pitch: usize,
}

impl Mapped {
    fn at(&self, row: u32, byte: u32) -> u8 {
        unsafe { *self.data.add(row as usize * self.pitch + byte as usize) }
    }
}

/// Mean of `N` channels across a `w` x `h` sample grid.
fn mean<const N: usize>(w: u32, h: u32, px: impl Fn(u32, u32, u32) -> u8) -> [u64; N] {
    let mut sum = [0u64; N];
    for y in 0..h {
        for x in 0..w {
            for (c, s) in sum.iter_mut().enumerate() {
                *s += px(x, y, c as u32) as u64;
            }
        }
    }
    sum.map(|s| s / (w as u64 * h as u64).max(1))
}
