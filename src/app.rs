use crate::{config::{AppConfig, Mode}, logging, proxy::socks5::Socks5Server, statistics::{Statistics, StatsSnapshot}};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus { Disconnected, Connecting, Connected, Error }
impl Default for ConnectionStatus { fn default() -> Self { Self::Disconnected } }

#[derive(Debug, Clone)]
pub struct Telemetry { pub status: ConnectionStatus, pub transport: String, pub dc: String, pub tun: String, pub udp: String, pub calls: String, pub error: String }
impl Default for Telemetry {
    fn default() -> Self { Self { status: ConnectionStatus::Disconnected, transport: "—".into(), dc: "—".into(), tun: "INACTIVE".into(), udp: "UNKNOWN".into(), calls: "NOT READY".into(), error: String::new() } }
}

#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<RwLock<AppConfig>>,
    pub stats: Arc<Statistics>,
    pub telemetry: Arc<RwLock<Telemetry>>,
    runtime: Arc<Runtime>,
    shutdown: Arc<RwLock<Option<CancellationToken>>>,
}
impl AppContext {
    pub fn new(config: AppConfig, runtime: Runtime) -> Self {
        Self { config: Arc::new(RwLock::new(config)), stats: Arc::new(Statistics::default()), telemetry: Arc::new(RwLock::new(Telemetry::default())), runtime: Arc::new(runtime), shutdown: Arc::new(RwLock::new(None)) }
    }

    pub fn snapshot(&self) -> StatsSnapshot { self.stats.snapshot() }
    pub(crate) fn spawn<F>(&self, future: F) where F: std::future::Future<Output = ()> + Send + 'static { self.runtime.spawn(future); }
    pub fn status(&self) -> Telemetry { self.telemetry.read().clone() }

    pub fn connect(&self) {
        if matches!(self.telemetry.read().status, ConnectionStatus::Connected | ConnectionStatus::Connecting) { return; }

        let config = self.config.read().clone();

        #[cfg(windows)]
        if config.routing.mode != Mode::Proxy && config.tun.enabled && !crate::platform::windows::is_elevated() {
            {
                let mut t = self.telemetry.write();
                t.status = ConnectionStatus::Connecting;
                t.tun = "ELEVATION REQUIRED".into();
                t.udp = "WAITING".into();
                t.calls = "WAITING".into();
                t.error.clear();
            }

            match crate::platform::windows::relaunch_as_admin_and_connect() {
                Ok(()) => {
                    // The elevated instance takes over. Do not continue starting
                    // a second non-elevated TUN session in this process.
                    std::process::exit(0);
                }
                Err(e) => {
                    let mut t = self.telemetry.write();
                    t.status = ConnectionStatus::Error;
                    t.tun = "FAILED".into();
                    t.udp = "UNAVAILABLE".into();
                    t.calls = "UNAVAILABLE".into();
                    t.error = format!("Administrator permission is required for Calls/Full mode: {e}");
                    return;
                }
            }
        }

        self.telemetry.write().status = ConnectionStatus::Connecting;
        self.telemetry.write().error.clear();
        let stats = self.stats.clone();
        let telemetry = self.telemetry.clone();
        let token = CancellationToken::new();
        *self.shutdown.write() = Some(token.clone());
        let shutdown_for_tasks = token.clone();

        self.runtime.spawn(async move {
            let proxy = Socks5Server::new(config.clone(), stats.clone());
            let listener = match proxy.bind().await {
                Ok(l) => l,
                Err(e) => {
                    let mut t = telemetry.write(); t.status = ConnectionStatus::Error; t.error = format!("SOCKS5 bind failed: {e}"); return;
                }
            };

            // Telegram Desktop does not automatically discover TeleRoute's local
            // SOCKS5 listener. Tell it explicitly through the supported tg://socks
            // deep link as soon as the listener is ready.
            #[cfg(windows)]
            if let Err(e) = crate::platform::windows::open_telegram_socks_proxy(
                &config.proxy.bind,
                config.proxy.port,
            ) {
                tracing::warn!(error = %e, "failed to open Telegram SOCKS5 setup link");
            }

            telemetry.write().tun = if matches!(config.routing.mode, Mode::Proxy) || !config.tun.enabled { "INACTIVE".into() } else { "STARTING".into() };

            if config.routing.mode != Mode::Proxy && config.tun.enabled {
                #[cfg(windows)]
                {
                    match crate::tun::TunManager::start(&config, stats.clone()).await {
                        Ok(tun) => {
                            telemetry.write().tun = "ACTIVE".into();
                            telemetry.write().udp = "READY".into();
                            telemetry.write().calls = "READY (transport-level)".into();
                            let child = shutdown_for_tasks.child_token();
                            tokio::spawn(async move { let _ = tun.run(child).await; });
                        }
                        Err(e) => {
                            telemetry.write().tun = "FAILED".into();
                            telemetry.write().udp = "UNAVAILABLE".into();
                            telemetry.write().calls = "UNAVAILABLE".into();
                            telemetry.write().error = format!("TUN initialization failed: {e}");
                        }
                    }
                }
                #[cfg(not(windows))]
                {
                    telemetry.write().tun = "UNSUPPORTED".into();
                    telemetry.write().calls = "UNAVAILABLE".into();
                }
            }

            proxy.warmup_wss_pool().await;
            telemetry.write().status = ConnectionStatus::Connected;
            telemetry.write().transport = if config.routing.prefer_wss { "WebSocket / fallback TCP".into() } else { "TCP".into() };
            telemetry.write().dc = "automatic".into();

            if let Err(e) = proxy.run_on_listener(listener, token.clone()).await {
                telemetry.write().status = ConnectionStatus::Error;
                telemetry.write().error = format!("Core stopped: {e}");
            } else if token.is_cancelled() {
                telemetry.write().status = ConnectionStatus::Disconnected;
                telemetry.write().transport = "—".into();
            }
        });
    }

    pub fn disconnect(&self) {
        if let Some(token) = self.shutdown.write().take() { token.cancel(); }
        let mut t = self.telemetry.write();
        t.status = ConnectionStatus::Disconnected; t.transport = "—".into(); t.tun = "INACTIVE".into(); t.udp = "UNKNOWN".into(); t.calls = "NOT READY".into();
    }

    pub fn save_config(&self) -> anyhow::Result<()> {
        let cfg = self.config.read().clone(); cfg.save()?;
        let exe = std::env::current_exe()?;
        crate::platform::windows::set_autostart(cfg.autostart.start_with_windows, &exe)?;
        Ok(())
    }

    pub async fn run_diagnostics(&self) -> crate::diagnostics::DiagnosticReport {
        let cfg = self.config.read().clone();
        let tun = self.telemetry.read().tun == "ACTIVE";
        crate::diagnostics::run_full(&cfg, self.stats.clone(), tun).await
    }
}

pub fn run() -> anyhow::Result<()> {
    let config = AppConfig::load_or_default()?;
    logging::init(&config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("tele-route").build()?;
    let ctx = AppContext::new(config.clone(), runtime);

    let args: Vec<String> = std::env::args().collect();
    let autostart = args.iter().any(|a| a == "--autostart");
    let elevated_connect = args.iter().any(|a| a == "--elevated-connect");

    if (autostart && config.autostart.start_connected) || elevated_connect {
        ctx.connect();
    }

    crate::gui::run(ctx, autostart && config.autostart.start_minimized)
}
