use directories_next::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{fs, net::IpAddr, path::PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode { Proxy, Calls, Full, Mtproto }
impl Default for Mode { fn default() -> Self { Self::Full } }


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MtprotoConfig {
    pub server: String,
    pub port: u16,
    pub secret: String,
}
impl Default for MtprotoConfig {
    fn default() -> Self {
        Self {
            server: String::new(),
            port: 443,
            secret: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub templates: Vec<String>,
    pub path: String,
    pub pool_size: usize,
}
impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            templates: vec![
                "wss://kws{dc}.web.telegram.org/apiws".into(),
                "wss://kws{dc}-1.web.telegram.org/apiws".into(),
                "wss://{dc_name}.web.telegram.org/apiws".into(),
            ],
            path: "/apiws".into(),
            pool_size: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeouts {
    pub socks_handshake_ms: u64,
    pub connect_ms: u64,
    pub idle_ms: u64,
    pub reconnect_ms: u64,
    pub udp_idle_ms: u64,
}
impl Default for Timeouts {
    fn default() -> Self { Self { socks_handshake_ms: 700, connect_ms: 4500, idle_ms: 90_000, reconnect_ms: 800, udp_idle_ms: 90_000 } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub bind: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}
impl Default for ProxyConfig { fn default() -> Self { Self { bind: "127.0.0.1".into(), port: 1080, username: None, password: None } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunConfig {
    pub enabled: bool,
    pub adapter_name: String,
    pub address: IpAddr,
    pub mtu: usize,
    pub telegram_udp_cidrs: Vec<String>,
    pub extra_udp_cidrs: Vec<String>,
    pub route_metric: u32,
}
impl Default for TunConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            adapter_name: "TeleRoute".into(),
            address: "10.250.0.1".parse().unwrap(),
            mtu: 1280,
            telegram_udp_cidrs: vec![
                "149.154.0.0/16".into(),
                "91.108.0.0/16".into(),
                "5.28.192.0/18".into(),
                "95.161.64.0/20".into(),
            ],
            extra_udp_cidrs: Vec::new(),
            route_metric: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayConfig {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub server_name: Option<String>,
    pub auth_token: Option<String>,
}
impl Default for RelayConfig { fn default() -> Self { Self { enabled: false, endpoint: None, server_name: None, auth_token: None } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub mode: Mode,
    pub route_all_traffic: bool,
    pub telegram_only: bool,
    pub prefer_wss: bool,
    pub direct_udp_fallback: bool,
    pub relay_udp_fallback: bool,
}
impl Default for RoutingConfig {
    fn default() -> Self { Self { mode: Mode::Full, route_all_traffic: false, telegram_only: true, prefer_wss: true, direct_udp_fallback: true, relay_udp_fallback: true } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig { pub level: String, pub max_file_mb: u64, pub keep_files: usize }
impl Default for LoggingConfig { fn default() -> Self { Self { level: "info".into(), max_file_mb: 8, keep_files: 5 } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutostartConfig {
    pub start_with_windows: bool,
    pub start_connected: bool,
    pub start_minimized: bool,
    pub minimize_to_tray: bool,
}
impl Default for AutostartConfig { fn default() -> Self { Self { start_with_windows: false, start_connected: false, start_minimized: false, minimize_to_tray: true } } }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub mtproto: MtprotoConfig,
    pub websocket: EndpointConfig,
    pub timeouts: Timeouts,
    pub tun: TunConfig,
    pub relay: RelayConfig,
    pub routing: RoutingConfig,
    pub logging: LoggingConfig,
    pub autostart: AutostartConfig,
    pub log_diagnostics: bool,
}
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            proxy: ProxyConfig::default(), mtproto: MtprotoConfig::default(), websocket: EndpointConfig::default(), timeouts: Timeouts::default(), tun: TunConfig::default(),
            relay: RelayConfig::default(), routing: RoutingConfig::default(), logging: LoggingConfig::default(), autostart: AutostartConfig::default(), log_diagnostics: true,
        }
    }
}
impl AppConfig {
    pub fn data_dir() -> PathBuf { ProjectDirs::from("org", "TeleRoute", "TeleRoute").map(|p| p.data_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from("data")) }
    pub fn config_path() -> PathBuf { Self::data_dir().join("config.toml") }
    pub fn logs_dir() -> PathBuf { Self::data_dir().join("logs") }
    pub fn load_or_default() -> anyhow::Result<Self> {
        let path = Self::config_path();
        if !path.exists() { let cfg = Self::default(); cfg.save()?; return Ok(cfg); }
        Ok(toml::from_str(&fs::read_to_string(path)?)?)
    }
    pub fn save(&self) -> anyhow::Result<()> {
        fs::create_dir_all(Self::data_dir())?;
        fs::write(Self::config_path(), toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn default_is_full() { assert_eq!(AppConfig::default().routing.mode, Mode::Full); }
    #[test] fn toml_roundtrip() {
        let cfg = AppConfig::default();
        let decoded: AppConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(decoded.proxy.port, 1080);
        assert_eq!(decoded.tun.mtu, 1280);
        assert!(!decoded.tun.telegram_udp_cidrs.is_empty());
    }

    #[test] fn legacy_config_without_mtproto_still_loads() {
        let legacy = r#"
[proxy]
bind = "127.0.0.1"
port = 1080
username = false
password = false
"#;
        let result: Result<AppConfig, _> = toml::from_str(legacy);
        assert!(result.is_err()); // other required sections are intentionally still validated.
        let mut value: toml::Value = toml::from_str(legacy).unwrap();
        value["mtproto"] = toml::Value::Table(toml::map::Map::new());
        let cfg: AppConfig = value.try_into().unwrap();
        assert_eq!(cfg.mtproto.port, 443);
    }
}
