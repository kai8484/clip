//! Writes clips to MP4 via the Media Foundation Sink Writer.
//! Video is remuxed without re-encoding; PCM audio is compressed to AAC.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::*;

use super::encoder::Packet;
use crate::log::log;
use crate::mf::{self, time_of, Mf, FRAME_BYTES};

pub struct Job {
    pub path: PathBuf,
    pub video_type: Vec<u8>,
    pub packets: Vec<Packet>,
    pub base: i64,
    pub frame_dur: i64,
    pub audio: Option<Vec<i16>>,
}

pub fn write(job: Job) -> Result<PathBuf> {
    let mf = Mf::init();
    let r = write_inner(&job);
    drop(mf);
    // The clip's video GOPs and PCM audio are no longer needed; free them before rewriting the
    // file so faststart's copy does not contend with them for RAM.
    let path = job.path;
    drop(job.video_type);
    drop(job.packets);
    drop(job.audio);
    if let Err(e) = r {
        // Removes the placeholder claimed by the save thread, so no empty clip is left behind.
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    // Faststart is best-effort: the muxed file is already valid, so a failure only costs an
    // initial seek on players that can't read a trailing `moov`.
    match faststart(&path) {
        Ok(true) => log("clip: moved moov before mdat (faststart)"),
        Ok(false) => {}
        Err(e) => log(&format!("clip left non-faststart: {e:?}")),
    }
    Ok(path)
}

fn write_inner(job: &Job) -> Result<()> {
    if let Some(dir) = job.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    unsafe {
        let vtype = MFCreateMediaType()?;
        MFInitAttributesFromBlob(&vtype, &job.video_type)?;

        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 2)?;
        let attrs = attrs.unwrap();
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        attrs.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
        // No MF_MPEG4SINK_MOOV_BEFORE_MDAT here: the sink fails Finalize with E_ACCESSDENIED on
        // this Windows build. `faststart` relocates `moov` after the mux instead.
        let url = HSTRING::from(job.path.as_os_str());
        let writer = MFCreateSinkWriterFromURL(&url, None, &attrs)?;
        let vs = writer.AddStream(&vtype)?;
        writer.SetInputMediaType(vs, &vtype, None)?;

        let a_stream = if job.audio.is_some() {
            let s = writer.AddStream(&mf::aac_type()?)?;
            writer.SetInputMediaType(s, &mf::pcm_type()?, None)?;
            Some(s)
        } else {
            None
        };

        writer.BeginWriting()?;

        const CHUNK: usize = 1024; // audio frames per sample
        let empty = Vec::new();
        let audio = job.audio.as_ref().unwrap_or(&empty);
        let total_af = audio.len() / 2;
        let n = job.packets.len();
        let (mut vi, mut ai) = (0usize, 0usize);
        loop {
            let vt = (vi < n).then(|| job.packets[vi].pts - job.base);
            let at = (a_stream.is_some() && ai < total_af).then(|| time_of(ai as i64));
            let write_audio = match (vt, at) {
                (None, None) => break,
                (Some(v), Some(a)) => a < v,
                (None, Some(_)) => true,
                (Some(_), None) => false,
            };
            if write_audio {
                let frames = CHUNK.min(total_af - ai);
                let bytes = std::slice::from_raw_parts(audio[ai * 2..].as_ptr() as *const u8, frames * FRAME_BYTES);
                let t = time_of(ai as i64);
                let d = time_of((ai + frames) as i64) - t;
                writer.WriteSample(a_stream.unwrap(), &mf::sample(bytes, t, d)?)?;
                ai += frames;
            } else {
                let p = &job.packets[vi];
                let t = p.pts - job.base;
                let d = if vi + 1 < n { job.packets[vi + 1].pts - p.pts } else { job.frame_dur };
                let s = mf::sample(&p.data, t, d.max(1))?;
                if p.key {
                    s.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
                }
                writer.WriteSample(vs, &s)?;
                vi += 1;
            }
        }
        writer.Finalize()?;
    }
    Ok(())
}

// --- faststart ----------------------------------------------------------------

/// Largest `moov` payload we are willing to hold in memory while relocating it. Clips have a tiny
/// `moov`; anything larger means the file isn't what we expect, so leave it alone.
const MAX_MOOV: u64 = 64 << 20;

