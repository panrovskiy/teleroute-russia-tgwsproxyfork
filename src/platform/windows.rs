use std::{ffi::OsStr, fs, os::windows::{ffi::OsStrExt, process::CommandExt}, path::{Path, PathBuf}};

use windows_sys::{
    Win32::{
        Foundation::HWND,
        UI::{
            Shell::ShellExecuteW,
            WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, SW_SHOWNORMAL},
        },
    },
};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const APP_NAME: &str = "TeleRoute";

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(value: &str) -> Vec<u16> {
    wide(OsStr::new(value))
}

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

pub fn ensure_elevated_at_start() -> anyhow::Result<bool> {
    if is_elevated() {
        return Ok(false);
    }

    let exe = std::env::current_exe()?;
    let verb = wide_str("runas");
    let exe_w = wide(&exe.into_os_string());

    let result = unsafe {
        ShellExecuteW(
            0 as HWND,
            verb.as_ptr(),
            exe_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    if (result as isize) <= 32 {
        anyhow::bail!("Windows administrator elevation was not granted (ShellExecuteW code {})", result as isize);
    }

    Ok(true)
}

pub fn show_startup_error(message: &str) {
    let title = wide_str("TeleRoute");
    let text = wide_str(message);
    unsafe {
        MessageBoxW(
            0 as HWND,
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn write_startup_error(message: &str) {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        let dir = std::path::PathBuf::from(base).join("TeleRoute");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("startup-error.log");
        let _ = std::fs::write(&path, format!("{message}\n"));
    }
}


/// Ask Telegram Desktop to add the local SOCKS5 proxy through its registered URI handler.
/// ShellExecuteW uses the Windows shell association for tg:// instead of launching Explorer.
fn launch_uri_detached(uri: String) -> anyhow::Result<()> {
    // Do not invoke ShellExecuteW from the elevated GUI process. Starting the
    // protocol through a hidden cmd.exe keeps Telegram in the user's normal
    // desktop context and isolates any protocol-handler failure from TeleRoute.
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    std::process::Command::new("cmd.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["/C", "start", "", &uri])
        .spawn()
        .map(|_| ())
        .map_err(Into::into)
}

pub fn open_telegram_socks_proxy(host: &str, port: u16) -> anyhow::Result<()> {
    let advertised_host = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };

    launch_uri_detached(format!(
        "tg://socks?server={advertised_host}&port={port}"
    ))
}

pub fn open_telegram_mtproto_proxy(server: &str, port: u16, secret: &str) -> anyhow::Result<()> {
    launch_uri_detached(format!(
        "tg://proxy?server={server}&port={port}&secret={secret}"
    ))
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
        if target.exists() {
            let _ = fs::remove_file(&target);
        }
        fs::rename(&tmp, &target)?;
    }
    Ok(target)
}
