#[cfg(windows)]
pub mod windows;

#[cfg(not(windows))]
pub mod windows {
    use std::path::Path;
    pub fn set_autostart(_: bool, _: &Path) -> anyhow::Result<()> { Ok(()) }
    pub fn is_elevated() -> bool { false }
}
