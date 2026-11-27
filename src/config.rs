//! Persistent configuration (`data\config.json`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::log::{app_dir, exe_dir};

/// Video capture target. Stored in config.json as `"screen"` / `"games"` (accepts legacy `"juegos"`).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(from = "String", into = "&'static str")]
pub enum CaptureMode {
    /// Full monitor of the game (or of the active window when no game is running).
    Screen,
    /// Fullscreen game only while visible (black frame otherwise).
    #[default]
    GamesOnly,
}

impl From<String> for CaptureMode {
    fn from(s: String) -> Self {
        if s == "screen" {
            CaptureMode::Screen
        } else {
            CaptureMode::GamesOnly
        }
    }
}

impl From<CaptureMode> for &'static str {
    fn from(m: CaptureMode) -> Self {
        match m {
            CaptureMode::Screen => "screen",
            CaptureMode::GamesOnly => "games",
        }
    }
}

pub const MAX_SECONDS: u32 = 300;
/// Manual video bitrate range (Mbps) for custom quality.
pub const MIN_MBPS: u32 = 2;
pub const MAX_MBPS: u32 = 150;

/// Video codec used when recording clips.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    #[default]
    H264,
    Hevc,
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub output_dir: PathBuf,
    pub seconds: u32,
    pub hotkey_vk: u32,
    /// MOD_ALT = 1, MOD_CONTROL = 2, MOD_SHIFT = 4
    pub hotkey_mods: u32,
    pub fps: u32,
    pub bitrate_mbps: u32,
    pub codec: VideoCodec,
    /// Maximum output height (0 = native, 1080 = 1080p, 720 = 720p).
    pub max_height: u32,
    /// Game audio (auto-detected fullscreen process; see `game`).
    pub game_audio: bool,
    pub game_volume: u32,
    /// Desktop audio (everything else: Discord, music, etc.).
    pub desktop_audio: bool,
    pub desktop_volume: u32,
    /// Microphone (off by default). Empty `mic_device` = Windows default.
    pub mic_audio: bool,
    pub mic_volume: u32,
    pub mic_device: String,
    pub capture_mode: CaptureMode,
    pub sound_on_save: bool,
    pub sound_volume: u32,
    /// Global hotkey to pause/resume recording (vk 0 = unassigned).
    pub toggle_vk: u32,
    pub toggle_mods: u32,
    /// Global hotkey to open the clips folder (vk 0 = unassigned).
    pub folder_vk: u32,
    pub folder_mods: u32,
    /// Base URL of the Cloudflare Worker for sharing clips (e.g. `https://clips.example.com`).
    pub share_url: String,
    /// Secret upload key expected by the Cloudflare Worker (`UPLOAD_KEY`).
    pub share_key: String,
}

/// Global hotkeys: active at any time, including inside fullscreen games.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GlobalAction {
    SaveClip,
    ToggleRecording,
    OpenFolder,
}

impl GlobalAction {
    pub const ALL: [GlobalAction; 3] =
        [GlobalAction::SaveClip, GlobalAction::ToggleRecording, GlobalAction::OpenFolder];

    /// ID for RegisterHotKey / WM_HOTKEY.
    pub fn id(self) -> i32 {
        self as i32 + 1
    }

    pub fn from_id(id: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            GlobalAction::SaveClip => "Save clip",
            GlobalAction::ToggleRecording => "Pause / resume recording",
            GlobalAction::OpenFolder => "Open the clips folder",
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            output_dir: default_output_dir(),
            seconds: 30,
            hotkey_vk: 0x78, // F9
            hotkey_mods: 0,
            fps: 60,
            bitrate_mbps: 16, // "Medium"
            codec: VideoCodec::H264,
            max_height: 0,
            game_audio: true,
            game_volume: 100,
            desktop_audio: true,
            desktop_volume: 100,
            mic_audio: false,
            mic_volume: 100,
            mic_device: String::new(),
            capture_mode: CaptureMode::GamesOnly,
            sound_on_save: true,
            sound_volume: 100,
            toggle_vk: 0,
            toggle_mods: 0,
            folder_vk: 0,
            folder_mods: 0,
            share_url: String::new(),
            share_key: String::new(),
        }
    }
}

impl Config {
    /// Parameters that require restarting video capture when changed (resets the buffer).
    pub fn video_params(&self) -> (bool, u32, u32, VideoCodec, u32) {
        (self.enabled, self.fps, self.bitrate_mbps, self.codec, self.max_height)
    }

