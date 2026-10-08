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
        if matches!(self.telemetry.read().status, ConnectionStatus::Connected | ConnectionStatus::Connecting) {
            return;
        }

        let config = self.config.read().clone();

        if config.routing.mode == Mode::Mtproto {
            let mut t = self.telemetry.write();
            t.status = ConnectionStatus::Connecting;
            t.error.clear();
            t.tun = "DISABLED".into();
            t.udp = "DISABLED".into();
            t.calls = "UNAVAILABLE".into();
            drop(t);

            #[cfg(windows)]
            {
                let server = config.mtproto.server.trim();
                let secret = config.mtproto.secret.trim();
                if server.is_empty() || secret.is_empty() {
                    let mut t = self.telemetry.write();
                    t.status = ConnectionStatus::Error;
                    t.error = "MTProto requires server, port and secret".into();
                    return;
                }

                match crate::platform::windows::open_telegram_mtproto_proxy(
                    server,
                    config.mtproto.port,
                    secret,
                ) {
                    Ok(()) => {
                        let mut t = self.telemetry.write();
                        t.status = ConnectionStatus::Connected;
                        t.transport = "MTProto".into();
                        t.dc = "remote proxy".into();
                    }
                    Err(e) => {
                        let mut t = self.telemetry.write();
                        t.status = ConnectionStatus::Error;
                        t.error = format!("Could not open MTProto proxy in Telegram: {e}");
                    }
                }
            }
            return;
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
                    let mut t = telemetry.write();
                    t.status = ConnectionStatus::Error;
                    t.error = format!("SOCKS5 bind failed: {e}");
                    return;
                }
            };

            #[cfg(windows)]
            if let Err(e) = crate::platform::windows::open_telegram_socks_proxy(
                &config.proxy.bind,
                config.proxy.port,
            ) {
                tracing::warn!(error = %e, "failed to open Telegram SOCKS5 setup link");
            }

            if config.routing.mode == Mode::Calls || config.routing.mode == Mode::Full {
                if config.tun.enabled {
                    telemetry.write().tun = "STARTING".into();

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
                        telemetry.write().udp = "UNAVAILABLE".into();
                        telemetry.write().calls = "UNAVAILABLE".into();
                    }
                } else {
                    telemetry.write().tun = "DISABLED".into();
                    telemetry.write().udp = "UNAVAILABLE".into();
                    telemetry.write().calls = "UNAVAILABLE".into();
                }
            } else {
                telemetry.write().tun = "INACTIVE".into();
                telemetry.write().udp = "NOT USED".into();
                telemetry.write().calls = "UNAVAILABLE".into();
            }

            // Do not wait for remote WSS warm-up before declaring the local
            // listener ready. The warm-up continues independently.
            let warmup_proxy = proxy.clone();
            tokio::spawn(async move {
                warmup_proxy.warmup_wss_pool().await;
            });

            {
                let mut t = telemetry.write();
                t.status = ConnectionStatus::Connected;
                t.transport = if config.routing.prefer_wss {
                    "WebSocket / fallback TCP".into()
                } else {
                    "TCP".into()
                };
                t.dc = "automatic".into();
            }

            if let Err(e) = proxy.run_on_listener(listener, token.clone()).await {
                let mut t = telemetry.write();
                t.status = ConnectionStatus::Error;
                t.error = format!("Proxy core stopped: {e}");
            } else if token.is_cancelled() {
                let mut t = telemetry.write();
                t.status = ConnectionStatus::Disconnected;
                t.transport = "—".into();
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
