use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=WINTUN_DLL");
    println!("cargo:rerun-if-changed=wintun/wintun.dll");

    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    // Always start the desktop application elevated on Windows. TUN/Wintun
    // creation needs administrator privileges, so the UAC prompt is shown once
    // at process startup instead of blocking the GUI when Connect is pressed.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level=requireAdministrator");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    let configured = env::var_os("WINTUN_DLL").map(PathBuf::from);
    let candidates = configured.into_iter()
        .chain([manifest.join("wintun/wintun.dll"), manifest.join("wintun.dll")]);

    let source = candidates.into_iter().find(|p| p.is_file()).unwrap_or_else(|| {
        panic!("wintun.dll is required for Windows builds. The release workflow downloads the official signed Wintun 0.14.1 DLL before compilation.");
    });

    let embedded = out_dir.join("wintun.dll");
    fs::copy(&source, &embedded).expect("failed to copy wintun.dll into OUT_DIR");

    // Do not embed an absolute Windows path into generated Rust source.
    // Using OUT_DIR at compile time keeps the generated source valid on
    // Windows paths containing backslashes and avoids raw-string delimiter
    // mistakes.
    let generated = out_dir.join("wintun_embedded.rs");
    fs::write(
        generated,
        "pub static WINTUN_DLL: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/wintun.dll\"));\n",
    ).expect("failed to generate embedded Wintun source");
}