    pub fn audio_on(&self) -> bool {
        self.game_audio || self.desktop_audio || self.mic_audio
    }

    pub fn sanitize(&mut self) {
        self.seconds = self.seconds.clamp(5, MAX_SECONDS);
        if !matches!(self.fps, 30 | 60 | 120) {
            self.fps = 60;
        }
        self.bitrate_mbps = self.bitrate_mbps.clamp(MIN_MBPS, MAX_MBPS);
        if !matches!(self.max_height, 0 | 720 | 1080) {
            self.max_height = 0;
        }
        self.game_volume = self.game_volume.min(100);
        self.desktop_volume = self.desktop_volume.min(100);
        self.mic_volume = self.mic_volume.min(200);
        self.sound_volume = self.sound_volume.min(100);
        if self.output_dir.as_os_str().is_empty() {
            self.output_dir = default_output_dir();
        }
        if self.hotkey_vk == 0 || self.hotkey_vk > 0xFE {
            self.hotkey_vk = 0x78;
        }
        self.hotkey_mods &= 0x7;
        self.toggle_mods &= 0x7;
        for a in GlobalAction::ALL {
            if a == GlobalAction::SaveClip {
                continue;
            }
            let (vk, mods) = self.hotkey_mut(a);
            *mods &= 0x7;
            if *vk > 0xFE {
                *vk = 0;
            }
        }
        self.share_url = self.share_url.trim().trim_end_matches('/').to_string();
        self.share_key = self.share_key.trim().to_string();
    }

    /// Key `(vk, mods)` for a global hotkey; `vk == 0` means unassigned.
    pub fn hotkey(&self, a: GlobalAction) -> (u32, u32) {
        match a {
            GlobalAction::SaveClip => (self.hotkey_vk, self.hotkey_mods),
            GlobalAction::ToggleRecording => (self.toggle_vk, self.toggle_mods),
            GlobalAction::OpenFolder => (self.folder_vk, self.folder_mods),
        }
    }

    /// Estimated RAM usage while recording (MB), matching Task Manager: app overhead (~40 MB),
    /// raw audio, and encoded video (typical bitrate averages ~10% of the cap).
    pub fn typical_ram_mb(&self) -> u32 {
        let channels = self.game_audio as u32 + self.desktop_audio as u32 + self.mic_audio as u32;
        let audio = channels * self.seconds * 192 / 1000;
        let video = self.bitrate_mbps * self.seconds / 80;
        (40 + audio + video).div_ceil(10) * 10
    }

    pub fn hotkey_mut(&mut self, a: GlobalAction) -> (&mut u32, &mut u32) {
        match a {
            GlobalAction::SaveClip => (&mut self.hotkey_vk, &mut self.hotkey_mods),
            GlobalAction::ToggleRecording => (&mut self.toggle_vk, &mut self.toggle_mods),
            GlobalAction::OpenFolder => (&mut self.folder_vk, &mut self.folder_mods),
        }
    }
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

pub fn exists() -> bool {
    config_path().exists()
}

pub fn load() -> Config {
    let mut cfg: Config = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default();
    cfg.sanitize();
    cfg
}

pub fn save(cfg: &Config) -> std::io::Result<()> {
    std::fs::create_dir_all(app_dir())?;
    let tmp = app_dir().join("config.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg).unwrap_or_default())?;
    std::fs::rename(&tmp, config_path())
}

pub fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

fn default_output_dir() -> PathBuf {
    // Store clips next to the executable so the directory stays self-contained.
    exe_dir().join("clips")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_mode_defaults_to_games_only() {
        assert_eq!(Config::default().capture_mode, CaptureMode::GamesOnly);

        // Accepts the stored names and the legacy Spanish one; anything unknown is a game mode.
        for (name, mode) in [("games", CaptureMode::GamesOnly), ("juegos", CaptureMode::GamesOnly), ("screen", CaptureMode::Screen)] {
            assert_eq!(CaptureMode::from(name.to_string()), mode, "from {name}");
            let parsed: CaptureMode = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(parsed, mode, "deserializing {name}");
            let stored = serde_json::to_string(&mode).unwrap();
            assert_eq!(stored, format!("\"{}\"", <&'static str>::from(mode)), "serializing {name}");
            assert_eq!(serde_json::from_str::<CaptureMode>(&stored).unwrap(), mode, "round-trip {name}");
        }
        assert_eq!(CaptureMode::from("unknown".to_string()), CaptureMode::GamesOnly);
    }
}
