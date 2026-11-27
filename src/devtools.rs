//! Windowless test modes (compiled only with `--features devtools`; excluded from release builds).
//!
//! - `--test <seconds> <folder> [game|desktop|mic|games]`: records, saves a clip, and exits.
//! - `--test-mic`: prints how much audio each microphone delivers in 3 s.
//! - `--test-audio`, `--test-vol`: WASAPI diagnostics.

use std::sync::Arc;
use std::time::Duration;

use crate::record::{audio, clock};

/// Returns an exit code if `args` requests a test mode.
pub fn run(args: &[String]) -> Option<i32> {
    let at = |flag: &str| args.iter().position(|a| a == flag);
    if let Some(i) = at("--test-audio") {
        return Some(test_audio(&args[i + 1..]));
    }
    if at("--test-vol").is_some() {
        return Some(test_vol());
    }
    if at("--test-mic").is_some() {
        return Some(test_mic());
    }
    at("--test").map(|i| test_mode(&args[i + 1..]))
}

/// `--test-vol`: master endpoint volume and per-session audio volume (pid, level, mute).
/// Loopback taps the mix after session volume: if a game is turned down in Windows mixer,
/// the recorded clip is quiet even if headphones are loud.
fn test_vol() -> i32 {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
        let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None).unwrap();
        let mv = vol.GetMasterVolumeLevelScalar().unwrap();
        println!("master: {mv:.3} (mute {})", vol.GetMute().unwrap().as_bool());
        let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None).unwrap();
        let list = mgr.GetSessionEnumerator().unwrap();
        for i in 0..list.GetCount().unwrap() {
            let Ok(s) = list.GetSession(i) else { continue };
            let Ok(c2) = s.cast::<IAudioSessionControl2>() else { continue };
            let pid = c2.GetProcessId().unwrap_or(0);
            let state = c2.GetState().map(|s| s.0).unwrap_or(-1);
            let Ok(sv) = s.cast::<ISimpleAudioVolume>() else { continue };
            println!("  pid {pid:7}  vol {:.3}  mute {}  state {state}", sv.GetMasterVolume().unwrap_or(-1.0), sv.GetMute().map(|m| m.as_bool()).unwrap_or(true));
        }
    }
    0
}

/// `--test-audio`: opens default-device loopback like the recorder and dumps per-packet info:
/// arrival QPC, frame count, silent flag, and peak per channel to diagnose WASAPI vs. mixer issues.
fn test_audio(args: &[String]) -> i32 {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::CreateEventW;
    use windows::core::PCWSTR;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _mf = crate::mf::Mf::init();
        let pid: Option<u32> = args.first().and_then(|s| s.parse().ok());
        let exclude = args.get(1).map(|s| s == "exclude").unwrap_or(false);
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
        let client: IAudioClient = match pid {
            Some(pid) => {
                println!("process {pid} loopback ({}):", if exclude { "all except it" } else { "only it" });
                audio::activate_process(pid, !exclude).unwrap()
            }
            None => {
                let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
                println!("default device loopback:");
                dev.Activate(CLSCTX_ALL, None).unwrap()
            }
        };
        let (channels, mask) = audio::mix_layout(&client);
        println!("device: {channels} channels, mask 0x{mask:X}");
        let fmt = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE,
                nChannels: channels,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * channels as u32 * 4,
                nBlockAlign: channels * 4,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
            dwChannelMask: if mask == 0 { (1u32 << channels) - 1 } else { mask },
            SubFormat: windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71),
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            2_000_000,
            0,
            &fmt.Format,
            None,
        ).unwrap();
        let event = CreateEventW(None, false, false, PCWSTR::null()).unwrap();
        client.SetEventHandle(event).unwrap();
        let cap: IAudioCaptureClient = client.GetService().unwrap();
        client.Start().unwrap();
        let t0 = clock::now_100ns();
        let mut last_qpc = 0i64;
        let deadline = t0 + 40_000_000; // 4 s
        let mut packets = 0usize;
        let mut total_frames = 0usize;
        while clock::now_100ns() < deadline {
            std::thread::sleep(Duration::from_millis(30));
            loop {
                if cap.GetNextPacketSize().unwrap() == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)).unwrap();
                let dt_ms = (qpc as i64 - t0) as f64 / 10_000.0;
                let step = if last_qpc == 0 { 0.0 } else { (qpc as i64 - last_qpc) as f64 / 10_000.0 };
                last_qpc = qpc as i64;
                let ch = channels as usize;
                let mut per_ch = vec![0f32; ch];
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                if !silent {
                    let src = std::slice::from_raw_parts(data as *const f32, frames as usize * ch);
                    for fr in src.chunks_exact(ch) {
                        for (c, x) in fr.iter().enumerate() {
                            per_ch[c] = per_ch[c].max(x.abs());
                        }
                    }
                }
                packets += 1;
                total_frames += frames as usize;
                let max_all: String = per_ch.iter().map(|v| format!("{:.2}", v)).collect::<Vec<_>>().join(",");
                println!("  qpc+{dt_ms:8.1}ms step {step:7.1}ms  frames {frames:5}  sil {silent}  ch_max [{max_all}]");
                cap.ReleaseBuffer(frames).unwrap();
            }
        }
        println!("packets: {packets}, frames: {total_frames} ({:.2}s of audio)", total_frames as f64 / 48_000.0);
    }
    0
}

