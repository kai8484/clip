//! Short generated/embedded sound cues (independent of Windows sound schemes).

use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

/// Save confirmation cue (0.5 s).
static SAVED_WAV: &[u8] = include_bytes!("../../assets/enable.wav");
/// Duration of the save cue (100 ns units) so it can be muted from desktop audio capture.
pub const SAVED_LEN: i64 = 5_000_000;

static PAUSED: OnceLock<Vec<u8>> = OnceLock::new();
static RESUMED: OnceLock<Vec<u8>> = OnceLock::new();
static SAVED_SCALED: Mutex<Option<(u32, Vec<u8>)>> = Mutex::new(None);

fn play(w: &[u8]) {
    unsafe {
        let _ = PlaySoundW(PCWSTR(w.as_ptr() as *const u16), HMODULE::default(), SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

pub fn play_saved(volume: u32) {
    let vol = volume.min(100);
    if vol == 0 {
        return;
    }
    if vol >= 100 {
        play(SAVED_WAV);
        return;
    }
    let mut guard = SAVED_SCALED.lock().unwrap();
    // Stop any async playback before replacing the in-memory buffer.
    unsafe {
        let _ = PlaySoundW(PCWSTR::null(), HMODULE::default(), SND_ASYNC);
    }
    let needs_rebuild = guard.as_ref().is_none_or(|(cached_vol, _)| *cached_vol != vol);
    if needs_rebuild {
        *guard = Some((vol, scale_wav(SAVED_WAV, vol as f32 / 100.0)));
    }
    if let Some((_, buf)) = guard.as_ref() {
        play(buf);
    }
}

fn scale_wav(src: &[u8], gain: f32) -> Vec<u8> {
    let mut out = src.to_vec();
    if out.len() < 12 {
        return out;
    }
    let mut bps = 16u16;
    let mut pos = 12usize;
    while pos + 8 <= out.len() {
        let id = &src[pos..pos + 4];
        let size = u32::from_le_bytes([src[pos + 4], src[pos + 5], src[pos + 6], src[pos + 7]]) as usize;
        let payload_start = pos + 8;
        let payload_end = payload_start.saturating_add(size).min(out.len());
        if id == b"fmt " && payload_end >= payload_start + 16 {
            bps = u16::from_le_bytes([src[payload_start + 14], src[payload_start + 15]]);
        } else if id == b"data" {
            let data = &mut out[payload_start..payload_end];
            if bps == 16 {
                for s in data.as_chunks_mut::<2>().0 {
                    let v = i16::from_le_bytes(*s) as f32 * gain;
                    *s = (v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16).to_le_bytes();
                }
            } else if bps == 24 {
                for s in data.as_chunks_mut::<3>().0 {
                    let raw = (s[0] as i32) | ((s[1] as i32) << 8) | (((s[2] as i8) as i32) << 16);
                    let scaled = (raw as f32 * gain).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                    s[0] = (scaled & 0xFF) as u8;
                    s[1] = ((scaled >> 8) & 0xFF) as u8;
                    s[2] = ((scaled >> 16) & 0xFF) as u8;
                }
            }
            break;
        }
        pos = payload_start.saturating_add(size + (size & 1));
    }
    out
}

/// Recording paused: two descending notes (G5 -> C5).
pub fn play_paused() {
    play(PAUSED.get_or_init(|| make(&[(784.0, 0.08), (523.25, 0.16)])));
}

/// Recording resumed: two ascending notes (C5 -> G5).
pub fn play_resumed() {
    play(RESUMED.get_or_init(|| make(&[(523.25, 0.08), (784.0, 0.16)])));
}

fn make(notes: &[(f32, f32)]) -> Vec<u8> {
    const RATE: u32 = 44_100;
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, dur) in notes {
        let n = (RATE as f32 * dur) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            let attack = (t / 0.004).min(1.0);
            let decay = (-t * 22.0).exp();
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.85
                + (2.0 * std::f32::consts::PI * freq * 2.0 * t).sin() * 0.15;
            pcm.push((s * attack * decay * 0.30 * i16::MAX as f32) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&RATE.to_le_bytes());
    v.extend_from_slice(&(RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}
