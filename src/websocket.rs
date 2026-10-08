use crate::{config::AppConfig, statistics::Statistics, telegram::{dc, obfs2::{self, ClientObfs, ServerObfs}}};
use futures_util::{SinkExt, StreamExt};
use ctr::cipher::StreamCipher;
use std::{collections::HashMap, net::SocketAddr, sync::{atomic::Ordering, Arc}, time::Duration};
use tokio::sync::Mutex;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{lookup_host, TcpStream}, time::timeout};
use tokio_tungstenite::{client_async_tls_with_config, tungstenite::{client::IntoClientRequest, http::HeaderValue, Message}};
use tracing::{info, warn};

pub type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

#[derive(Clone, Default)]
pub struct WebSocketPool {
    inner: Arc<Mutex<HashMap<u16, Vec<WsStream>>>>,
}
impl WebSocketPool {
    pub async fn take(&self, dc: u16) -> Option<WsStream> {
        self.inner.lock().await.get_mut(&dc).and_then(|v| v.pop())
    }

    pub async fn warmup(&self, config: &AppConfig) {
        let target_size = config.websocket.pool_size.max(1);
        for dc in 1..=5u16 {
            loop {
                let current = self.inner.lock().await.get(&dc).map(|v| v.len()).unwrap_or(0);
                if current >= target_size {
                    break;
                }

                let transport = WebSocketTransport::with_pool(
                    config.clone(),
                    Arc::new(Statistics::default()),
                    self.clone(),
                );
                let Some(url) = transport.endpoints(dc).first().cloned() else {
                    break;
                };

                match timeout(
                    Duration::from_millis(config.timeouts.connect_ms),
                    transport.connect(&url, dc),
                ).await {
                    Ok(Ok((ws, _))) => self.inner.lock().await.entry(dc).or_default().push(ws),
                    _ => break,
                }
            }
        }
    }
}

pub struct WebSocketTransport {
    config: AppConfig,
    stats: Arc<Statistics>,
    pool: WebSocketPool,
}
impl WebSocketTransport {
    pub fn new(config: AppConfig, stats: Arc<Statistics>) -> Self {
        Self {
            config,
            stats,
            pool: WebSocketPool::default(),
        }
    }

    pub fn with_pool(config: AppConfig, stats: Arc<Statistics>, pool: WebSocketPool) -> Self {
        Self { config, stats, pool }
    }

    pub async fn bridge(
        &self,
        client: &mut TcpStream,
        initial_header: [u8; 64],
        client_obfs: ServerObfs,
    ) -> anyhow::Result<()> {
        let dc_id = client_obfs.parsed.dc.unsigned_abs() as u16;
        let media = client_obfs.parsed.dc < 0;
        self.stats.ws_attempts.fetch_add(1, Ordering::Relaxed);

        if let Some(mut ws) = self.pool.take(dc_id).await {
            self.stats.ws_success.fetch_add(1, Ordering::Relaxed);
            self.stats.current_dc.store(dc_id as i64, Ordering::Relaxed);
            self.stats.current_transport.store(1, Ordering::Relaxed);

            let upstream = obfs2::new_client(client_obfs.parsed.protocol, client_obfs.parsed.dc)?;
            ws.send(Message::Binary(upstream.wire_header.to_vec().into())).await?;
            return self.pipe(client, client_obfs, &mut ws, upstream).await;
        }

        // Race all configured WSS endpoints instead of trying them serially.
        // The first successful TLS/WebSocket handshake wins, reducing startup
        // latency significantly on networks where one endpoint is slow/blocked.
        let endpoints = self.endpoints(dc_id, media);
        let target_ip = dc::default_ipv4(dc_id)
            .ok_or_else(|| anyhow::anyhow!("no default Telegram DC address for DC{dc_id}"))?;
        let deadline = Duration::from_millis(self.config.timeouts.connect_ms);
        let mut attempts = futures_util::stream::FuturesUnordered::new();

        for url in endpoints {
            attempts.push(async {
                let result = timeout(deadline, self.connect(&url, dc_id, target_ip)).await;
                (url, result)
            });
        }

        let mut last_error: Option<anyhow::Error> = None;

        while let Some((url, result)) = attempts.next().await {
            match result {
                Ok(Ok((mut ws, _))) => {
                    self.stats.ws_success.fetch_add(1, Ordering::Relaxed);
                    self.stats.current_dc.store(dc_id as i64, Ordering::Relaxed);
                    self.stats.current_transport.store(1, Ordering::Relaxed);
                    info!(dc = dc_id, media, %url, "WSS transport connected");

                    let upstream = obfs2::new_client(client_obfs.parsed.protocol, client_obfs.parsed.dc)?;
                    ws.send(Message::Binary(upstream.wire_header.to_vec().into())).await?;
                    return self.pipe(client, client_obfs, &mut ws, upstream).await;
                }
                Ok(Err(e)) => last_error = Some(e.into()),
                Err(_) => last_error = Some(anyhow::anyhow!("WSS connect timeout")),
            }
        }

        if let Some(e) = last_error {
            warn!(dc = dc_id, error = %e, "all WSS endpoints failed");
        }

        let target = target_from_header(&initial_header, dc_id);
        anyhow::bail!(
            "no usable WSS endpoint for DC{}; direct target fallback={} (header preserved)",
            dc_id,
            target
        )
    }

    async fn connect(
        &self,
        url: &str,
        _dc_id: u16,
        target_ip: std::net::IpAddr,
    ) -> Result<(
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ), tokio_tungstenite::tungstenite::Error> {
        let mut req = url.into_client_request()?;
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static("binary"),
        );

        req.headers_mut().insert(
            "Origin",
            HeaderValue::from_static("https://web.telegram.org"),
        );
        req.headers_mut().insert(
            "User-Agent",
            HeaderValue::from_static(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
            ),
        );

