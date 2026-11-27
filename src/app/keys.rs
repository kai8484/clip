//! Key display names and active-key polling for hotkey capture.

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC};

pub const MOD_ALT: u32 = 1;
pub const MOD_CTRL: u32 = 2;
pub const MOD_SHIFT: u32 = 4;

pub fn hotkey_name(vk: u32, mods: u32) -> String {
    let mut s = String::new();
    if mods & MOD_CTRL != 0 {
        s += "Ctrl + ";
    }
    if mods & MOD_SHIFT != 0 {
        s += "Shift + ";
    }
    if mods & MOD_ALT != 0 {
        s += "Alt + ";
    }
    s + &vk_name(vk)
}

pub fn vk_name(vk: u32) -> String {
    let fixed = match vk {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x13 => "Pause",
        0x20 => "Space",
        0x21 => "PgUp",
        0x22 => "PgDn",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2C => "PrtSc",
        0x2D => "Insert",
        0x2E => "Del",
        0x6A => "Num *",
        0x6B => "Num +",
        0x6D => "Num -",
        0x6E => "Num .",
        0x6F => "Num /",
        0x91 => "Scroll Lock",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        _ => unsafe {
            let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
            let mut buf = [0u16; 64];
            let n = GetKeyNameTextW((scan << 16) as i32, &mut buf);
            if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                format!("Key {vk:#04X}")
            }
        },
    }
}

fn down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

/// Returns the first currently pressed non-modifier key along with active modifiers.
pub fn pressed_key() -> Option<(u32, u32)> {
    for vk in 0x08..=0xFEi32 {
        // Ignore modifiers, Windows keys, menu, Caps Lock, Num Lock, and mouse buttons.
        if matches!(vk, 0x10 | 0x11 | 0x12 | 0x14 | 0x5B | 0x5C | 0x5D | 0x90 | 0xA0..=0xA5) {
            continue;
        }
        if down(vk) {
            let mut m = 0;
            if down(0x11) {
                m |= MOD_CTRL;
            }
            if down(0x10) {
                m |= MOD_SHIFT;
            }
            if down(0x12) {
                m |= MOD_ALT;
            }
            return Some((vk as u32, m));
        }
    }
    None
}
