fn main() {
    for icon in [
        "../icons/icon.ico",
        "../icons/icon.png",
        "../icons/32x32.png",
        "../icons/128x128.png",
        "../icons/128x128@2x.png",
        "../icons/tray.png",
    ] {
        println!("cargo:rerun-if-changed={icon}");
    }
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=msvcrt");
        println!("cargo:rustc-link-lib=ucrt");
        println!("cargo:rustc-link-lib=vcruntime");
    }
    tauri_build::build()
}
