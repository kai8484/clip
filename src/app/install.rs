//! First-run install: where clip lives, and copying the executable plus its data there.
//!
//! The wizard defaults to `%LOCALAPPDATA%\Programs\clip`, but any user-writable folder works.
//! Everything clip needs (`clip.exe`, `data\`, `clips\`) stays inside the chosen folder.

use std::path::{Component, Path, PathBuf};

use windows::core::{HSTRING, Interface};
use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, SHGetKnownFolderPath, ShellLink, FOLDERID_LocalAppData, KNOWN_FOLDER_FLAG,
};

use super::autostart;
use crate::config::{self, Config};
use crate::log::exe_dir;

/// Result of [`install_into`].
#[derive(Debug)]
pub struct InstallOutcome {
    pub installed_dir: PathBuf,
    pub installed_exe: PathBuf,
    pub clips_dir: PathBuf,
    /// True when clip already lives in the target folder (no copy needed).
    pub in_place: bool,
    /// The effective configuration of the installed copy.
    pub config: Config,
}

/// `%LOCALAPPDATA%\Programs\clip`, with fallbacks that still keep the install per-user.
pub fn default_install_dir() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(local_app_data_folder);
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    default_install_dir_from(local.as_deref(), profile.as_deref(), &exe_dir())
}

/// Testable form of [`default_install_dir`]: explicit sources, no environment lookups.
pub fn default_install_dir_from(
    local_app_data: Option<&Path>,
    user_profile: Option<&Path>,
    exe_dir: &Path,
) -> PathBuf {
    if let Some(base) = local_app_data.filter(|p| !p.as_os_str().is_empty()) {
        return base.join("Programs").join("clip");
    }
    if let Some(profile) = user_profile.filter(|p| !p.as_os_str().is_empty()) {
        return profile.join("AppData").join("Local").join("Programs").join("clip");
    }
    exe_dir.join("clip")
}

/// `SHGetKnownFolderPath(FOLDERID_LocalAppData)`, used when the environment variable is missing.
fn local_app_data_folder() -> Option<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KNOWN_FOLDER_FLAG(0), None::<&HANDLE>).ok()?;
        let path = raw.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(raw.as_ptr() as *const _));
        path
    }
}

/// Trims a picked folder and keeps `<picked>` unless it is an existing, non-empty folder that is
/// not already called `clip`; then `clip` is appended so the app stays contained in one folder.
pub fn normalize_install_dir(picked: &Path) -> PathBuf {
    let dir = clean_path(picked);
    if dir.as_os_str().is_empty() {
        return default_install_dir();
    }
    let named_clip = dir
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("clip"));
    if named_clip || !dir.exists() || is_empty_dir(&dir) {
        dir
    } else {
        dir.join("clip")
    }
}

/// Windows path comparison: case-insensitive, `\\?\`-agnostic, and trailing separators ignored.
pub fn is_same_dir(a: &Path, b: &Path) -> bool {
    compare_key(a) == compare_key(b)
}

/// Copies (or reuses) clip into `target_dir` and prepares `data\config.json` and `clips\`.
pub fn install_into(
    current_exe: &Path,
    target_dir: &Path,
    create_shortcut: bool,
    start_with_windows: bool,
) -> Result<InstallOutcome, String> {
    // Drive roots must be rejected before normalization, which would otherwise turn `C:\` into
    // `C:\clip`.
    let requested = clean_path(target_dir);
    validate_target(&requested)?;
    let target = normalize_install_dir(&requested);

    let exe_dir = current_exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let in_place = is_same_dir(&exe_dir, &target);
    let data_dir = target.join("data");
    let clips_dir = target.join("clips");
    let installed_exe = target.join("clip.exe");

    create_dir(&target)?;
    create_dir(&data_dir)?;
    create_dir(&clips_dir)?;
    ensure_writable(&target)?;

    if !in_place {
        if let Err(e) = std::fs::copy(current_exe, &installed_exe) {
            // 5 = access denied, 32 = sharing violation: almost always a running clip.exe.
            if installed_exe.is_file() && matches!(e.raw_os_error(), Some(5) | Some(32)) {
                return Err("clip.exe in that folder is currently running — close it first.".into());
            }
            return Err(format!("Could not copy clip.exe to {}: {e}", installed_exe.display()));
        }
    }

    let config = load_or_create_config(&data_dir, &clips_dir)?;

    if create_shortcut {
        // Best effort: a missing Start Menu entry never blocks an install.
        let _ = create_start_menu_shortcut(&installed_exe);
    }
    if start_with_windows {
        autostart::set_for_exe(&installed_exe, true);
    }

    Ok(InstallOutcome { installed_dir: target, installed_exe, clips_dir, in_place, config })
}