/// One top-level MP4 box and where it starts in the file.
#[derive(Clone, Copy, Debug)]
struct TopBox {
    kind: [u8; 4],
    start: u64,
    /// Whole box size, header included.
    size: u64,
    /// Header size: 8, or 16 for a 64-bit `largesize`.
    header: u64,
}

impl TopBox {
    fn payload(&self) -> u64 {
        self.start + self.header
    }

    fn end(&self) -> u64 {
        self.start + self.size
    }
}

/// Moves `moov` ahead of `mdat` ("faststart") so playback can start from the first bytes instead of
/// seeking to the end of the file. Rewrites into a sibling temp file created with `create_new`, then
/// replaces the muxed file in one rename. Returns `true` when the file was rewritten.
fn faststart(path: &Path) -> Result<bool> {
    let mut src = File::open(path)?;
    let len = src.metadata()?.len();
    let boxes = read_boxes(&mut src, len)?;
    let Some(mi) = boxes.iter().position(|b| &b.kind == b"moov") else { return Ok(false) };
    let Some(di) = boxes.iter().position(|b| &b.kind == b"mdat") else { return Ok(false) };
    if mi < di {
        return Ok(false); // already faststart
    }

    // New order: ftyp first (players probe it), then moov, then every other box in original order.
    let ftyp = boxes.iter().position(|b| &b.kind == b"ftyp");
    let mut order = Vec::with_capacity(boxes.len());
    if let Some(fi) = ftyp {
        order.push(fi);
    }
    order.push(mi);
    order.extend((0..boxes.len()).filter(|&i| i != mi && Some(i) != ftyp));

    // Where each box will start after the rewrite; the difference is the offset shift for its data.
    let mut starts = vec![0u64; boxes.len()];
    let mut pos = 0u64;
    for &i in &order {
        starts[i] = pos;
        pos = pos
            .checked_add(boxes[i].size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "MP4 size overflow"))?;
    }
    if pos != len {
        bail!("unexpected bytes after the last MP4 box");
    }

    let moov = &boxes[mi];
    let moov_len = moov.size - moov.header;
    if moov_len > MAX_MOOV {
        bail!("moov box too large to relocate ({moov_len} bytes)");
    }
    let mut moov_payload = vec![0u8; moov_len as usize];
    src.seek(SeekFrom::Start(moov.payload()))?;
    src.read_exact(&mut moov_payload)?;
    patch_chunk_offsets(&mut moov_payload, &boxes, &starts, 0)?;

    let (tmp, mut out) = temp_sibling(path)?;
    // One reused staging buffer: io::copy's 8 KB default costs thousands of syscall pairs per clip.
    let mut buf = vec![0u8; 512 * 1024];
    let rewrite = (|| -> Result<()> {
        for &i in &order {
            let b = &boxes[i];
            if i == mi {
                write_box_header(&mut out, &b.kind, b.size, b.header)?;
                out.write_all(&moov_payload)?;
            } else {
                src.seek(SeekFrom::Start(b.start))?;
                copy_exact(&mut src, &mut out, b.size, &mut buf)?;
            }
        }
        out.flush()?;
        Ok(())
    })();
    drop(out);
    drop(src); // close the reader before replacing the file it points at
    if let Err(e) = rewrite {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let mut rename_err = None;
    for attempt in 0..5 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => {
                rename_err = None;
                break;
            }
            Err(e) => {
                rename_err = Some(e);
                if attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
    if let Some(e) = rename_err {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(true)
}

/// Reads the top-level box list by seeking over box headers; payloads are never loaded.
fn read_boxes(f: &mut File, len: u64) -> io::Result<Vec<TopBox>> {
    let mut boxes = Vec::new();
    let mut pos = 0u64;
    while pos + 8 <= len {
        let mut h = [0u8; 16];
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
        let kind = [h[4], h[5], h[6], h[7]];
        let mut header = 8u64;
        if size == 1 {
            f.read_exact(&mut h[8..16])?;
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header || size > len - pos {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated MP4 box"));
        }
        boxes.push(TopBox { kind, start: pos, size, header });
        pos += size;
    }
    Ok(boxes)
}

/// Copies exactly `rem` bytes from `src` to `out` through `buf` in chunks of up to `buf.len()`.
fn copy_exact(src: &mut File, out: &mut File, rem: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut rem = rem;
    while rem > 0 {
        let want = rem.min(buf.len() as u64) as usize;
        match src.read(&mut buf[..want]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "MP4 box ends before its declared size",
                ))
            }
            Ok(n) => {
                out.write_all(&buf[..n])?;
                rem -= n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_box_header(out: &mut File, kind: &[u8; 4], size: u64, header: u64) -> io::Result<()> {
    if header == 16 {
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(kind)?;
        out.write_all(&size.to_be_bytes())
    } else {
        let size = u32::try_from(size).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "box too large"))?;
        out.write_all(&size.to_be_bytes())?;
        out.write_all(kind)
    }
}

fn temp_sibling(path: &Path) -> Result<(PathBuf, File)> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "clip.mp4".into());
    for i in 0..32 {
        let candidate = path.with_file_name(format!("{name}.faststart{i}.tmp"));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("couldn't create a temporary file next to {}", path.display())
}

/// Walks `moov`'s container boxes and rewrites the absolute chunk offsets in every `stco`/`co64`.
fn patch_chunk_offsets(body: &mut [u8], boxes: &[TopBox], starts: &[u64], depth: u32) -> Result<()> {
    if depth > 16 {
        bail!("MP4 box nesting is too deep");
    }
    let mut pos = 0usize;
    while pos + 8 <= body.len() {
        let size32 = u32::from_be_bytes(body[pos..pos + 4].try_into().unwrap()) as u64;
        let kind = [body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]];
        let (size, header) = match size32 {
            0 => ((body.len() - pos) as u64, 8usize),
            1 => {
                if pos + 16 > body.len() {
                    bail!("truncated MP4 box header");
                }
                (u64::from_be_bytes(body[pos + 8..pos + 16].try_into().unwrap()), 16)
            }
            n => (n, 8),
        };
        let size = usize::try_from(size).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "box size overflow"))?;
        if size < header || pos + size > body.len() {
            bail!("truncated MP4 box");
        }
        let child = &mut body[pos + header..pos + size];
        match &kind {
            b"stco" | b"co64" => patch_offsets(child, &kind == b"co64", boxes, starts)?,
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" | b"dinf" | b"mvex" => {
                patch_chunk_offsets(child, boxes, starts, depth + 1)?;
            }
            _ => {}
        }
        pos += size;
    }
    Ok(())
}

