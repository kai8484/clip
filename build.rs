//! Generates and embeds the multi-size `.ico` resource for the executable.

#[path = "src/ui_kit/icon_draw.rs"]
mod icon_draw;

use std::io::Write;

fn png(size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&icon_draw::app_rgba(size)).unwrap();
    }
    out
}

fn main() {
    println!("cargo:rerun-if-changed=src/ui_kit/icon_draw.rs");
    println!("cargo:rerun-if-changed=build.rs");
    // The icon resource belongs to executables built from this crate. This crate has no binary
    // target (the app binary lives in `tauri/` and embeds `tauri/icons/icon.ico` itself), and
    // embed-resource would otherwise link the resource into this library, duplicating the ICON
    // resource in every downstream executable and failing the link with CVT1100.
    if !std::path::Path::new("src/main.rs").exists() && !std::path::Path::new("src/bin").is_dir() {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 128, 256];
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| png(s)).collect();

    // .ico container with PNG images (supported since Windows Vista).
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&images) {
        let b = if *s >= 256 { 0 } else { *s as u8 };
        ico.extend_from_slice(&[b, b, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in &images {
        ico.extend_from_slice(img);
    }
    let ico_path = out.join("clip.ico");
    std::fs::write(&ico_path, ico).unwrap();

    let rc_path = out.join("clip.rc");
    let mut rc = std::fs::File::create(&rc_path).unwrap();
    writeln!(rc, "1 ICON \"{}\"", ico_path.display().to_string().replace('\\', "\\\\")).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE);
}
