//! Active game detection: finds a foreground process covering an entire monitor
//! (exclusive fullscreen or borderless windowed), excluding known non-game apps.
//! Used for monitor selection, "Games only" video mode, and the "Game" audio channel.
//! Remains locked while the process stays alive, even across Alt+Tab.

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsZoomed,
};

/// Apps that may go fullscreen without being games (lowercase, without `.exe`).
const NOT_GAMES: &[&str] = &[
    "chrome", "msedge", "firefox", "opera", "brave", "vivaldi",
    "vlc", "mpc-hc", "mpc-hc64", "mpc-be", "mpc-be64", "mpv", "potplayer", "potplayer64", "potplayermini", "potplayermini64",
    "microsoft.media.player", "video.ui", "wmplayer",
    "explorer", "applicationframehost", "powerpnt", "lockapp", "searchhost", "startmenuexperiencehost", "shellexperiencehost",
    // Windows Snipping Tool covers the whole monitor while selecting a capture region.
    "snippingtool", "screenclippinghost",
    // Chat, music, editors, photo viewers and browsers other than the ones above.
    "discord", "spotify", "windowsterminal", "code", "devenv", "photos", "microsoft.photos",
    "obs64", "obs32", "zen", "floorp", "librewolf", "arc", "waterfox",
    "clip",
];

/// Window classes of desktop and taskbar windows, which never count as games.
const SHELL_CLASSES: &[&str] = &["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

const FS_EVERY: i64 = 2_500_000; // 250 ms
const ALIVE_EVERY: i64 = 30_000_000; // 3 s
const STILL_ACTIVE: u32 = 259;

struct Proc {
    pid: u32,
    exe: String,
    handle: HANDLE,
    /// Last fullscreen window seen for the game (identifies which monitor it is on).
    window: HWND,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[derive(Default)]
pub struct Detector {
    game: Option<Proc>,
    /// True while the game is currently in the foreground fullscreen.
    in_front: bool,
    /// Last fullscreen PID confirmed not to be a game (avoids re-querying).
    not_game: u32,
    next_fs: i64,
    next_alive: i64,
}

impl Detector {
    /// Checks the foreground window (every 250 ms) and whether the game is still alive (every 3 s).
    /// Returns true if the detected game changed.
    pub fn poll(&mut self, now: i64) -> bool {
        // Devtools-only hook: treat a fake PID as the active game (`CLIP_TEST_GAME=<pid>`)
        // to test per-process audio loopback without running a fullscreen game.
        #[cfg(feature = "devtools")]
        if let Some(pid) = std::env::var("CLIP_TEST_GAME").ok().and_then(|s| s.parse().ok()) {
            if self.game.as_ref().is_none_or(|g| g.pid != pid) {
                self.game = Some(Proc {
                    pid,
                    exe: "test".into(),
                    handle: HANDLE::default(),
                    window: unsafe { GetForegroundWindow() },
                });
                self.in_front = true;
                return true;
            }
            self.in_front = true;
            return false;
        }
        let mut changed = false;
        if now >= self.next_alive {
            self.next_alive = now + ALIVE_EVERY;
            if self.game.as_ref().is_some_and(|g| !alive(g)) {
                self.game = None;
                changed = true;
            }
        }
        if now >= self.next_fs {
            self.next_fs = now + FS_EVERY;
            self.in_front = false;
            if let Some((pid, window)) = fullscreen_window() {
                if let Some(g) = self.game.as_mut().filter(|g| g.pid == pid) {
                    g.window = window;
                    self.in_front = true;
                } else if pid != self.not_game {
                    match open(pid, window) {
                        Some(p) if is_game(&p.exe) => {
                            self.game = Some(p);
                            self.in_front = true;
                            changed = true;
                        }
                        _ => self.not_game = pid,
                    }
                }
            }
        }
        changed
    }

    pub fn pid(&self) -> Option<u32> {
        self.game.as_ref().map(|g| g.pid)
    }

    /// Executable name without `.exe` (e.g., `"VALORANT-Win64-Shipping"`).
    pub fn name(&self) -> Option<String> {
        self.game.as_ref().map(|g| g.exe.clone())
    }

    /// Monitor containing the game window, if open and not minimized.
    pub fn monitor(&self) -> Option<HMONITOR> {
        let w = self.game.as_ref()?.window;
        unsafe {
            if !IsWindow(w).as_bool() || IsIconic(w).as_bool() {
                return None;
            }
            let m = MonitorFromWindow(w, MONITOR_DEFAULTTONULL);
            (!m.is_invalid()).then_some(m)
        }
    }

    /// True if the game is visible on its monitor: either in front or the foreground window
    /// is on a different monitor (looking at Discord on a second display does not cover the game).
    pub fn visible(&self) -> bool {
        let Some(m) = self.monitor() else { return false };
        self.in_front || foreground().is_none_or(|h| unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) } != m)
    }
}

/// Monitor of the active window (ignores desktop and taskbar since desktop spans all monitors).
pub fn active_monitor() -> Option<HMONITOR> {
    let m = unsafe { MonitorFromWindow(foreground()?, MONITOR_DEFAULTTONULL) };
    (!m.is_invalid()).then_some(m)
}

