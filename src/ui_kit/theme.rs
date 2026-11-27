//! Shared motion timing and text helpers used by the Tauri UI.

use std::sync::OnceLock;

pub const T_TOGGLE: f32 = 0.15;

/// Cubic ease-in-out (0..1 -> 0..1).
pub fn ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let f = 2.0 * t - 2.0;
        0.5 * f * f * f + 1.0
    }
}

/// Respects Windows "Animation effects" setting: disables motion when turned off.
pub fn reduced_motion() -> bool {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    static R: OnceLock<bool> = OnceLock::new();
    *R.get_or_init(|| {
        let mut on = BOOL(1);
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some(&mut on as *mut BOOL as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_ok()
        };
        ok && !on.as_bool()
    })
}

pub fn fmt_secs(s: u32) -> String {
    match (s / 60, s % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}