/// Shifts every entry of a chunk offset table by the movement of the top-level box it points into.
fn patch_offsets(body: &mut [u8], wide: bool, boxes: &[TopBox], starts: &[u64]) -> Result<()> {
    if body.len() < 8 {
        bail!("short chunk offset box");
    }
    let count = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
    let width = if wide { 8 } else { 4 };
    let end = count
        .checked_mul(width)
        .and_then(|n| n.checked_add(8))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "chunk offset table overflow"))?;
    if end > body.len() {
        bail!("truncated chunk offset table");
    }
    for k in 0..count {
        let at = 8 + k * width;
        let old = if wide {
            u64::from_be_bytes(body[at..at + 8].try_into().unwrap())
        } else {
            u32::from_be_bytes(body[at..at + 4].try_into().unwrap()) as u64
        };
        let new = old as i64 + shift_of(old, boxes, starts);
        if new < 0 {
            bail!("chunk offset underflow");
        }
        let new = new as u64;
        if wide {
            body[at..at + 8].copy_from_slice(&new.to_be_bytes());
        } else {
            let new = u32::try_from(new).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "chunk offset no longer fits 32 bits"))?;
            body[at..at + 4].copy_from_slice(&new.to_be_bytes());
        }
    }
    Ok(())
}

/// Offset delta for a file position: `new_start - old_start` of the top-level box containing it.
fn shift_of(off: u64, boxes: &[TopBox], starts: &[u64]) -> i64 {
    for (i, b) in boxes.iter().enumerate() {
        if off >= b.payload() && off < b.end() {
            return starts[i] as i64 - b.start as i64;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + payload.len());
        v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    /// `moov/trak/mdia/minf/stbl` holding one chunk offset table (`stco` or `co64`).
    fn sample_moov(offset: u64, wide: bool) -> Vec<u8> {
        let mut table = vec![0u8; 4]; // version + flags
        table.extend_from_slice(&1u32.to_be_bytes());
        if wide {
            table.extend_from_slice(&offset.to_be_bytes());
        } else {
            table.extend_from_slice(&(offset as u32).to_be_bytes());
        }
        let mut v = mp4_box(if wide { b"co64" } else { b"stco" }, &table);
        for kind in [b"stbl", b"minf", b"mdia", b"trak"] {
            v = mp4_box(kind, &v);
        }
        mp4_box(b"moov", &v)
    }

    /// ftyp + mdat + moov (the layout Media Foundation produces); returns `(mdat offset, moov size)`.
    fn write_sample(path: &Path, wide: bool) -> (u64, u64) {
        let ftyp = mp4_box(b"ftyp", b"isomisom");
        let mdat = mp4_box(b"mdat", &[0xa5u8; 64]);
        let offset = (ftyp.len() + 8) as u64; // first byte of the mdat payload
        let moov = sample_moov(offset, wide);
        let mut file = ftyp;
        file.extend_from_slice(&mdat);
        file.extend_from_slice(&moov);
        std::fs::write(path, &file).unwrap();
        (offset, moov.len() as u64)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("clip_faststart_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn top_kinds(data: &[u8]) -> Vec<[u8; 4]> {
        let mut v = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            v.push([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);
            if size < 8 {
                break;
            }
            pos += size;
        }
        v
    }

    /// First chunk offset stored anywhere in the (patched) moov payload.
    fn chunk_offset(moov_payload: &[u8], wide: bool) -> u64 {
        let tag = if wide { b"co64" } else { b"stco" };
        let at = moov_payload.windows(4).position(|w| w == tag).unwrap();
        let entry = at + 4 + 8; // skip version/flags + entry count
        if wide {
            u64::from_be_bytes(moov_payload[entry..entry + 8].try_into().unwrap())
        } else {
            u32::from_be_bytes(moov_payload[entry..entry + 4].try_into().unwrap()) as u64
        }
    }

    fn check_relocated(wide: bool) {
        let dir = temp_dir(if wide { "co64" } else { "stco" });
        let path = dir.join("clip_2026-01-02_03-04-05.mp4");
        let (old, moov_len) = write_sample(&path, wide);
        let before = std::fs::read(&path).unwrap();

        assert!(faststart(&path).unwrap());

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), before.len(), "relocation must not change the file size");
        assert_eq!(top_kinds(&data), vec![*b"ftyp", *b"moov", *b"mdat"]);
        let moov_payload = &data[24..24 + (moov_len - 8) as usize];
        assert_eq!(chunk_offset(moov_payload, wide), old + moov_len, "chunk offsets shift by the moov size");

        // Running again finds moov already ahead of mdat and leaves the file untouched.
        assert!(!faststart(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), data, "a faststart file must be left alone");

        // No temp files are left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("faststart")).collect();
        assert!(leftovers.is_empty(), "temporary files must be cleaned up");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn faststart_moves_moov_ahead_of_mdat_and_shifts_stco() {
        check_relocated(false);
    }

    #[test]
    fn faststart_shifts_64_bit_co64_offsets() {
        check_relocated(true);
    }

    #[test]
    fn faststart_leaves_unexpected_files_alone() {
        let dir = temp_dir("untouched");
        let path = dir.join("clip.mp4");
        // No moov: nothing to relocate and no error for the caller.
        std::fs::write(&path, mp4_box(b"ftyp", b"isomisom")).unwrap();
        assert!(!faststart(&path).unwrap());
        assert_eq!(top_kinds(&std::fs::read(&path).unwrap()), vec![*b"ftyp"]);

        // Truncated box: reported as an error, original file untouched.
        let bad = dir.join("bad.mp4");
        std::fs::write(&bad, [0u8, 0, 0, 99, b'm', b'd', b'a', b't']).unwrap();
        let before = std::fs::read(&bad).unwrap();
        assert!(faststart(&bad).is_err());
        assert_eq!(std::fs::read(&bad).unwrap(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    unsafe fn hevc_blob(seq: &[u8]) -> Vec<u8> {
        let t = MFCreateMediaType().unwrap();
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC).unwrap();
        t.SetUINT64(&MF_MT_FRAME_SIZE, (1280u64 << 32) | 720).unwrap();
        t.SetUINT64(&MF_MT_FRAME_RATE, (1u64 << 32) | 30).unwrap();
        t.SetUINT32(&MF_MT_INTERLACE_MODE, 2).unwrap();
        t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, seq).unwrap();
        let size = MFGetAttributesAsBlobSize(&t).unwrap();
        let mut v = vec![0u8; size as usize];
        MFGetAttributesAsBlob(&t, &mut v).unwrap();
        v
    }

    /// Muxes into the (existing, zero-byte) file that the save thread already claimed, which is
    /// what `mux::write` gets in production. Asking the MPEG-4 sink to put `moov` first via
    /// `MF_MPEG4SINK_MOOV_BEFORE_MDAT` makes `Finalize` fail with E_ACCESSDENIED here, so the
    /// relocation has to happen in `faststart` instead.
    #[test]
    fn muxes_into_a_claimed_placeholder_then_faststart_moves_moov() {
        let dir = temp_dir("placeholder");
        let path = dir.join("clip_2026-01-02_03-04-05.mp4");
        drop(OpenOptions::new().write(true).create_new(true).open(&path).unwrap());

        // Sequence header from a real AMD HEVC stream (see the recorder log).
        let seq = hex("0000000140010c01ffff016000000300b0000003000003005d2c0900000001420101016000000300b0000003000003005da00280802d172e490a564b8ffc00040005a808080820000003002000000303d880000000014401e0f7c04c90");
        let frame = |pts: i64, key: bool| {
            let mut d = seq.clone();
            d.extend_from_slice(&[0, 0, 0, 1, if key { 0x26 } else { 0x02 }, 0x01, 0xaf, 0x09, 0x40]);
            Packet { data: d.into(), pts, key }
        };
        let job = Job {
            path: path.clone(),
            video_type: unsafe { hevc_blob(&seq) },
            packets: vec![frame(0, true), frame(333_333, false)],
            base: 0,
            frame_dur: 333_333,
            audio: Some(vec![0i16; 4_800]), // 50 ms of PCM, enough for one AAC sample
        };

        let _mf = Mf::init();
        write_inner(&job).expect("muxing into a claimed placeholder");
        assert!(faststart(&path).unwrap(), "Media Foundation writes moov after mdat");
        let kinds = top_kinds(&std::fs::read(&path).unwrap());
        let at = |kind: &[u8; 4]| kinds.iter().position(|k| k == kind).unwrap();
        assert_eq!(kinds[0], *b"ftyp");
        assert!(at(b"moov") < at(b"mdat"), "moov must precede mdat, got {kinds:?}");
        // The muxer's own moov-before-mdat attribute is what fails; the file itself must be readable.
        assert!(std::fs::metadata(&path).unwrap().len() > 1_000, "clip must hold real data");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "micro-benchmark"]
    fn perf_bench_faststart() {
        let dir = temp_dir("bench");
        let path = dir.join("bench_32mb.mp4");
        let ftyp = mp4_box(b"ftyp", b"isomisom");
        let payload_len = 32usize * 1024 * 1024;
        let mut mdat_hdr = Vec::with_capacity(8);
        mdat_hdr.extend_from_slice(&((8 + payload_len) as u32).to_be_bytes());
        mdat_hdr.extend_from_slice(b"mdat");
        let offset = (ftyp.len() + 8) as u64;
        let moov = sample_moov(offset, false);
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(&ftyp).unwrap();
            f.write_all(&mdat_hdr).unwrap();
            let chunk = vec![0xA5u8; 1024 * 1024];
            for _ in 0..32 {
                f.write_all(&chunk).unwrap();
            }
            f.write_all(&moov).unwrap();
        }
        let t0 = std::time::Instant::now();
        let moved = faststart(&path).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(moved);
        let _ = std::fs::remove_dir_all(&dir);
        println!("BENCH mux_faststart: 32MB={ms:.3}ms");
    }
}
