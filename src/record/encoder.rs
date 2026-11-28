//! Hardware video encoder (NVENC / AMF / QuickSync) via Media Foundation.
//! Accepts GPU NV12 textures directly (no CPU copies) and emits H.264/HEVC packets.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use anyhow::{bail, Result};
use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

use crate::config::VideoCodec;
use crate::log::log;

#[derive(Clone)]
pub struct Packet {
    pub data: Arc<[u8]>,
    pub pts: i64,
    pub key: bool,
}

/// Keyframe request interval (100 ns units = 2 s) for good compression and snappy trimming.
const KEY_EVERY: i64 = 20_000_000;

/// If no keyframe arrives within this window (100 ns), trimming and clip starts break.
const KEY_LATE: i64 = 4 * KEY_EVERY;

pub struct Encoder {
    mft: IMFTransform,
    api: Option<ICodecAPI>,
    /// Earliest PTS before manually requesting another keyframe.
    next_key: i64,
    last_key: i64,
    /// Prevents repeating missing-keyframe log warnings.
    warned_keys: bool,
    events: IMFMediaEventGenerator,
    _mgr: IMFDXGIDeviceManager,
    pub pending_input: u32,
    /// SPS/PPS (and VPS for HEVC) extracted from the latest IDR keyframe.
    seq_header: Option<Vec<u8>>,
    /// Reused scratch space for the parameter sets of the current keyframe.
    seq_scratch: Vec<u8>,
    provides_samples: bool,
    /// Single output sample reused across frames when the encoder doesn't provide samples;
    /// avoids allocating (and zero-filling) a multi-megabyte buffer for every frame.
    out_sample: Option<IMFSample>,
    out_size: u32,
    is_hevc: bool,
    pub name: String,
}

impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        luid: LUID,
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
        codec: VideoCodec,
    ) -> Result<Self> {
        unsafe {
            let (mft, name, active_codec) = match (codec, find_encoder(luid, codec)) {
                (VideoCodec::Hevc, Ok((t, n))) => (t, n, VideoCodec::Hevc),
                (VideoCodec::Hevc, Err(e)) => {
                    log(&format!("HEVC unavailable ({e}), falling back to H.264"));
                    let (t, n) = find_encoder(luid, VideoCodec::H264)?;
                    (t, n, VideoCodec::H264)
                }
                (_, res) => {
                    let (t, n) = res?;
                    (t, n, VideoCodec::H264)
                }
            };
            let is_hevc = active_codec == VideoCodec::Hevc;
            let subtype = if is_hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };

            let attrs = mft.GetAttributes()?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                bail!("encoder {name} is not asynchronous");
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            // `MF_LOW_LATENCY` uncorks the AMD MFT's pipeline. `CODECAPI_AVLowLatencyMode = true`
            // (set below) also lowers latency but caps this MFT at ~32 fps at 1080p60, whereas
            // `MF_LOW_LATENCY = 1` keeps full framerate with ~3.7x lower VCN usage.
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut mgr = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
            let mgr = mgr.unwrap();
            mgr.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)?;

            let api = mft.cast::<ICodecAPI>().ok();
            if let Some(api) = &api {
                let set = |g: &GUID, v: VARIANT| {
                    let _ = api.SetValue(g, &v);
                };
                if api
                    .SetValue(
                        &CODECAPI_AVEncCommonRateControlMode,
                        &VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32),
                    )
                    .is_ok()
                {
                    set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bitrate.saturating_mul(5) / 4));
                } else {
                    set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32));
                }
                set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                // One keyframe every 2 s (`feed` forces one manually if the encoder misses it).
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps.saturating_mul(2)));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                // Favor encoding speed over heavy analysis passes to keep VCN usage low.
                set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(0u32));
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(false));
            }

            let ot = MFCreateMediaType()?;
            ot.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            ot.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
            ot.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            set_video_common(&ot, w, h, fps)?;
            if !is_hevc {
                ot.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            }
            mft.SetOutputType(0, &ot, 0)?;

            let it = MFCreateMediaType()?;
            it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_video_common(&it, w, h, fps)?;
            mft.SetInputType(0, &it, 0)?;

            let info = mft.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = mft.cast()?;

            Ok(Self {
                mft,
                api,
                next_key: 0,
                last_key: 0,
                warned_keys: false,
                events,
                _mgr: mgr,
                pending_input: 0,
                seq_header: None,
                seq_scratch: Vec::new(),
                provides_samples: provides,
                out_sample: None,
                out_size: info.cbSize.max(4 * 1024 * 1024),
                is_hevc,
                name,
            })
        }
    }

    pub fn codec_label(&self) -> &'static str {
        if self.is_hevc { "HEVC" } else { "H.264" }
    }

    /// Drains pending encoder events without blocking.
    pub fn poll(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        loop {
            let ev = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(e.into()),
            };
            let t = unsafe { ev.GetType()? };
            if t == METransformNeedInput.0 as u32 {
                self.pending_input += 1;
            } else if t == METransformHaveOutput.0 as u32 {
                self.pull(out)?;
            }
        }
        Ok(())
    }

    /// The reusable output sample, reset for the next frame.
    unsafe fn reusable_sample(&mut self) -> Result<IMFSample> {
        if let Some(s) = &self.out_sample {
            // The encoder may have attached extra buffers, or left the frame marked as a clean
            // point; both must be reset before the sample is passed back in.
            let ok = s.GetBufferCount().unwrap_or(0) == 1
                && s.GetBufferByIndex(0)
                    .ok()
                    .filter(|b| b.GetMaxLength().unwrap_or(0) >= self.out_size)
                    .map(|b| b.SetCurrentLength(0).is_ok())
                    .unwrap_or(false);
            if !ok {
                s.RemoveAllBuffers()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
            }
            let _ = s.SetSampleTime(0);
            let _ = s.SetSampleDuration(0);
            let _ = s.DeleteItem(&MFSampleExtension_CleanPoint);
            return Ok(s.clone());
        }
        let s = MFCreateSample()?;
        s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
        self.out_sample = Some(s.clone());
        Ok(s)
    }

    fn pull(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        unsafe {
            let own = if self.provides_samples {
                None
            } else {
                Some(self.reusable_sample()?)
            };
            let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = ManuallyDrop::take(&mut buf[0].pSample);
            drop(ManuallyDrop::take(&mut buf[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else { return Ok(()) };

            let pts = sample.GetSampleTime().unwrap_or(0);
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let mb = sample.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            mb.Lock(&mut p, None, Some(&mut len))?;
            let data: Arc<[u8]> = std::slice::from_raw_parts(p, len as usize).into();
            mb.Unlock()?;

            // Clips must start on an IDR frame; some encoders flag non-IDR I-frames as clean points,
            // causing black/green playback until the next IDR. Fall back to the flag only if not Annex B.
            // A single scan per frame yields both the key flag and the parameter sets (stopping at the
            // first VCL NAL), so no temporary NAL lists are allocated.
            self.seq_scratch.clear();
            let key = if is_annex_b(&data) {
                scan_frame(&data, self.is_hevc, &mut self.seq_scratch)
            } else {
                clean
            };
            if key {
                self.last_key = pts;
            } else if pts - self.last_key > KEY_LATE && !self.warned_keys {
                self.warned_keys = true;
                log(&format!("encoder {} is not emitting keyframes: clips may look black or green", self.name));
            }
            if key && !self.seq_scratch.is_empty() && self.seq_header.as_deref() != Some(self.seq_scratch.as_slice()) {
                if self.seq_header.is_none() {
                    let published = self.published_header();
                    let hex = |h: &Option<Vec<u8>>| h.as_ref().map_or("none".into(), |h| h.iter().map(|b| format!("{b:02x}")).collect::<String>());
                    let same = published.as_deref() == Some(self.seq_scratch.as_slice());
                    let same = if same { " (matching)" } else { "" };
                    log(&format!(
                        "{} sequence header{same}: in stream {}, published by encoder {}",
                        self.codec_label(),
                        hex(&Some(self.seq_scratch.clone())),
                        hex(&published)
                    ));
                }
                self.seq_header = Some(self.seq_scratch.clone());
            }
            out.push(Packet { data, pts, key });
        }
        Ok(())
    }

    pub fn feed(&mut self, tex: &ID3D11Texture2D, pts: i64, dur: i64) -> Result<()> {
        unsafe {
            // Only force a keyframe if the encoder hasn't emitted one naturally (with slack for packet delay).
            if pts >= self.next_key && pts - self.last_key > KEY_EVERY * 3 / 2 {
                self.next_key = pts + KEY_EVERY;
                if let Some(api) = &self.api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                if let Ok(len) = b2.GetContiguousLength() {
                    let _ = buf.SetCurrentLength(len);
                }
            }
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(pts)?;
            s.SetSampleDuration(dur)?;
            self.mft.ProcessInput(0, &s, 0)?;
        }
        self.pending_input -= 1;
        Ok(())
    }

    /// Sequence header published by the encoder's output type. On AMD this can differ from the
    /// encoded frames (e.g., POC type 2 vs type 0), causing broken MP4 playback if used verbatim.
    fn published_header(&self) -> Option<Vec<u8>> {
        unsafe {
            let t = self.mft.GetOutputCurrentType(0).ok()?;
            let mut v = vec![0u8; t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()? as usize];
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None).ok()?;
            Some(v)
        }
    }

    /// Serialized output media type for the MP4 muxer thread, patched with the stream's actual
    /// sequence header when available.
    pub fn type_blob(&self) -> Option<Vec<u8>> {
        unsafe {
            let src = self.mft.GetOutputCurrentType(0).ok()?;
            let t = MFCreateMediaType().ok()?;
            src.CopyAllItems(&t).ok()?;
            if let Some(sh) = &self.seq_header {
                t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, sh).ok()?;
            }
            let size = MFGetAttributesAsBlobSize(&t).ok()?;
            let mut v = vec![0u8; size as usize];
            MFGetAttributesAsBlob(&t, &mut v).ok()?;
            Some(v)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(sd) = self.mft.cast::<IMFShutdown>() {
                let _ = sd.Shutdown();
            }
        }
    }
}

