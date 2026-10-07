use std::{fs, path::{Path, PathBuf}};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const APP_NAME: &str = "TeleRoute";

pub fn set_autostart(enabled: bool, exe: &Path) -> anyhow::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY)?;
    if enabled {
        key.set_value(APP_NAME, &format!("\"{}\" --autostart", exe.display()))?;
    } else {
        let _ = key.delete_value(APP_NAME);
    }
    Ok(())
}

pub fn is_elevated() -> bool {
    let output = std::process::Command::new("net").arg("session").output();
    matches!(output, Ok(o) if o.status.success())
}


mod embedded {
    include!(concat!(env!("OUT_DIR"), "/wintun_embedded.rs"));
}

pub fn ensure_wintun_dll() -> anyhow::Result<PathBuf> {

    let base = std::env::temp_dir().join("TeleRoute").join("runtime");
    fs::create_dir_all(&base)?;
    let target = base.join("wintun-0.14.1.dll");

    let needs_write = match fs::read(&target) {
        Ok(existing) => existing.as_slice() != embedded::WINTUN_DLL,
        Err(_) => true,
    };
    if needs_write {
        let tmp = target.with_extension("dll.tmp");
        fs::write(&tmp, embedded::WINTUN_DLL)?;
        fs::rename(&tmp, &target)?;
    }
    Ok(target)
}
