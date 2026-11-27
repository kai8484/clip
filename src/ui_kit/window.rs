//! Native window helpers: locating the app's own window and opening folders in Explorer.

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};

/// Top-level window with `title` belonging to this app binary, avoiding collisions with
/// Explorer folders of the same name.
pub fn find_own_window(title: &str) -> Option<HWND> {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId};
    struct State {
        title: Vec<u16>,
        found: Option<HWND>,
    }
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let s = &mut *(lp.0 as *mut State);
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(h, &mut buf).max(0) as usize;
        if n == s.title.len() && buf[..n] == s.title[..] {
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            if is_own_process(pid) {
                s.found = Some(h);
                return false.into();
            }
        }
        true.into()
    }
    let mut s = State { title: title.encode_utf16().collect(), found: None };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut s as *mut State as isize));
    }
    s.found
}

fn is_own_process(pid: u32) -> bool {
    use windows::core::PWSTR;
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
        let me = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
            .unwrap_or_else(|| "clip.exe".into());
        path.ends_with(&me) || path.ends_with("\\clip.exe")
    }
}

/// Opens `dir` in File Explorer (creating it if missing).
/// Refuses anything that is not a directory, so a dropped `.exe`/`.bat` is never handed
/// to the shell, and launches `%SystemRoot%\explorer.exe` by path rather than PATH lookup.
pub fn open_folder(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    if !std::fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
        return;
    }
    let Some(explorer) = explorer_exe() else { return };
    let _ = std::process::Command::new(explorer).arg(dir).spawn();
}

fn explorer_exe() -> Option<std::path::PathBuf> {
    use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
    let mut buf = [0u16; 260];
    let n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let root = String::from_utf16_lossy(&buf[..n]);
    let p = std::path::Path::new(&root).join("explorer.exe");
    p.is_file().then_some(p)
}