/// Trimmed path without trailing separators or a `\\?\` verbatim prefix; a bare `C:` becomes
/// the root `C:\`. Saved config and shortcuts must never carry the verbatim prefix.
fn clean_path(picked: &Path) -> PathBuf {
    let raw = picked.as_os_str().to_string_lossy();
    let trimmed = raw.trim().trim_end_matches(['\\', '/']);
    let plain = strip_verbatim(trimmed);
    if plain.len() == 2 && plain.ends_with(':') {
        PathBuf::from(format!("{plain}\\"))
    } else {
        PathBuf::from(plain)
    }
}

/// Removes a `\\?\` verbatim prefix, keeping UNC paths (`\\?\UNC\srv\share` -> `\\srv\share`).
fn strip_verbatim(raw: &str) -> String {
    match raw.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => rest.to_string(),
        },
        None => raw.to_string(),
    }
}

fn compare_key(p: &Path) -> String {
    let normalized = p.as_os_str().to_string_lossy().replace('/', "\\");
    let plain = strip_verbatim(&normalized);
    plain.trim_end_matches('\\').to_lowercase()
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

fn validate_target(target: &Path) -> Result<(), String> {
    if target.as_os_str().is_empty() {
        return Err("Choose a folder to install clip.".into());
    }
    // A bare name like `myclip` would resolve against the process working directory, which is
    // not where the user thinks it is.
    if !target.is_absolute() {
        return Err("Choose a full folder path (or click Change to pick one).".into());
    }
    let raw = target.as_os_str().to_string_lossy();
    // Verbatim prefixes legitimately contain `?`, so validate the path after it.
    let checked = strip_verbatim(&raw);
    let checked = checked.as_str();
    if checked.contains('\0') || has_invalid_chars(checked) {
        return Err("That folder name contains characters Windows can't use.".into());
    }
    if is_drive_root(target) {
        return Err("Choose a folder inside a drive, not the drive root itself (for example D:\\Games\\clip).".into());
    }
    Ok(())
}

fn has_invalid_chars(s: &str) -> bool {
    s.chars().enumerate().any(|(i, c)| {
        if c.is_control() {
            return true;
        }
        match c {
            '<' | '>' | '"' | '|' | '?' | '*' => true,
            // Only the drive colon (`C:\...`) is allowed.
            ':' => !(i == 1 && s.as_bytes()[0].is_ascii_alphabetic()),
            _ => false,
        }
    })
}

fn is_drive_root(target: &Path) -> bool {
    let components: Vec<Component> = target.components().collect();
    matches!(
        components.as_slice(),
        [Component::Prefix(_), Component::RootDir] | [Component::RootDir]
    )
}

fn create_dir(dir: &Path) -> Result<(), String> {
    match std::fs::create_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "Windows did not allow writing to {}. Choose a folder in your user profile or another drive — Program Files needs an administrator.",
            dir.display()
        )),
        Err(e) => Err(format!("Could not create {}: {e}", dir.display())),
    }
}

fn ensure_writable(dir: &Path) -> Result<(), String> {
    let probe = dir.join(".clip-write-test");
    let res = std::fs::write(&probe, b"clip");
    let _ = std::fs::remove_file(&probe);
    match res {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "Windows did not allow writing to {}. Choose a folder in your user profile or another drive — Program Files needs an administrator.",
            dir.display()
        )),
        Err(e) => Err(format!("Could not write to {}: {e}", dir.display())),
    }
}

fn load_or_create_config(data_dir: &Path, clips_dir: &Path) -> Result<Config, String> {
    let path = data_dir.join("config.json");
    if path.is_file() {
        // Keep an existing configuration untouched: only sanitize the copy held in memory.
        let mut cfg = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_else(|| default_config(clips_dir));
        cfg.sanitize();
        return Ok(cfg);
    }
    let cfg = default_config(clips_dir);
    config::save_to(data_dir, &cfg).map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    Ok(cfg)
}

