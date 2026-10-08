#[cfg(windows)]
pub mod windows;

#[cfg(not(windows))]
pub mod windows {
    use std::path::Path;
    pub fn set_autostart(_: bool, _: &Path) -> anyhow::Result<()> { Ok(()) }
    pub fn is_elevated() -> bool { false }
    pub fn ensure_elevated_at_start() -> anyhow::Result<bool> { Ok(false) }
    pub fn show_startup_error(_: &str) {}
    pub fn write_startup_error(_: &str) {}
}
