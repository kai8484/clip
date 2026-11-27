//! Executable run modes and inter-process coordination: CLI arguments, window titles, and mutexes.

use std::ffi::OsString;
use std::path::Path;

use windows::core::{w, PCWSTR};

const BACKGROUND: &str = "--background";

pub enum Mode {
    /// Recorder with system tray icon. `background`: don't open a window on launch (autostart).
    Tray { background: bool },
}

impl Mode {
    fn args(&self) -> Vec<OsString> {
        match self {
            Mode::Tray { background: false } => vec![],
            Mode::Tray { background: true } => vec![BACKGROUND.into()],
        }
    }

    /// Full command line (used for Windows autostart).
    pub fn command_line(&self, exe: &Path) -> String {
        let mut s = format!("\"{}\"", exe.display());
        for a in self.args() {
            s.push(' ');
            s.push_str(&a.to_string_lossy());
        }
        s
    }
}

/// Windows belonging to clip.
#[derive(Clone, Copy)]
pub enum Window {
    Settings,
}

impl Window {
    pub fn title(self) -> &'static str {
        match self {
            Window::Settings => "clip",
        }
    }
}

/// Single-instance mutex for the tray recorder.
pub const TRAY_MUTEX: PCWSTR = w!("Local\\clip.Main");