fn default_config(clips_dir: &Path) -> Config {
    Config { output_dir: clips_dir.to_path_buf(), ..Config::default() }
}

fn start_menu_programs_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("Microsoft").join("Windows").join("Start Menu").join("Programs"))
}

/// Creates `<APPDATA>\...\Programs\clip.lnk` through the Shell COM link object.
fn create_start_menu_shortcut(exe: &Path) -> windows::core::Result<()> {
    let Some(programs) = start_menu_programs_dir() else {
        return Ok(());
    };
    std::fs::create_dir_all(&programs)
        .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
    let lnk = programs.join("clip.lnk");
    let working_dir = exe.parent().unwrap_or(Path::new(""));

    unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let res = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(exe.as_os_str()))?;
            link.SetWorkingDirectory(&HSTRING::from(working_dir.as_os_str()))?;
            link.SetDescription(&HSTRING::from("clip instant-replay recorder"))?;
            link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0)?;
            let file: IPersistFile = link.cast()?;
            file.Save(&HSTRING::from(lnk.as_os_str()), BOOL(1))?;
            Ok(())
        })();
        if com {
            CoUninitialize();
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "clip-install-test-{}-{}-{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_install_dir_prefers_local_app_data() {
        let dir = default_install_dir_from(
            Some(Path::new(r"C:\Users\me\AppData\Local")),
            Some(Path::new(r"C:\Users\me")),
            Path::new(r"D:\Downloads"),
        );
        assert_eq!(dir, PathBuf::from(r"C:\Users\me\AppData\Local\Programs\clip"));
    }

    #[test]
    fn default_install_dir_falls_back_to_user_profile() {
        let dir = default_install_dir_from(None, Some(Path::new(r"C:\Users\me")), Path::new(r"D:\Downloads"));
        assert_eq!(dir, PathBuf::from(r"C:\Users\me\AppData\Local\Programs\clip"));
    }

    #[test]
    fn default_install_dir_falls_back_to_exe_dir() {
        let dir = default_install_dir_from(None, None, Path::new(r"D:\Downloads"));
        assert_eq!(dir, PathBuf::from(r"D:\Downloads\clip"));
    }

    #[test]
    fn normalize_keeps_a_folder_already_named_clip() {
        let root = temp_dir("named-clip");
        let clip = root.join("CLIP");
        std::fs::create_dir_all(&clip).unwrap();
        std::fs::write(clip.join("something.txt"), b"x").unwrap();
        assert_eq!(normalize_install_dir(&clip), clip);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_keeps_an_empty_existing_folder() {
        let root = temp_dir("empty");
        let empty = root.join("My Folder");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(normalize_install_dir(&empty), empty);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_keeps_a_folder_that_does_not_exist_yet() {
        let root = temp_dir("missing");
        let missing = root.join("Games").join("clip");
        assert_eq!(normalize_install_dir(&missing), missing);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_appends_clip_to_a_non_empty_other_folder() {
        let root = temp_dir("non-empty");
        let games = root.join("Games");
        std::fs::create_dir_all(&games).unwrap();
        std::fs::write(games.join("game.exe"), b"x").unwrap();
        assert_eq!(normalize_install_dir(&games), games.join("clip"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_trims_whitespace_and_trailing_separators() {
        assert_eq!(normalize_install_dir(Path::new(r"D:\Tools\ ")), PathBuf::from(r"D:\Tools"));
        assert_eq!(normalize_install_dir(Path::new("D:\\Tools\\\\")), PathBuf::from(r"D:\Tools"));
    }

    #[test]
    fn same_dir_ignores_case_and_verbatim_prefix() {
        assert!(is_same_dir(Path::new(r"C:\Users\Me\clip"), Path::new(r"c:\users\me\CLIP")));
        assert!(is_same_dir(Path::new(r"\\?\C:\Users\Me\clip"), Path::new(r"C:\Users\Me\clip")));
        assert!(is_same_dir(Path::new(r"C:\Users\Me\clip\"), Path::new(r"C:\Users\Me\clip")));
        assert!(!is_same_dir(Path::new(r"C:\Users\Me\clip"), Path::new(r"C:\Users\Me\clip2")));
    }

    #[test]
    fn install_into_creates_exe_data_and_clips() {
        let root = temp_dir("fresh-install");
        let src_dir = root.join("Downloads");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src_exe = src_dir.join("clip.exe");
        std::fs::write(&src_exe, b"fake-exe-bytes").unwrap();
        let target = root.join("Programs").join("clip");

        let out = install_into(&src_exe, &target, false, false).unwrap();

        assert_eq!(out.installed_dir, target);
        assert_eq!(out.installed_exe, target.join("clip.exe"));
        assert_eq!(out.clips_dir, target.join("clips"));
        assert!(!out.in_place);
        assert_eq!(std::fs::read(&out.installed_exe).unwrap(), b"fake-exe-bytes");
        assert!(out.clips_dir.is_dir());
        assert!(target.join("data").is_dir());

        let saved = std::fs::read_to_string(target.join("data").join("config.json")).unwrap();
        let cfg: Config = serde_json::from_str(&saved).unwrap();
        assert_eq!(cfg.output_dir, out.clips_dir);
        assert_eq!(out.config.output_dir, out.clips_dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_into_preserves_existing_config() {
        let root = temp_dir("preserve-config");
        let src_exe = root.join("clip.exe");
        std::fs::write(&src_exe, b"fake-exe-bytes").unwrap();
        let target = root.join("clip");
        std::fs::create_dir_all(target.join("data")).unwrap();

        let existing = Config {
            seconds: 45,
            output_dir: root.join("My Clips"),
            ..Config::default()
        };
        let original = serde_json::to_string_pretty(&existing).unwrap();
        std::fs::write(target.join("data").join("config.json"), &original).unwrap();

        let out = install_into(&src_exe, &target, false, false).unwrap();

        assert_eq!(out.config.seconds, 45);
        assert_eq!(out.config.output_dir, root.join("My Clips"));
        let saved = std::fs::read_to_string(target.join("data").join("config.json")).unwrap();
        assert_eq!(saved, original);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_into_an_existing_clip_folder_is_in_place() {
        let root = temp_dir("in-place");
        let exe_dir = root.join("clip");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let exe = exe_dir.join("clip.exe");
        std::fs::write(&exe, b"fake-exe-bytes").unwrap();

        let out = install_into(&exe, &exe_dir, false, false).unwrap();

        assert!(out.in_place);
        assert_eq!(out.installed_exe, exe);
        assert_eq!(out.config.output_dir, exe_dir.join("clips"));
        assert!(exe_dir.join("data").join("config.json").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_into_rejects_empty_and_drive_roots() {
        let root = temp_dir("invalid");
        let exe = root.join("clip.exe");
        std::fs::write(&exe, b"fake-exe-bytes").unwrap();

        assert!(install_into(&exe, Path::new(""), false, false).is_err());
        let err = install_into(&exe, Path::new(r"C:\"), false, false).unwrap_err();
        assert!(err.contains("drive root"), "unexpected error: {err}");
        assert!(install_into(&exe, Path::new(r"D:\Ga<mes"), false, false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_into_rejects_relative_paths() {
        let root = temp_dir("relative");
        let exe = root.join("clip.exe");
        std::fs::write(&exe, b"fake-exe-bytes").unwrap();

        let err = install_into(&exe, Path::new("myclip"), false, false).unwrap_err();
        assert!(err.contains("full folder path"), "unexpected error: {err}");
        // Nothing may be written next to the relative-looking target either.
        assert!(!Path::new("myclip").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn install_into_saves_clips_dir_without_verbatim_prefix() {
        let root = temp_dir("verbatim");
        let exe = root.join("clip.exe");
        std::fs::write(&exe, b"fake-exe-bytes").unwrap();
        let target = format!(r"\\?\{}", root.join("clip").display());

        let out = install_into(&exe, Path::new(&target), false, false).unwrap();

        assert_eq!(out.installed_dir, root.join("clip"));
        assert_eq!(out.config.output_dir, root.join("clip").join("clips"));
        let saved = std::fs::read_to_string(root.join("clip").join("data").join("config.json")).unwrap();
        assert!(!saved.contains(r"\\?\"), "verbatim prefix leaked into config: {saved}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
