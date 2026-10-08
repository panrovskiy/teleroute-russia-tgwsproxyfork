use crate::{config::AppConfig, statistics::Statistics, telegram::obfs2, websocket::{self, WebSocketPool}};
use std::{net::IpAddr, sync::{atomic::Ordering, Arc}, time::Duration};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{lookup_host, TcpListener, TcpStream}, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub struct Socks5Server {
    config: AppConfig,
    stats: Arc<Statistics>,
    ws_pool: WebSocketPool,
}

impl Socks5Server {
    pub fn new(config: AppConfig, stats: Arc<Statistics>) -> Self {
        Self {
            config,
            stats,
            ws_pool: WebSocketPool::default(),
        }
    }

    pub async fn warmup_wss_pool(&self) {
        self.ws_pool.warmup(&self.config).await;
    }

    pub async fn bind(&self) -> anyhow::Result<TcpListener> {
        let addr = format!("{}:{}", self.config.proxy.bind, self.config.proxy.port);
        Ok(TcpListener::bind(&addr).await?)
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let listener = self.bind().await?;
        self.run_on_listener(listener, shutdown).await
    }

    pub async fn run_on_listener(
        &self,
        listener: TcpListener,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let addr = listener.local_addr()?;
        info!(%addr, "SOCKS5 listening");

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                res = listener.accept() => {
                    let (stream, peer) = res?;
                    let this = self.clone();

                    tokio::spawn(async move {
                        this.stats.connections.fetch_add(1, Ordering::Relaxed);
                        this.stats.active_connections.fetch_add(1, Ordering::Relaxed);

                        if let Err(e) = this.handle(stream).await {
                            debug!(peer = %peer, error = %e, "SOCKS5 connection closed");
                        }

                        this.stats.active_connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
        }

        Ok(())
    }

    async fn handle(&self, mut stream: TcpStream) -> anyhow::Result<()> {
        self.negotiate(&mut stream).await?;
        let (host, port) = read_request(&mut stream).await?;

        send_success(&mut stream).await?;

        // Do not decide Telegram routing from the SOCKS destination alone.
        // Telegram Desktop can connect to a DC IP that is not in our static
        // bootstrap table. The obfuscated MTProto init contains the real DC.
        //
        // We therefore inspect a short prefix after SOCKS CONNECT. A valid
        // 64-byte MTProto init goes through WSS; everything else is passed
        // through unchanged to the original SOCKS destination.
        let (prefix, telegram_handshake) = if self.config.routing.prefer_wss && port == 443 {
            let prefix = read_probe_prefix(
                &mut stream,
                Duration::from_millis(self.config.timeouts.socks_handshake_ms.min(750)),
            )
            .await?;

            let handshake = if prefix.len() == 64 {
                match <[u8; 64]>::try_from(prefix.as_slice()) {
                    Ok(header) => obfs2::parse_server_header(header).ok(),
                    Err(_) => None,
                }
            } else {
                None
            };

            (prefix, handshake)
        } else {
            (Vec::new(), None)
        };

        if let Some(parsed) = telegram_handshake {
            if parsed.parsed.dc != 0 {
                let transport = crate::websocket::WebSocketTransport::with_pool(
                    self.config.clone(),
                    self.stats.clone(),
                    self.ws_pool.clone(),
                );

                let initial = <[u8; 64]>::try_from(prefix.as_slice()).unwrap();

                match transport.bridge(&mut stream, initial, parsed).await {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "WSS path failed; falling back to direct TCP"
                        );
                    }
                }
            }
        }

        let target_ip = resolve_first(&host, port).await.ok();
        let target = match target_ip {
            Some(ip) => format!("{}:{}", ip, port),
            None => format!("{}:{}", host, port),
        };

        if prefix.is_empty() {
            websocket::direct_tcp_bridge(stream, &target, self.stats.clone()).await?;
        } else {
            websocket::direct_tcp_bridge_with_prefix(
                stream,
                &target,
                &prefix,
                self.stats.clone(),
            )
            .await?;
        }

        Ok(())
    }

