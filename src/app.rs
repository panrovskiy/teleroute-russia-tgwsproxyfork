use crate::{config::{AppConfig, Mode, TelegramFrontend}, logging, proxy::socks5::Socks5Server, statistics::{Statistics, StatsSnapshot}};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus { Disconnected, Connecting, Connected, Stopping, Error }
impl Default for ConnectionStatus { fn default() -> Self { Self::Disconnected } }

#[derive(Debug, Clone)]
pub struct Telemetry { pub status: ConnectionStatus, pub transport: String, pub dc: String, pub tun: String, pub udp: String, pub calls: String, pub telegram_client: String, pub error: String }
impl Default for Telemetry {
    fn default() -> Self { Self { status: ConnectionStatus::Disconnected, transport: "—".into(), dc: "—".into(), tun: "INACTIVE".into(), udp: "UNKNOWN".into(), calls: "NOT READY".into(), telegram_client: "DISCONNECTED".into(), error: String::new() } }
}

#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<RwLock<AppConfig>>,
    pub stats: Arc<Statistics>,
    pub telemetry: Arc<RwLock<Telemetry>>,
    ws_pool: crate::websocket::WebSocketPool,
    runtime: Arc<Runtime>,
    shutdown: Arc<RwLock<Option<CancellationToken>>>,
    frontend_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}
impl AppContext {
    pub fn new(config: AppConfig, runtime: Runtime) -> Self {
        Self { config: Arc::new(RwLock::new(config)), stats: Arc::new(Statistics::default()), telemetry: Arc::new(RwLock::new(Telemetry::default())), ws_pool: crate::websocket::WebSocketPool::default(), runtime: Arc::new(runtime), shutdown: Arc::new(RwLock::new(None)), frontend_task: Arc::new(RwLock::new(None)) }
    }

    pub fn snapshot(&self) -> StatsSnapshot { self.stats.snapshot() }
    pub(crate) fn spawn<F>(&self, future: F) where F: std::future::Future<Output = ()> + Send + 'static { self.runtime.spawn(future); }
    pub fn status(&self) -> Telemetry { self.telemetry.read().clone() }

    pub fn connect(&self) {
        if matches!(
            self.telemetry.read().status,
            ConnectionStatus::Connected | ConnectionStatus::Connecting | ConnectionStatus::Stopping
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
            t.telegram_client = "EXTERNAL MTProto HANDOFF".into();
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

                let proxy_id = format!("v4:{}:{}:{}", server, config.mtproto.port, secret);

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
                t.telegram_client = "EXTERNAL PROXY MANAGED BY TELEGRAM".into();
            }

            return;
        }

        {
            let mut t = self.telemetry.write();
            t.status = ConnectionStatus::Connecting;
            t.error.clear();
            t.telegram_client = "WAITING FOR TELEGRAM CLIENT".into();
        }

        let stats = self.stats.clone();
        let telemetry = self.telemetry.clone();
        let config_store = self.config.clone();
        let ws_pool = self.ws_pool.clone();
        let frontend_task_store = self.frontend_task.clone();
        let token = CancellationToken::new();
        *self.shutdown.write() = Some(token.clone());

