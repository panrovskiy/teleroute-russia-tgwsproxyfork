use crate::{config::{AppConfig, Mode, TelegramFrontend}, logging, proxy::socks5::Socks5Server, statistics::{Statistics, StatsSnapshot}};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tracing::info;

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
        if matches!(
            self.telemetry.read().status,
            ConnectionStatus::Connected | ConnectionStatus::Connecting
        ) {
            return;
        }

        let config = self.config.read().clone();

        // Remote MTProto mode is a separate user-supplied proxy and intentionally
        // does not provide the local Telegram WS bridge/call path.
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
                    t.error = "External MTProto requires server, port and secret".into();
                    return;
                }

                let proxy_id = format!("v3:{:{}:{}", server, config.mtproto.port, secret);

                if config.telegram.auto_configure
                    && config.telegram.configured_mtproto.as_deref() != Some(proxy_id.as_str())
                {
                    match crate::platform::windows::open_telegram_mtproto_proxy(
                        server,
                        config.mtproto.port,
                        secret,
                    ) {
                        Ok(()) => {
                            let mut cfg = self.config.write();
                            cfg.telegram.configured_mtproto = Some(proxy_id);
                            let _ = cfg.save();
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to open external MTProto link"),
                    }
                }

                let mut t = self.telemetry.write();
                t.status = ConnectionStatus::Connected;
                t.transport = "External MTProto".into();
                t.dc = "remote proxy".into();
            }

            return;
        }

        self.telemetry.write().status = ConnectionStatus::Connecting;
        self.telemetry.write().error.clear();

        let stats = self.stats.clone();
        let telemetry = self.telemetry.clone();
        let config_store = self.config.clone();
        let token = CancellationToken::new();
        *self.shutdown.write() = Some(token.clone());

        self.runtime.spawn(async move {
            let mtproto_pool = crate::websocket::WebSocketPool::default();

            let frontend_result: anyhow::Result<(String, String)> = match config.telegram.frontend {
                TelegramFrontend::MtprotoWs => (async {
                    let server = crate::proxy::mtproto::MtprotoServer::new(
                        config.clone(),
                        stats.clone(),
                        mtproto_pool.clone(),
                    )?;

                    let listener = server.bind().await?;
                    let addr = listener.local_addr()?;

                    let child = token.child_token();
                    tokio::spawn(async move {
                        if let Err(e) = server.run_on_listener(listener, child).await {
                            tracing::error!(error = %e, "local MTProto frontend stopped");
                        }
                    });

                    #[cfg(windows)]
                    {
                        let host = config.telegram.mtproto_bind.clone();
                        let port = config.telegram.mtproto_port;
                        let secret = config.telegram.mtproto_secret.clone();
                        let proxy_id = format!("v3:{:{}:{}", host, port, secret);

                        if config.telegram.auto_configure
                            && config.telegram.configured_mtproto.as_deref() != Some(proxy_id.as_str())
                        {
                            let cfg_store = config_store.clone();
                            tokio::spawn(async move {
                                let tg_secret = format!("dd{secret}");
                                match crate::platform::windows::open_telegram_mtproto_proxy(
                                    &host,
                                    port,
                                    &tg_secret,
                                ) {
                                    Ok(()) => {
                                        let mut cfg = cfg_store.write();
                                        cfg.telegram.configured_mtproto = Some(proxy_id);
                                        if let Err(e) = cfg.save() {
                                            tracing::warn!(error = %e, "could not persist Telegram MTProto registration state");
                                        }
                                    }
                                    Err(e) => tracing::warn!(error = %e, "failed to open local Telegram MTProto link"),
                                }
                            });
                        }
                    }

                    Ok((
                        "MTProto WebSocket".into(),
                        format!("127.0.0.1:{}", addr.port()),
                    ))
                }).await,

                TelegramFrontend::Socks5 => (async {
                    let proxy = Socks5Server::new(config.clone(), stats.clone());
                    let listener = proxy.bind().await?;
                    let addr = listener.local_addr()?;

                    #[cfg(windows)]
                    {
                        let host = match config.proxy.bind.as_str() {
                            "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
                            other => other,
                        };
                        let port = config.proxy.port;
                        let proxy_id = format!("v3:{:{}", host, port);

                        if config.telegram.auto_configure
                            && config.telegram.configured_proxy.as_deref() != Some(proxy_id.as_str())
                        {
                            let cfg_store = config_store.clone();
                            let host_for_task = host.to_owned();
                            tokio::spawn(async move {
                                match crate::platform::windows::open_telegram_socks_proxy(
                                    &host_for_task,
                                    port,
                                ) {
                                    Ok(()) => {
                                        let mut cfg = cfg_store.write();
                                        cfg.telegram.configured_proxy = Some(proxy_id);
                                        let _ = cfg.save();
                                    }
                                    Err(e) => tracing::warn!(error = %e, "failed to open Telegram SOCKS5 link"),
                                }
                            });
                        }
                    }

                    let child = token.child_token();
                    tokio::spawn(async move {
                        if let Err(e) = proxy.run_on_listener(listener, child).await {
                            tracing::error!(error = %e, "SOCKS5 frontend stopped");
                        }
                    });

                    Ok((
                        "SOCKS5".into(),
                        format!("127.0.0.1:{}", addr.port()),
                    ))
                }).await
            };

            let (transport, frontend_addr) = match frontend_result {
                Ok(v) => v,
                Err(e) => {
                    let mut t = telemetry.write();
                    t.status = ConnectionStatus::Error;
                    t.error = format!("Telegram frontend failed: {e}");
                    return;
                }
            };

            // TUN is optional. Telegram WS media uses the negative-DC routing
            // path (kwsN-1) directly, matching Flowseal's architecture.
            if config.tun.enabled && matches!(config.routing.mode, Mode::Calls | Mode::Full) {
                telemetry.write().tun = "STARTING".into();
                telemetry.write().udp = "STARTING".into();
                telemetry.write().calls = "STARTING".into();

                let tun_config = config.clone();
                let tun_stats = stats.clone();
                let tun_telemetry = telemetry.clone();
                let tun_shutdown = token.child_token();

                tokio::spawn(async move {
                    #[cfg(windows)]
                    {
                        match AssertUnwindSafe(crate::tun::TunManager::start(&tun_config, tun_stats))
                            .catch_unwind()
                            .await
                        {
                            Ok(Ok(tun)) => {
                                tun_telemetry.write().tun = "ACTIVE".into();
                                tun_telemetry.write().udp = "READY".into();
                                tun_telemetry.write().calls = "TUN ACTIVE; CALL NOT VERIFIED".into();
                                tokio::spawn(async move {
                                    let _ = AssertUnwindSafe(tun.run(tun_shutdown))
                                        .catch_unwind()
                                        .await;
                                });
                            }
                            Ok(Err(e)) => {
                                tun_telemetry.write().tun = "FAILED".into();
                                tun_telemetry.write().udp = "UNAVAILABLE".into();
                                tun_telemetry.write().calls = "WS media still available".into();
                                tun_telemetry.write().error = format!("Optional TUN initialization failed: {e}");
                            }
                            Err(panic) => {
                                tun_telemetry.write().tun = "FAILED".into();
                                tun_telemetry.write().udp = "UNAVAILABLE".into();
                                tun_telemetry.write().calls = "WS media still available".into();
                                tracing::error!(?panic, "optional TUN task panicked");
                            }
                        }
                    }
                });
            } else {
                telemetry.write().tun = "NOT REQUIRED".into();
                telemetry.write().udp = "NOT REQUIRED".into();
                telemetry.write().calls = "MEDIA WSS NOT VERIFIED".into();
            }

            {
                let mut t = telemetry.write();
                t.status = ConnectionStatus::Connected;
                t.transport = transport;
                t.dc = "automatic".into();
                t.error.clear();
            }

            info!("Telegram frontend ready at {frontend_addr}");
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
        let telemetry = self.telemetry.read().clone();
        let connected = matches!(
            telemetry.status,
            ConnectionStatus::Connected | ConnectionStatus::Connecting
        );
        let tun = telemetry.tun == "ACTIVE";

        let mut report = crate::diagnostics::run_full(&cfg, self.stats.clone(), tun).await;

        if !connected {
            if report.socks5 == "NOT LISTENING" {
                report.socks5 = "STOPPED (disconnected)".into();
            }
            if report.mtproto == "NOT LISTENING" {
                report.mtproto = "STOPPED (disconnected)".into();
            }
            report.call_transport = "NOT RUNNING (disconnected)".into();
            report.media_websocket = "NOT RUNNING (disconnected)".into();
        }

        report
    }
}

pub fn run() -> anyhow::Result<()> {
    let config = AppConfig::load_or_default()?;
    logging::init(&config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("tele-route").build()?;
    let ctx = AppContext::new(config.clone(), runtime);

    let args: Vec<String> = std::env::args().collect();
    let autostart = args.iter().any(|a| a == "--autostart");
    if autostart && config.autostart.start_connected {
        ctx.connect();
    }

    crate::gui::run(ctx, autostart && config.autostart.start_minimized)
}