/// Foreground window, excluding desktop and taskbars.
fn foreground() -> Option<HWND> {
    unsafe {
        let h = GetForegroundWindow();
        if h.is_invalid() {
            return None;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(h, &mut cls).max(0) as usize;
        let cls = &cls[..n];
        (!SHELL_CLASSES.iter().any(|c| eq_utf16(cls, c))).then_some(h)
    }
}

/// Compares a UTF-16 slice against an ASCII literal without allocating.
fn eq_utf16(slice: &[u16], ascii: &str) -> bool {
    let bytes = ascii.as_bytes();
    slice.len() == bytes.len() && slice.iter().zip(bytes).all(|(&u, &b)| u == b as u16)
}

fn is_game(exe: &str) -> bool {
    !exe.is_empty() && !NOT_GAMES.iter().any(|n| exe.eq_ignore_ascii_case(n))
}

fn open(pid: u32, window: HWND) -> Option<Proc> {
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).unwrap_or_default();
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let path = if !handle.is_invalid() && QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            // Some anti-cheats block OpenProcess; fall back to the process snapshot name.
            process_name(pid)?
        };
        Some(Proc { pid, exe: exe_stem(&path), handle, window })
    }
}

/// `"C:\\...\\VALORANT-Win64-Shipping.exe"` -> `"VALORANT-Win64-Shipping"`.
fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or("");
    match file.len().checked_sub(4) {
        Some(n) if file.is_char_boundary(n) && file[n..].eq_ignore_ascii_case(".exe") => file[..n].to_string(),
        _ => file.to_string(),
    }
}

fn alive(p: &Proc) -> bool {
    if p.handle.is_invalid() {
        // Anti-cheat blocked OpenProcess: the cached window is a cheaper proof of life
        // than walking the whole process list every 3 s.
        if !p.window.is_invalid() {
            let mut wpid = 0u32;
            unsafe {
                if IsWindow(p.window).as_bool() {
                    GetWindowThreadProcessId(p.window, Some(&mut wpid));
                }
            }
            if wpid == p.pid {
                return true;
            }
        }
        return pid_exists(p.pid);
    }
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(p.handle, &mut code).is_ok() && code == STILL_ACTIVE }
}

/// True if a process with `pid` exists. Scans the process list without allocating.
fn pid_exists(pid: u32) -> bool {
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = false;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                if e.th32ProcessID == pid {
                    found = true;
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Executable name of `pid`, allocating only for the matching entry.
fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut name = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                if e.th32ProcessID == pid {
                    let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    name = Some(String::from_utf16_lossy(&e.szExeFile[..n]));
                    break;
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        name
    }
}

/// Returns `(PID, window)` if the foreground window covers its entire monitor.
/// Desktop, regular windows, and maximized windows are excluded: on a taskbar-less monitor
/// a maximized window also covers the full monitor, which would misclassify Discord or Spotify
/// as games. Borderless fullscreen games resize to the monitor bounds rather than maximizing.
fn fullscreen_window() -> Option<(u32, HWND)> {
    unsafe {
        let h = foreground()?;
        if IsZoomed(h).as_bool() {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).ok()?;
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        // Borderless windows can sit 1 px inside the monitor bounds on some setups.
        if !(r.left <= m.left + 1 && r.top <= m.top + 1 && r.right >= m.right - 1 && r.bottom >= m.bottom - 1) {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        (pid != 0).then_some((pid, h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_utf16_matches_ascii() {
        let cls: Vec<u16> = "Shell_TrayWnd".encode_utf16().collect();
        assert!(eq_utf16(&cls, "Shell_TrayWnd"));
        assert!(!eq_utf16(&cls, "Shell_TrayWNd"), "case must matter");
        assert!(!eq_utf16(&cls, "Shell_TrayWn"), "length must match");
        assert!(!eq_utf16(&[], "Progman"));
        assert!(!eq_utf16(&cls, ""));
    }

    #[test]
    fn pid_exists_finds_self() {
        assert!(pid_exists(std::process::id()));
        assert!(!pid_exists(u32::MAX));
    }

    #[test]
    fn process_name_of_self_is_executable() {
        let name = process_name(std::process::id()).expect("own process must be listed");
        assert!(name.to_ascii_lowercase().ends_with(".exe"), "unexpected name {name}");
        assert_eq!(process_name(u32::MAX), None);
    }

    #[test]
    fn alive_trusts_window_when_handle_is_invalid() {
        let fg = unsafe { GetForegroundWindow() };
        if fg.is_invalid() {
            return; // no foreground window in this session
        }
        let mut wpid = 0u32;
        unsafe { GetWindowThreadProcessId(fg, Some(&mut wpid)) };
        if wpid == 0 {
            return;
        }
        let p = Proc { pid: wpid, exe: "test".into(), handle: HANDLE::default(), window: fg };
        assert!(alive(&p));
        // A stale window handle for another PID falls through to the process list.
        let stale = Proc { pid: wpid, exe: "test".into(), handle: HANDLE::default(), window: HWND::default() };
        assert!(alive(&stale));
    }

    #[test]
    #[ignore = "micro-benchmark"]
    fn perf_bench_game_alive_and_fg() {
        let fg = unsafe { GetForegroundWindow() };
        let mut pid = std::process::id();
        if !fg.is_invalid() {
            let mut wpid = 0u32;
            unsafe { GetWindowThreadProcessId(fg, Some(&mut wpid)) };
            if wpid != 0 {
                pid = wpid;
            }
        }
        let p = Proc { pid, exe: "test".into(), handle: HANDLE::default(), window: fg };
        let t0 = std::time::Instant::now();
        let mut ok = 0usize;
        for _ in 0..20 {
            if alive(&p) {
                ok += 1;
            }
        }
        let alive_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = std::time::Instant::now();
        let mut fg_hits = 0usize;
        for _ in 0..500 {
            if foreground().is_some() {
                fg_hits += 1;
            }
        }
        let fg_ms = t1.elapsed().as_secs_f64() * 1000.0;
        std::hint::black_box((ok, fg_hits));
        println!("BENCH game: alive_20x={alive_ms:.3}ms foreground_500x={fg_ms:.3}ms");
    }
}
