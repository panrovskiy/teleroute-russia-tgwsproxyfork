use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=WINTUN_DLL");
    println!("cargo:rerun-if-changed=wintun/wintun.dll");

    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

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

    let generated = out_dir.join("wintun_embedded.rs");
    fs::write(
        generated,
        format!(
            "pub static WINTUN_DLL: &[u8] = include_bytes!(r#\"{}\");\n",
            embedded.display()
        ),
    ).expect("failed to generate embedded Wintun source");
}