        // Flowseal's bridge connects to the Telegram DC IP directly while
        // retaining kwsN.web.telegram.org as Host/SNI.
        let socket = TcpStream::connect(SocketAddr::new(target_ip, 443)).await?;
        socket.set_nodelay(true)?;
        client_async_tls_with_config(req, socket, None, None).await
    }

    fn endpoints(&self, dc_id: u16, media: bool) -> Vec<String> {
        let mut endpoints: Vec<String> = self.config.websocket.templates.iter()
            .map(|t| {
                t.replace("{dc}", &dc_id.to_string())
                    .replace("{dc_name}", dc::name(dc_id))
            })
            .collect();

        if endpoints.is_empty() {
            endpoints = if media {
                vec![
                    format!("wss://kws{dc_id}-1.web.telegram.org/apiws"),
                    format!("wss://kws{dc_id}.web.telegram.org/apiws"),
                ]
            } else {
                vec![format!("wss://kws{dc_id}.web.telegram.org/apiws")]
            };
        }

        if media {
            endpoints.sort_by_key(|u| if u.contains("-1.web.telegram.org") { 0 } else { 1 });
        } else {
            endpoints.retain(|u| !u.contains("-1.web.telegram.org"));
        }

        endpoints
    }

    async fn pipe(
        &self,
        client: &mut TcpStream,
        mut client_obfs: ServerObfs,
        ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        mut upstream: ClientObfs,
    ) -> anyhow::Result<()> {
        let (mut client_r, mut client_w) = client.split();
        let (mut ws_w, mut ws_r) = ws.split();
        let stats_up = self.stats.clone();
        let stats_down = self.stats.clone();

        let up = async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = client_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }

                let mut data = buf[..n].to_vec();
                client_obfs.decrypt.apply_keystream(&mut data);
                upstream.encrypt.apply_keystream(&mut data);

                stats_up.bytes_up.fetch_add(n as u64, Ordering::Relaxed);
                stats_up.packets_up.fetch_add(1, Ordering::Relaxed);
                ws_w.send(Message::Binary(data.into())).await?;
            }

            let _ = ws_w.send(Message::Close(None)).await;
            Ok::<(), anyhow::Error>(())
        };

        let down = async move {
            while let Some(msg) = ws_r.next().await {
                match msg? {
                    Message::Binary(data) => {
                        let mut data = data.to_vec();
                        upstream.decrypt.apply_keystream(&mut data);
                        client_obfs.encrypt.apply_keystream(&mut data);
                        stats_down.bytes_down.fetch_add(data.len() as u64, Ordering::Relaxed);
                        stats_down.packets_down.fetch_add(1, Ordering::Relaxed);
                        client_w.write_all(&data).await?;
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                    _ => {}
                }
            }
            Ok::<(), anyhow::Error>(())
        };

        tokio::select! {
            r = up => r?,
            r = down => r?,
        }

        Ok(())
    }
}

fn url_host(req: &tokio_tungstenite::tungstenite::handshake::client::Request) -> String {
    req.uri().host().unwrap_or_default().to_string()
}

fn target_from_header(_header: &[u8; 64], dc_id: u16) -> String {
    dc::default_ipv4(dc_id)
        .map(|ip| format!("{ip}:443"))
        .unwrap_or_else(|| format!("{}:443", dc::name(dc_id)))
}

pub async fn direct_tcp_bridge_with_prefix(
    mut client: TcpStream,
    target: &str,
    prefix: &[u8],
    stats: Arc<Statistics>,
) -> anyhow::Result<()> {
    let mut server = timeout(
        Duration::from_millis(7000),
        TcpStream::connect(target),
    ).await??;

    server.write_all(prefix).await?;
    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    stats.bytes_up.fetch_add(up + prefix.len() as u64, Ordering::Relaxed);
    stats.bytes_down.fetch_add(down, Ordering::Relaxed);
    stats.tcp_fallbacks.fetch_add(1, Ordering::Relaxed);
    stats.current_transport.store(2, Ordering::Relaxed);
    Ok(())
}

pub async fn direct_tcp_bridge(
    mut client: TcpStream,
    target: &str,
    stats: Arc<Statistics>,
) -> anyhow::Result<()> {
    let mut server = timeout(
        Duration::from_millis(7000),
        TcpStream::connect(target),
    ).await??;

    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    stats.bytes_up.fetch_add(up, Ordering::Relaxed);
    stats.bytes_down.fetch_add(down, Ordering::Relaxed);
    stats.tcp_fallbacks.fetch_add(1, Ordering::Relaxed);
    stats.current_transport.store(3, Ordering::Relaxed);
    Ok(())
}

pub async fn probe(config: &crate::config::EndpointConfig) -> bool {
    let Some(template) = config.templates.first() else { return false; };
    let url = template
        .replace("{dc}", "2")
        .replace("{dc_name}", "venus");

    let mut req = match url.clone().into_client_request() {
        Ok(r) => r,
        Err(_) => return false,
    };

    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static("binary"),
    );

    let target_ip = match dc::default_ipv4(2) {
        Some(ip) => ip,
        None => return false,
    };

    let socket = match timeout(
        Duration::from_millis(2500),
        TcpStream::connect(SocketAddr::new(target_ip, 443)),
    ).await {
        Ok(Ok(s)) => s,
        _ => return false,
    };

    req.headers_mut().insert(
        "Origin",
        HeaderValue::from_static("https://web.telegram.org"),
    );
    req.headers_mut().insert(
        "User-Agent",
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
        ),
    );

    matches!(
        timeout(
            Duration::from_millis(2500),
            client_async_tls_with_config(req, socket, None, None),
        ).await,
        Ok(Ok(_))
    )
}