    async fn negotiate(&self, stream: &mut TcpStream) -> anyhow::Result<()> {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await?;

        if header[0] != 5 {
            anyhow::bail!("SOCKS5 version is not 5");
        }

        let mut methods = vec![0u8; header[1] as usize];
        stream.read_exact(&mut methods).await?;

        let auth = self.config.proxy.username.is_some();
        let chosen = if auth && methods.contains(&2) {
            2
        } else if !auth && methods.contains(&0) {
            0
        } else {
            0xff
        };

        stream.write_all(&[5, chosen]).await?;

        if chosen == 0xff {
            anyhow::bail!("SOCKS5 no compatible authentication");
        }

        if chosen == 2 {
            let mut h = [0u8; 2];
            stream.read_exact(&mut h).await?;

            let mut user = vec![0u8; h[1] as usize];
            stream.read_exact(&mut user).await?;

            let mut plen = [0u8; 1];
            stream.read_exact(&mut plen).await?;

            let mut pass = vec![0u8; plen[0] as usize];
            stream.read_exact(&mut pass).await?;

            let ok = self
                .config
                .proxy
                .username
                .as_deref()
                .map(|u| user == u.as_bytes())
                .unwrap_or(false)
                && self
                    .config
                    .proxy
                    .password
                    .as_deref()
                    .map(|p| pass == p.as_bytes())
                    .unwrap_or(false);

            stream.write_all(&[1, if ok { 0 } else { 1 }]).await?;

            if !ok {
                anyhow::bail!("SOCKS5 authentication failed");
            }
        }

        Ok(())
    }
}

impl Clone for Socks5Server {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            stats: self.stats.clone(),
            ws_pool: self.ws_pool.clone(),
        }
    }
}

async fn read_probe_prefix(
    stream: &mut TcpStream,
    timeout_duration: Duration,
) -> anyhow::Result<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + timeout_duration;
    let mut out = Vec::with_capacity(64);
    let mut buf = [0u8; 64];

    while out.len() < 64 {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }

        let remaining = deadline.saturating_duration_since(now);

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let needed = 64 - out.len();
                let take = n.min(needed);
                out.extend_from_slice(&buf[..take]);

                if out.len() == 64 {
                    break;
                }
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => break,
        }
    }

    Ok(out)
}

async fn read_request(stream: &mut TcpStream) -> anyhow::Result<(String, u16)> {
    let mut h = [0u8; 4];
    stream.read_exact(&mut h).await?;

    if h[0] != 5 || h[1] != 1 {
        stream
            .write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0])
            .await?;
        anyhow::bail!("SOCKS5 only supports CONNECT");
    }

    let host = match h[3] {
        1 => {
            let mut b = [0u8; 4];
            stream.read_exact(&mut b).await?;
            IpAddr::from(b).to_string()
        }
        3 => {
            let mut l = [0u8; 1];
            stream.read_exact(&mut l).await?;

            if l[0] == 0 {
                anyhow::bail!("empty domain");
            }

            let mut b = vec![0u8; l[0] as usize];
            stream.read_exact(&mut b).await?;
            String::from_utf8(b)?
        }
        4 => {
            let mut b = [0u8; 16];
            stream.read_exact(&mut b).await?;
            IpAddr::from(b).to_string()
        }
        _ => anyhow::bail!("unsupported SOCKS5 address type"),
    };

    let mut p = [0u8; 2];
    stream.read_exact(&mut p).await?;

    Ok((host, u16::from_be_bytes(p)))
}

pub async fn send_success(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream
        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}

pub async fn send_failure(stream: &mut TcpStream, code: u8) -> anyhow::Result<()> {
    stream
        .write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}

async fn resolve_first(host: &str, port: u16) -> anyhow::Result<IpAddr> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }

    let mut it = lookup_host((host, port)).await?;

    it.next()
        .map(|s| s.ip())
        .ok_or_else(|| anyhow::anyhow!("DNS returned no addresses"))
}
