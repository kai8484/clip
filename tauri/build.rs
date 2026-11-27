fn main() {
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=../assets/icon/app64.png");
    println!("cargo:rerun-if-changed=../assets/icon/tray32.png");
    tauri_build::build();
}
