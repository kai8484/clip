//! App data folder, log file (`clip.log`), and timestamps.
//!
//! Self-contained layout: data, clips, and logs live alongside the executable,
//! so the whole folder can be copied or moved anywhere.

use std::io::Write;
use std::path::{Path, PathBuf};

use windows::Win32::System::SystemInformation::GetLocalTime;

/// Directory containing the running executable (without the filename).
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
}

/// Data and log directory: `<exe>\data`.
pub fn app_dir() -> PathBuf {
    exe_dir().join("data")
}

fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn file_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn log(msg: &str) {
    let dir = app_dir();
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("clip.log");
    if let Ok(m) = std::fs::metadata(&p) {
        if m.len() > 512 * 1024 {
            let _ = std::fs::remove_file(&p);
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "[{}] {}", timestamp(), msg);
    }
    #[cfg(debug_assertions)]
    eprintln!("{msg}");
}
