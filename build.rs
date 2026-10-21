use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets");

    // IMPORTANT: use CARGO_CFG_* instead of cfg!(...).
    // build.rs runs on the HOST, so cfg!(target_os) would report the host OS
    // and break cross-compilation.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    match target_os.as_str() {
        "windows" => embed_windows_resources(),
        "macos" => embed_macos_info_plist(),
        _ => {}
    }
}

fn embed_windows_resources() {
    let mut res = winresource::WindowsResource::new();

    // FileVersion / ProductVersion / ProductName / FileDescription are filled
    // automatically from Cargo.toml. Override only what you need.
    res.set_icon("assets/windows/min-paint.ico")
        .set_manifest_file("assets/windows/min-paint.manifest")
        .set("CompanyName", "mammb.com.")
        .set("LegalCopyright", "Copyright (C) mammb.com. All rights reserved.")
        .set("OriginalFilename", "min-paint.exe");

    // Cross-compiling from Linux/macOS with the GNU target needs mingw's windres.
    // It is auto-detected, but you can pin it explicitly if needed:
    // if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64") {
    //     res.set_windres_path("x86_64-w64-mingw32-windres");
    // }

    res.compile().expect("failed to compile Windows resources");
}

fn embed_macos_info_plist() {
    // Embeds Info.plist into the Mach-O binary (__TEXT,__info_plist).
    // This is enough for CLI tools and for bundle-less identification;
    // a proper .app bundle is produced by cargo-bundle / cargo-packager.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let plist = manifest_dir.join("assets/macos/Info.plist");

    println!("cargo:rerun-if-changed={}", plist.display());
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