unsafe fn set_video_common(t: &IMFMediaType, w: u32, h: u32, fps: u32) -> Result<()> {
    t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    Ok(())
}

unsafe fn find_encoder(luid: LUID, codec: VideoCodec) -> Result<(IMFTransform, String)> {
    let subtype = match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
    };
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.unwrap();
    // Only enumerate encoders on the capturing GPU (matters on dual-GPU laptops).
    let luid_bytes = std::slice::from_raw_parts(&luid as *const LUID as *const u8, std::mem::size_of::<LUID>());
    attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid_bytes)?;

    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnum2(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &attrs,
        &mut ptr,
        &mut count,
    )?;
    let acts: Vec<IMFActivate> = if ptr.is_null() {
        Vec::new()
    } else {
        let s = std::slice::from_raw_parts_mut(ptr, count as usize);
        let v = s.iter_mut().filter_map(|a| a.take()).collect();
        CoTaskMemFree(Some(ptr as *const _));
        v
    };
    for act in acts {
        let name = activate_name(&act);
        match act.ActivateObject::<IMFTransform>() {
            Ok(t) => return Ok((t, name)),
            Err(e) => log(&format!("failed to activate {name}: {e}")),
        }
    }
    let label = match codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::Hevc => "HEVC",
    };
    bail!("your GPU has no hardware encoder for {label}")
}

unsafe fn activate_name(act: &IMFActivate) -> String {
    let mut p = windows::core::PWSTR::null();
    let mut len = 0u32;
    if act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    } else {
        "encoder".into()
    }
}

// --- Annex-B helpers ----------------------------------------------------------

/// Single-pass, allocation-free Annex-B NAL iterator. Accepts 3- and 4-byte start codes and trims
/// the zero padding that belongs to the next start code.
struct Nals<'a> {
    d: &'a [u8],
    at: usize,
}

impl<'a> Nals<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, at: 0 }
    }
}

impl<'a> Iterator for Nals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let d = self.d;
        while self.at < d.len() {
            let code = (self.at..d.len().saturating_sub(2)).find(|&i| d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1)?;
            let start = code + 3;
            // The NAL ends at the next start code, whose leading zeros are not part of the payload.
            let next = (start..d.len().saturating_sub(2)).find(|&i| d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1);
            // Resume at the next start code (not past it): its payload is still to be returned.
            self.at = next.unwrap_or(d.len());
            let mut end = next.unwrap_or(d.len());
            while end > start && d[end - 1] == 0 {
                end -= 1;
            }
            if end > start {
                return Some(&d[start..end]);
            }
        }
        None
    }
}

