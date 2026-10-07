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

pub fn relaunch_as_admin_and_connect() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let exe_quoted = exe.to_string_lossy().replace('\'', "''");
    let command = format!(
        "Start-Process -FilePath '{}' -ArgumentList '--elevated-connect' -Verb RunAs",
        exe_quoted
    );

    let status = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .status()?;

    if !status.success() {
        anyhow::bail!("Administrator permission was not granted");
    }
    Ok(())
}

/// Ask Telegram Desktop to add the local SOCKS5 proxy.
pub fn open_telegram_socks_proxy(host: &str, port: u16) -> anyhow::Result<()> {
    let advertised_host = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    let uri = format!("tg://socks?server={advertised_host}&port={port}");
    std::process::Command::new("explorer.exe").arg(&uri).spawn()?;
    Ok(())
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
        if target.exists() { let _ = fs::remove_file(&target); }
        fs::rename(&tmp, &target)?;
    }
    Ok(target)
}