        self.runtime.spawn(async move {
            let mtproto_pool = ws_pool;

            let frontend_result: anyhow::Result<(String, String)> = match config.telegram.frontend {
                TelegramFrontend::MtprotoWs => (async {
                    let warmup_pool = mtproto_pool.clone();
                    let warmup_config = config.clone();
                    let warmup_shutdown = token.child_token();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = warmup_shutdown.cancelled() => {}
                            _ = warmup_pool.warmup(&warmup_config) => {
                                tracing::info!("WSS route warmup completed");
                            }
                        }
                    });

                    let server = crate::proxy::mtproto::MtprotoServer::new(
                        config.clone(),
                        stats.clone(),
                        mtproto_pool.clone(),
                    )?;

                    let listener = server.bind().await?;
                    let addr = listener.local_addr()?;

                    let child = token.child_token();
                    let child_guard = child.clone();
                    let lifetime = token.clone();
                    let listener_telemetry = telemetry.clone();
                    let listener_task_store = frontend_task_store.clone();
                    let listener_task = tokio::spawn(async move {
                        let result = server.run_on_listener(listener, child).await;
                        if !child_guard.is_cancelled() {
                            lifetime.cancel();
                            let reason = match result {
                                Ok(()) => "MTProto listener exited unexpectedly".to_owned(),
                                Err(error) => format!("MTProto listener stopped: {error:#}"),
                            };
                            let mut status = listener_telemetry.write();
                            status.status = ConnectionStatus::Error;
                            status.telegram_client = "LISTENER STOPPED".into();
                            status.error = reason.clone();
                            tracing::error!(reason = %reason, "local MTProto frontend stopped unexpectedly");
                        } else {
                            tracing::info!("local MTProto listener exited after shutdown");
                        }
                    });
                    *listener_task_store.write() = Some(listener_task);

                    #[cfg(windows)]
                    {
                        let host = config.telegram.mtproto_bind.clone();
                        let port = config.telegram.mtproto_port;
                        let secret = config.telegram.mtproto_secret.clone();
                        // v4 marks the plain-secret client URI. Earlier versions
                        // prepended "dd", which selects Telegram Fake-TLS mode; this
                        // listener only parses the ordinary obfuscated MTProto header.
                        let proxy_id = format!("v5:{}:{}:{}", host, port, secret);

                        if config.telegram.auto_configure
                            && config.telegram.configured_mtproto.as_deref() != Some(proxy_id.as_str())
                        {
                            let cfg_store = config_store.clone();
                            tokio::spawn(async move {
                                // This listener implements the regular secret-based obfuscated
                                // MTProto header, not Telegram's Fake-TLS ("dd") transport.
                                match crate::platform::windows::open_telegram_mtproto_proxy(
                                    &host,
                                    port,
                                    &secret,
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
                    let proxy = Socks5Server::with_pool(config.clone(), stats.clone(), mtproto_pool.clone());
                    let warmup_proxy = proxy.clone();
                    let warmup_shutdown = token.child_token();
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = warmup_shutdown.cancelled() => {}
                            _ = warmup_proxy.warmup_wss_pool() => {
                                tracing::info!("SOCKS5 WSS route warmup completed");
                            }
                        }
                    });

                    let listener = proxy.bind().await?;
                    let addr = listener.local_addr()?;

                    #[cfg(windows)]
                    {
                        let host = match config.proxy.bind.as_str() {
                            "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
                            other => other,
                        };
                        let port = config.proxy.port;
                        let proxy_id = format!("v3:{}:{}", host, port);

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
                    let child_guard = child.clone();
                    let lifetime = token.clone();
                    let listener_telemetry = telemetry.clone();
                    let listener_task_store = frontend_task_store.clone();
                    let listener_task = tokio::spawn(async move {
                        let result = proxy.run_on_listener(listener, child).await;
                        if !child_guard.is_cancelled() {
                            lifetime.cancel();
                            let reason = match result {
                                Ok(()) => "SOCKS5 listener exited unexpectedly".to_owned(),
                                Err(error) => format!("SOCKS5 listener stopped: {error:#}"),
                            };
                            let mut status = listener_telemetry.write();
                            status.status = ConnectionStatus::Error;
                            status.telegram_client = "LISTENER STOPPED".into();
                            status.error = reason.clone();
                            tracing::error!(reason = %reason, "SOCKS5 frontend stopped unexpectedly");
                        } else {
                            tracing::info!("local SOCKS5 listener exited after shutdown");
                        }
                    });
                    *listener_task_store.write() = Some(listener_task);

                    Ok((
                        "SOCKS5".into(),
                        format!("127.0.0.1:{}", addr.port()),
                    ))
                }).await
            };

            let (transport, frontend_addr) = match frontend_result {
                Ok(v) => v,
                Err(e) => {
                    token.cancel();
                    let mut t = telemetry.write();
                    t.status = ConnectionStatus::Error;
                    t.telegram_client = "LISTENER FAILED TO START".into();
                    t.error = format!("Telegram frontend failed: {e}");
                    return;
                }
            };

            // Disconnect may happen while the listener is still starting.
            // Never publish a connected state or launch optional services after cancellation.
            if token.is_cancelled() {
                return;
            }

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
                                tun_telemetry.write().calls = "MEDIA WSS CHECKING".into();
                                tokio::spawn(async move {
                                    let _ = AssertUnwindSafe(tun.run(tun_shutdown))
                                        .catch_unwind()
                                        .await;
                                });
                            }
                            Ok(Err(e)) => {
                                tun_telemetry.write().tun = "FAILED".into();
                                tun_telemetry.write().udp = "UNAVAILABLE".into();
                                tun_telemetry.write().calls = "MEDIA WSS CHECKING".into();
                                tun_telemetry.write().error = format!("Optional TUN initialization failed: {e}");
                            }
                            Err(panic) => {
                                tun_telemetry.write().tun = "FAILED".into();
                                tun_telemetry.write().udp = "UNAVAILABLE".into();
                                tun_telemetry.write().calls = "MEDIA WSS CHECKING".into();
                                tracing::error!(?panic, "optional TUN task panicked");
                            }
                        }
                    }
                });
            } else {
                telemetry.write().tun = "NOT REQUIRED".into();
                telemetry.write().udp = "NOT REQUIRED".into();
                telemetry.write().calls = "MEDIA WSS CHECKING".into();
            }

            {
                let mut t = telemetry.write();
                t.status = ConnectionStatus::Connected;
                t.transport = transport;
                t.dc = "automatic".into();
                t.telegram_client = "WAITING FOR TELEGRAM CLIENT".into();
                t.error.clear();
            }

            // Track valid client sessions independently from listener health. If
            // Telegram is closed or this proxy is no longer selected, the UI changes
            // to "NO VALID TELEGRAM SESSION" instead of remaining green.
            let session_stats = stats.clone();
            let session_telemetry = telemetry.clone();
            let session_shutdown = token.child_token();
            tokio::spawn(async move {
                loop {
                    if session_shutdown.is_cancelled() {
                        break;
                    }

                    let active = session_stats.active_connections.load(std::sync::atomic::Ordering::Relaxed);
                    {
                        let mut status = session_telemetry.write();
                        if status.status == ConnectionStatus::Connected {
                            status.telegram_client = if active > 0 {
                                format!("CONNECTED · {active} active session(s)")
                            } else {
                                "NO VALID TELEGRAM SESSION".into()
                            };
                        } else if status.status == ConnectionStatus::Disconnected
                            || status.status == ConnectionStatus::Stopping
                        {
                            break;
                        }
                    }

                    tokio::select! {
                        _ = session_shutdown.cancelled() => break,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                    }
                }
            });

            // Probe media WSS separately using the same route pool as real client sessions.
            let monitor_config = config.clone();
            let monitor_stats = stats.clone();
            let monitor_pool = mtproto_pool.clone();
            let monitor_telemetry = telemetry.clone();
            let monitor_shutdown = token.child_token();
            tokio::spawn(async move {
                loop {
                    if monitor_shutdown.is_cancelled() {
                        break;
                    }

                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(12),
                        crate::websocket::probe_media_latency_with_pool(
                            &monitor_config,
                            monitor_pool.clone(),
                        ),
                    )
                    .await;

                    if monitor_shutdown.is_cancelled() {
                        break;
                    }

                    match result {
                        Ok(Some(latency_ms)) => {
                            if latency_ms > 0 {
                                monitor_stats.ping_ms.store(
                                    latency_ms,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            let mut status = monitor_telemetry.write();
                            if status.status != ConnectionStatus::Disconnected
                                && status.status != ConnectionStatus::Stopping
                            {
                                status.calls = if latency_ms > 0 {
                                    "MEDIA WSS READY · CALL NOT END-TO-END TESTED".into()
                                } else {
                                    "MEDIA WSS RECENTLY VERIFIED · CALL NOT END-TO-END TESTED".into()
                                };
                            }
                            tracing::info!(latency_ms, "Telegram media WSS route is reachable or was recently verified");
                        }
                        Ok(None) | Err(_) => {
                            // A probe can fail transiently after an earlier media
                            // route succeeded. The probe itself applies a short
                            // last-success grace window; None means no recent route
                            // was available either.
                            let mut status = monitor_telemetry.write();
                            if status.status != ConnectionStatus::Disconnected
                                && status.status != ConnectionStatus::Stopping
                            {
                                status.calls = "MEDIA WSS FAILED".into();
                            }
                            tracing::warn!("Telegram media WSS route probe failed");
                        }
                    }

                    tokio::select! {
                        _ = monitor_shutdown.cancelled() => break,
                        _ = tokio::time::sleep(std::time::Duration::from_secs(45)) => {}
                    }
                }
            });

            info!("Telegram frontend ready at {frontend_addr}");
        });
    }


    pub fn disconnect(&self) {
        let token = self.shutdown.write().take();
        let listener_task = self.frontend_task.write().take();
        let stopping_local_frontend = token.is_some() || listener_task.is_some();
        if let Some(token) = token {
            token.cancel();
        }

        self.stats.ping_ms.store(0, std::sync::atomic::Ordering::Relaxed);
        let mode = self.config.read().routing.mode;
        {
            let mut t = self.telemetry.write();
            t.status = if stopping_local_frontend {
                ConnectionStatus::Stopping
            } else {
                ConnectionStatus::Disconnected
            };
            t.transport = "—".into();
            t.tun = "INACTIVE".into();
            t.udp = "UNKNOWN".into();
            t.calls = "NOT READY".into();
            t.telegram_client = if mode == Mode::Mtproto && !stopping_local_frontend {
                "EXTERNAL PROXY MAY REMAIN ACTIVE".into()
            } else if stopping_local_frontend {
                "STOPPING LOCAL LISTENER".into()
            } else {
                "DISCONNECTED".into()
            };
            t.error = if mode == Mode::Mtproto && !stopping_local_frontend {
                "This mode hands an external MTProto server to Telegram. TeleRoute cannot switch off that remote service; disable the selected proxy in Telegram's Connection settings.".into()
            } else {
                String::new()
            };
        }

        let pool = self.ws_pool.clone();
        let telemetry = self.telemetry.clone();
        let stats = self.stats.clone();

        if let Some(mut listener_task) = listener_task {
            // Wait for the real listener future to exit before telling the UI
            // that the local proxy is off. A fixed sleep can show DISCONNECTED
            // while the server still owns its listening socket.
            self.runtime.spawn(async move {
                let stopped = tokio::time::timeout(
                    std::time::Duration::from_secs(4),
                    &mut listener_task,
                )
                .await
                .is_ok();

                if !stopped {
                    listener_task.abort();
                    let _ = listener_task.await;
                    pool.close_pooled_streams().await;
                    let mut status = telemetry.write();
                    if status.status == ConnectionStatus::Stopping {
                        status.status = ConnectionStatus::Error;
                        status.telegram_client = "LISTENER FORCED TO STOP".into();
                        status.error = "The local listener did not shut down cleanly; its task was stopped forcibly.".into();
                    }
                    tracing::error!("local Telegram listener exceeded the shutdown timeout");
                    return;
                }

                // Sessions inherit the same cancellation token. Give their
                // guards a short window to drop counters before reporting stop.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                while stats.active_connections.load(std::sync::atomic::Ordering::Relaxed) > 0
                    && tokio::time::Instant::now() < deadline
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }

                pool.close_pooled_streams().await;
                let active = stats.active_connections.load(std::sync::atomic::Ordering::Relaxed);
                let mut status = telemetry.write();
                if status.status == ConnectionStatus::Stopping {
                    if active == 0 {
                        status.status = ConnectionStatus::Disconnected;
                        status.telegram_client = "DISCONNECTED · LOCAL LISTENER STOPPED".into();
                        status.error.clear();
                        tracing::info!("local Telegram listener stopped; listening socket released");
                    } else {
                        status.status = ConnectionStatus::Error;
                        status.telegram_client = "CLIENT SESSIONS STILL CLOSING".into();
                        status.error = format!("{active} local client session(s) did not close in time.");
                        tracing::error!(active, "local client sessions did not drain during shutdown");
                    }
                }
            });
        } else if stopping_local_frontend {
            // Disconnect was clicked while the listener was still being created.
            // The startup task checks the same cancelled token before publishing
            // Connected; its child listeners are cancelled as soon as they start.
            self.runtime.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                pool.close_pooled_streams().await;
                let mut status = telemetry.write();
                if status.status == ConnectionStatus::Stopping {
                    status.status = ConnectionStatus::Disconnected;
                    status.telegram_client = "DISCONNECTED · STARTUP CANCELLED".into();
                    status.error.clear();
                }
            });
        } else {
            self.runtime.spawn(async move {
                pool.close_pooled_streams().await;
            });
        }
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

        let mut report = crate::diagnostics::run_full_with_pool(
            &cfg,
            self.stats.clone(),
            tun,
            self.ws_pool.clone(),
        ).await;

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
    // Use one explicit Rustls provider. Quinn enables ring, while Rustls defaults
    // can also enable aws-lc-rs; leaving selection implicit can panic at WSS setup.
    let _ = rustls::crypto::ring::default_provider().install_default();

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