fn nal_type(n: &[u8], is_hevc: bool) -> u8 {
    if is_hevc {
        (n[0] >> 1) & 0x3f
    } else {
        n[0] & 0x1f
    }
}

/// Starts with an Annex-B start code (`00 00 01` or `00 00 00 01`).
fn is_annex_b(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 1]) || d.starts_with(&[0, 0, 0, 1])
}

/// IDR picture: H.264 NAL 5, HEVC NAL 19 or 20.
fn is_idr(t: u8, is_hevc: bool) -> bool {
    if is_hevc {
        matches!(t, 19 | 20)
    } else {
        t == 5
    }
}

/// Coded slice NAL: H.264 types 1..=5, HEVC types 0..=31.
fn is_vcl(t: u8, is_hevc: bool) -> bool {
    if is_hevc {
        t <= 31
    } else {
        (1..=5).contains(&t)
    }
}

/// Contains an IDR picture; stops scanning at the first VCL NAL.
#[allow(dead_code)] // full-frame check kept for tests/debugging; `scan_frame` is the hot path
fn is_keyframe(d: &[u8], is_hevc: bool) -> bool {
    Nals::new(d).any(|n| is_idr(nal_type(n, is_hevc), is_hevc))
}

/// Scans one Annex-B frame in a single pass: returns whether it holds an IDR picture and appends the
/// parameter sets (H.264: SPS/PPS; HEVC: VPS/SPS/PPS) to `ps`, start-code prefixed. Stops at the
/// first VCL NAL, as nothing after the first slice can be a parameter set.
///
/// Unlike `Nals`, the VCL check happens right after the start code instead of after locating the next
/// one: a slice payload is megabytes long and only its header byte decides the return value.
fn scan_frame(d: &[u8], is_hevc: bool, ps: &mut Vec<u8>) -> bool {
    let mut at = 0usize;
    while at < d.len() {
        let Some(code) = (at..d.len().saturating_sub(2)).find(|&i| d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1)
        else {
            return false;
        };
        let start = code + 3;
        if start >= d.len() {
            return false;
        }
        // `00 00 01 00 00 01` leaves an empty NAL between two start codes: no header to inspect.
        // HEVC TRAIL_N (type 0) starts with a 0x00 byte, so the second byte must be checked too.
        if d[start] == 0 && d.get(start + 1).copied().unwrap_or(0) == 0 {
            at = start;
            continue;
        }
        let t = nal_type(&d[start..], is_hevc);
        if is_vcl(t, is_hevc) {
            return is_idr(t, is_hevc);
        }
        let next = (start..d.len().saturating_sub(2)).find(|&i| d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1);
        let mut end = next.unwrap_or(d.len());
        // The NAL ends at the next start code, whose leading zeros are not part of the payload.
        while end > start && d[end - 1] == 0 {
            end -= 1;
        }
        let is_ps = if is_hevc { matches!(t, 32..=34) } else { matches!(t, 7 | 8) };
        if is_ps && end > start {
            ps.extend_from_slice(&[0, 0, 0, 1]);
            ps.extend_from_slice(&d[start..end]);
        }
        at = next.unwrap_or(d.len());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nals_of(d: &[u8]) -> Vec<Vec<u8>> {
        Nals::new(d).map(<[u8]>::to_vec).collect()
    }

    #[test]
    fn nal_iterator_handles_3_and_4_byte_start_codes() {
        assert_eq!(nals_of(&[0, 0, 0, 1, 9, 1, 0, 0, 0, 0, 1, 2, 0, 0, 1]), vec![vec![9, 1], vec![2]]);
        assert!(nals_of(&[]).is_empty());
        assert!(nals_of(&[1, 2, 3]).is_empty());
        assert!(is_annex_b(&[0, 0, 1, 0x65]) && is_annex_b(&[0, 0, 0, 1, 0x65]) && !is_annex_b(&[1, 0, 0]));
    }

    #[test]
    fn keyframe_scan_collects_param_sets_only_before_the_first_slice() {
        let mut f = vec![0, 0, 0, 1, 0x67, 0x11];
        f.extend_from_slice(&[0, 0, 1, 0x68, 0x22]);
        f.extend_from_slice(&[0, 0, 1, 0x65, 0x33]);
        f.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x44]); // after the slice: must not be collected
        let mut ps = Vec::new();
        assert!(scan_frame(&f, false, &mut ps));
        assert_eq!(ps, [0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 1, 0x68, 0x22]);
        assert!(is_keyframe(&f, false));

        let p = [0, 0, 0, 1, 0x41, 0x99]; // non-IDR slice
        let mut ps = Vec::new();
        assert!(!scan_frame(&p, false, &mut ps));
        assert!(ps.is_empty());
    }

    #[test]
    fn hevc_scan_handles_vps_sps_pps_and_idr_types() {
        let mut f = vec![0, 0, 0, 1, 0x40, 0x01]; // VPS (32)
        f.extend_from_slice(&[0, 0, 1, 0x42, 0x01]); // SPS (33)
        f.extend_from_slice(&[0, 0, 1, 0x44, 0x01]); // PPS (34)
        f.extend_from_slice(&[0, 0, 1, 0x26, 0x01]); // IDR_W_RADL (19)
        let mut ps = Vec::new();
        assert!(scan_frame(&f, true, &mut ps));
        assert_eq!(ps, [
            0, 0, 0, 1, 0x40, 0x01,
            0, 0, 0, 1, 0x42, 0x01,
            0, 0, 0, 1, 0x44, 0x01,
        ]);
        assert!(is_keyframe(&f, true));

        let mut ps = Vec::new();
        assert!(!scan_frame(&[0, 0, 1, 0x02, 0x01], true, &mut ps)); // TRAIL_R (1): not a keyframe
    }

    #[test]
    fn scan_frame_skips_empty_zero_padded_nals() {
        // `00 00 01` directly followed by zero padding and another start code: the NAL is empty.
        let mut f = vec![0, 0, 1, 0, 0, 0, 1];
        f.extend_from_slice(&[0x26, 0x01]); // HEVC IDR_W_RADL (19)
        let mut ps = Vec::new();
        assert!(scan_frame(&f, true, &mut ps));
        assert!(ps.is_empty());

        // HEVC TRAIL_N (type 0) has a 0x00 header byte: it is a real NAL, not padding.
        let mut f = vec![0, 0, 1, 0, 0];
        f.extend_from_slice(&[0, 0, 1, 0x00, 0x01]);
        let mut ps = Vec::new();
        assert!(!scan_frame(&f, true, &mut ps));
        assert!(ps.is_empty());

        // Same padding shape before an IDR: the empty NAL must not hide the VCL NAL that follows.
        let mut f = vec![0, 0, 1, 0, 0];
        f.extend_from_slice(&[0, 0, 1, 0x26, 0x01]);
        let mut ps = Vec::new();
        assert!(scan_frame(&f, true, &mut ps));
    }

    #[test]
    #[ignore = "micro-benchmark"]
    fn perf_bench_scan_frame() {
        let mut frame = vec![0, 0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x42, 0x01, 0, 0, 1, 0x44, 0x01];
        frame.extend_from_slice(&[0, 0, 1, 0x02, 0x01]); // HEVC TRAIL_R (1)
        frame.resize(frame.len() + 128 * 1024, 0x5A);

        let mut ps = Vec::with_capacity(64);
        let iters = 4_000usize;
        let t0 = std::time::Instant::now();
        let mut idrs = 0usize;
        for _ in 0..iters {
            ps.clear();
            if scan_frame(&frame, true, &mut ps) {
                idrs += 1;
            }
        }
        let elapsed = t0.elapsed();
        let gb_s = (frame.len() * iters) as f64 / elapsed.as_secs_f64() / 1e9;
        std::hint::black_box(idrs);
        println!(
            "BENCH scan_frame: 4000x128KB={:.3}ms ({gb_s:.2} GB/s)",
            elapsed.as_secs_f64() * 1000.0
        );
    }
}