fn test_mic() -> i32 {
    // Probe how much audio each microphone delivers over 3 s.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let mut devs = vec![(String::new(), "Default".to_string())];
    devs.extend(audio::capture_devices());
    for (id, name) in devs {
        match audio::Audio::open(audio::Source::Mic(id)) {
            Ok(mut a) => {
                let mut buf = audio::AudioBuf::default();
                let t0 = clock::now_100ns();
                for _ in 0..60 {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = a.poll(&mut buf, t0);
                }
                let peak = buf.data.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                println!("{name}: {} samples, peak {peak}", buf.data.len() / 2);
            }
            Err(e) => println!("{name}: error {e}"),
        }
    }
    0
}

/// `--test <seconds> <folder> [game|desktop|mic|games]`: records, saves a clip, and exits.
fn test_mode(args: &[String]) -> i32 {
    let wait: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(8);
    let dir = args.get(1).map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let mut cfg = crate::config::load();
    cfg.output_dir = dir;
    cfg.sound_on_save = false;
    // Headless test runs are not inside a game: record the screen unless "games" is asked for.
    cfg.capture_mode = crate::config::CaptureMode::Screen;
    match args.get(2).map(|s| s.as_str()) {
        Some("game") => (cfg.game_audio, cfg.desktop_audio) = (true, false),
        Some("desktop") => (cfg.game_audio, cfg.desktop_audio) = (false, true),
        Some("mic") => (cfg.game_audio, cfg.desktop_audio, cfg.mic_audio) = (false, false, true),
        Some("games") | Some("juegos") => cfg.capture_mode = crate::config::CaptureMode::GamesOnly,
        Some("screen") => cfg.capture_mode = crate::config::CaptureMode::Screen,
        _ => {}
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let notify: crate::record::recorder::Notify = Arc::new(move |e| {
        let _ = tx.send(e);
    });
    let rec = crate::record::recorder::Recorder::start(cfg, notify);
    std::thread::sleep(Duration::from_secs(wait));
    rec.save();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(crate::record::recorder::Event::Started(n)) => println!("encoder: {n}"),
            Ok(crate::record::recorder::Event::Game(g)) => println!("game: {}", g.as_deref().unwrap_or("none")),
            Ok(crate::record::recorder::Event::Saved(p)) => {
                println!("OK {}", p.display());
                return 0;
            }
            Ok(crate::record::recorder::Event::SaveFailed(e)) | Ok(crate::record::recorder::Event::Error(e)) => {
                println!("ERROR {e}");
                return 1;
            }
            Err(_) => {}
        }
    }
    println!("ERROR timeout");
    2
}
