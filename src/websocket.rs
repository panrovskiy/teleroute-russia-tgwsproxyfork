use crate::{config::AppConfig, statistics::Statistics, telegram::{dc, obfs2::{self, ClientObfs, ServerObfs}}};
use futures_util::{FutureExt, SinkExt, StreamExt};
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
    // Remember a recently successful route per DC/media/test-DC tuple so
    // subsequent Telegram connections do not repeat cold endpoint discovery.
    preferred: Arc<Mutex<HashMap<(u16, bool, bool), String>>>,
}
impl WebSocketPool {
    pub async fn take(&self, dc: u16) -> Option<WsStream> {
        self.inner.lock().await.get_mut(&dc).and_then(|v| v.pop())
    }

    async fn preferred(&self, dc: u16, media: bool, is_test_dc: bool) -> Option<String> {
        self.preferred.lock().await.get(&(dc, media, is_test_dc)).cloned()
    }

    async fn remember(&self, dc: u16, media: bool, is_test_dc: bool, url: &str) {
        self.preferred.lock().await.insert((dc, media, is_test_dc), url.to_owned());
    }

    async fn forget(&self, dc: u16, media: bool, is_test_dc: bool) {
        self.preferred.lock().await.remove(&(dc, media, is_test_dc));
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
                let Some(url) = transport.endpoints(dc, false, false).first().cloned() else {
                    break;
                };
                let target_ip = target_ip_for_url(&url, dc, false);

                match timeout(
                    Duration::from_millis(config.timeouts.connect_ms.min(1800).max(500)),
                    transport.connect(&url, dc, target_ip),
                ).await {
                    Ok(Ok((ws, _))) => {
                        self.remember(dc, false, false, &url).await;
                        self.inner.lock().await.entry(dc).or_default().push(ws);
                    }
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
        let raw_dc = client_obfs.parsed.dc;
        let media = raw_dc < 0;
        let encoded_dc = raw_dc.unsigned_abs() as u16;
        let is_test_dc = (10_001u16..=10_005u16).contains(&encoded_dc);
        let mut dc_id = if is_test_dc { encoded_dc - 10_000 } else { encoded_dc };
        if dc_id == 203 { dc_id = 2; }
        if !(1u16..=5u16).contains(&dc_id) {
            anyhow::bail!("unsupported Telegram DC index in MTProto handshake");
        }
        let relay_dc = if media { -(dc_id as i16) } else { dc_id as i16 };
        self.stats.ws_attempts.fetch_add(1, Ordering::Relaxed);

        // The pool is keyed only by DC; keep media/test routes isolated.
        if !media && !is_test_dc {
        if let Some(mut ws) = self.pool.take(dc_id).await {
            self.stats.ws_success.fetch_add(1, Ordering::Relaxed);
            self.stats.current_dc.store(dc_id as i64, Ordering::Relaxed);
            self.stats.current_transport.store(1, Ordering::Relaxed);

            let upstream = obfs2::new_client(client_obfs.parsed.protocol, relay_dc)?;
            ws.send(Message::Binary(upstream.wire_header.to_vec().into())).await?;
            return self.pipe(client, client_obfs, &mut ws, upstream).await;
        }
        }

        // Race all configured WSS endpoints instead of trying them serially.
        // The first successful TLS/WebSocket handshake wins, reducing startup
        // latency significantly on networks where one endpoint is slow/blocked.
        let mut endpoints = self.endpoints(dc_id, media, is_test_dc);

        // Try the last known-good endpoint alone first. On a healthy route this
        // avoids opening several redundant TLS/WebSocket handshakes for every
        // Telegram TCP connection.
        if let Some(preferred_url) = self.pool.preferred(dc_id, media, is_test_dc).await {
            if endpoints.iter().any(|candidate| candidate == &preferred_url) {
                let preferred_timeout =
                    Duration::from_millis(self.config.timeouts.connect_ms.min(1200).max(500));
                let target_ip = target_ip_for_url(&preferred_url, dc_id, is_test_dc);
                if let Ok(Ok((mut ws, _))) = timeout(
                    preferred_timeout,
                    self.connect(&preferred_url, dc_id, target_ip),
                ).await {
                    self.pool.remember(dc_id, media, is_test_dc, &preferred_url).await;
                    self.stats.ws_success.fetch_add(1, Ordering::Relaxed);
                    self.stats.current_dc.store(dc_id as i64, Ordering::Relaxed);
                    self.stats.current_transport.store(1, Ordering::Relaxed);
                    info!(dc = dc_id, media, url = %preferred_url, "reused known-good WSS route");

                    let upstream = obfs2::new_client(client_obfs.parsed.protocol, relay_dc)?;
                    ws.send(Message::Binary(upstream.wire_header.to_vec().into())).await?;
                    return self.pipe(client, client_obfs, &mut ws, upstream).await;
                }
            }

            self.pool.forget(dc_id, media, is_test_dc).await;
            endpoints.retain(|candidate| candidate != &preferred_url);
        }

        // Cap the wait per endpoint even if config.toml contains a larger
        // value; four concurrent attempts still leave slower fallback routes
        // a chance, but a dead hostname cannot hold startup for many seconds.
        let deadline =
            Duration::from_millis(self.config.timeouts.connect_ms.min(1800).max(500));
        let mut remaining = endpoints.into_iter();
        let mut attempts = futures_util::stream::FuturesUnordered::new();

        // Bound concurrent endpoint handshakes; failed/blocked entries are
        // replaced one at a time so a large fallback list cannot spawn a storm.
        for _ in 0..4 {
            if let Some(url) = remaining.next() {
                let attempt_url = url.clone();
                let target_ip = target_ip_for_url(&url, dc_id, is_test_dc);
                attempts.push(async move {
                    let result = timeout(deadline, self.connect(&attempt_url, dc_id, target_ip)).await;
                    (url, result)
                }.boxed());
            }
        }

        let mut last_error: Option<anyhow::Error> = None;

        while let Some((url, result)) = attempts.next().await {
            match result {
                Ok(Ok((mut ws, _))) => {
                    self.pool.remember(dc_id, media, is_test_dc, &url).await;
                    self.stats.ws_success.fetch_add(1, Ordering::Relaxed);
                    self.stats.current_dc.store(dc_id as i64, Ordering::Relaxed);
                    self.stats.current_transport.store(1, Ordering::Relaxed);
                    info!(dc = dc_id, media, %url, "WSS transport connected");

                    let upstream = obfs2::new_client(client_obfs.parsed.protocol, relay_dc)?;
                    ws.send(Message::Binary(upstream.wire_header.to_vec().into())).await?;
                    return self.pipe(client, client_obfs, &mut ws, upstream).await;
                }
                Ok(Err(e)) => last_error = Some(e.into()),
                Err(_) => last_error = Some(anyhow::anyhow!("WSS connect timeout")),
            }

            if let Some(next_url) = remaining.next() {
                let attempt_url = next_url.clone();
                let target_ip = target_ip_for_url(&next_url, dc_id, is_test_dc);
                attempts.push(async move {
                    let result = timeout(deadline, self.connect(&attempt_url, dc_id, target_ip)).await;
                    (next_url, result)
                }.boxed());
            }
        }

        if let Some(e) = last_error {
            warn!(dc = dc_id, error = %e, "all official WSS and Cloudflare fallback endpoints failed");
        }

        // Final direct TCP fallback mirrors Flowseal's fallback order. It is
        // attempted with a strict timeout and never blocks the GUI.
        let direct_ip = if is_test_dc { dc::test_ipv4(dc_id) } else { dc::default_ipv4(dc_id) };
        if let Some(ip) = direct_ip {
            if let Ok(Ok(mut upstream_socket)) = timeout(
                Duration::from_millis(2200),
                TcpStream::connect(SocketAddr::new(ip, 443)),
            ).await {
                let relay = obfs2::new_client(client_obfs.parsed.protocol, relay_dc)?;
                upstream_socket.set_nodelay(true)?;
                upstream_socket.write_all(&relay.wire_header).await?;
                self.stats.tcp_fallbacks.fetch_add(1, Ordering::Relaxed);
                self.stats.current_transport.store(2, Ordering::Relaxed);
                warn!(dc = dc_id, "WSS unavailable; direct TCP fallback connected");
                return self.pipe_tcp(client, upstream_socket, client_obfs, relay).await;
            }
        }

        let target = target_from_header(&initial_header, dc_id);
        anyhow::bail!(
            "no usable WSS/Cloudflare endpoint for DC{}; direct TCP fallback to {} also failed",
            dc_id,
            target
        )
    }

    async fn connect(
        &self,
        url: &str,
        _dc_id: u16,
        target_ip: Option<std::net::IpAddr>,
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

        // Official Telegram endpoints use a DC redirect IP with their normal
        // hostname/SNI. Cloudflare fallback endpoints resolve via DNS and keep
        // TLS certificate verification enabled for their own hostnames.
        let mut addresses: Vec<SocketAddr> = if let Some(ip) = target_ip {
            vec![SocketAddr::new(ip, 443)]
        } else {
            let host = url_host(&req);
            let resolved = lookup_host((host.as_str(), 443)).await?;
            let mut out = Vec::new();
            for address in resolved {
                out.push(address);
            }
            out
        };

        let mut last_error = None;
        while let Some(addr) = addresses.pop() {
            match TcpStream::connect(addr).await {
                Ok(socket) => {
                    socket.set_nodelay(true)?;
                    match client_async_tls_with_config(req.clone(), socket, None, None).await {
                        Ok(connected) => return Ok(connected),
                        Err(error) => last_error = Some(error),
                    }
                }
                Err(error) => {
                    last_error = Some(tokio_tungstenite::tungstenite::Error::Io(error));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, "endpoint did not resolve to addresses")
        )))
    }

    fn endpoints(&self, dc_id: u16, media: bool, is_test_dc: bool) -> Vec<String> {
        let mut endpoints: Vec<String> = self.config.websocket.templates.iter()
            .filter(|template| !template.contains("{dc_name}"))
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

        if is_test_dc {
            endpoints = endpoints.into_iter().map(|endpoint| {
                if endpoint.contains("/apiws_test") {
                    endpoint
                } else {
                    endpoint.replacen("/apiws", "/apiws_test", 1)
                }
            }).collect();
        }

        if media {
            endpoints.sort_by_key(|u| if u.contains("-1.web.telegram.org") { 0 } else { 1 });
        } else {
            endpoints.retain(|u| !u.contains("-1.web.telegram.org"));
        }

        // Flowseal worker fallback connects to the worker domain itself and
        // sends dst/dc query parameters. A synthetic kwsN.<worker-domain> host
        // is not a valid worker route.
        let fallback_dst = dc::websocket_target_ipv4_for(dc_id, is_test_dc)
            .map(|ip| ip.to_string());
        if let Some(dst) = fallback_dst {
            let path = if is_test_dc { "/apiws_test" } else { "/apiws" };
            for domain in &self.config.websocket.fallback_domains {
                let domain = domain.trim();
                if domain.is_empty()
                    || domain.contains('/')
                    || domain.contains(':')
                    || domain.chars().any(char::is_whitespace)
                {
                    continue;
                }
                let endpoint = format!("wss://{domain}{path}?dst={dst}&dc={dc_id}");
                if !endpoints.contains(&endpoint) {
                    endpoints.push(endpoint);
                }
            }
        }

        endpoints
    }

    async fn pipe_tcp(
        &self,
        client: &mut TcpStream,
        upstream_stream: TcpStream,
        mut client_obfs: ServerObfs,
        mut upstream: ClientObfs,
    ) -> anyhow::Result<()> {
        let (mut client_r, mut client_w) = client.split();
        let (mut upstream_r, mut upstream_w) = upstream_stream.into_split();
        let stats_up = self.stats.clone();
        let stats_down = self.stats.clone();

        let up = async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = client_r.read(&mut buf).await?;
                if n == 0 { break; }
                let mut data = buf[..n].to_vec();
                client_obfs.decrypt.apply_keystream(&mut data);
                upstream.encrypt.apply_keystream(&mut data);
                upstream_w.write_all(&data).await?;
                stats_up.bytes_up.fetch_add(n as u64, Ordering::Relaxed);
                stats_up.packets_up.fetch_add(1, Ordering::Relaxed);
            }
            Ok::<(), anyhow::Error>(())
        };

        let down = async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = upstream_r.read(&mut buf).await?;
                if n == 0 { break; }
                let mut data = buf[..n].to_vec();
                upstream.decrypt.apply_keystream(&mut data);
                client_obfs.encrypt.apply_keystream(&mut data);
                client_w.write_all(&data).await?;
                stats_down.bytes_down.fetch_add(n as u64, Ordering::Relaxed);
                stats_down.packets_down.fetch_add(1, Ordering::Relaxed);
            }
            Ok::<(), anyhow::Error>(())
        };

        tokio::select! {
            result = up => result?,
            result = down => result?,
        }
        Ok(())
    }

    async fn pipe(
        &self,
        client: &mut TcpStream,
        mut client_obfs: ServerObfs,
        ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        mut upstream: ClientObfs,
    ) -> anyhow::Result<()> {
        let protocol = client_obfs.parsed.protocol;
        let (mut client_r, mut client_w) = client.split();
        let (mut ws_w, mut ws_r) = ws.split();
        let stats_up = self.stats.clone();
        let stats_down = self.stats.clone();

        let up = async move {
            let mut splitter = MtprotoPacketSplitter::new(protocol);
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = client_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }

                let mut plain = buf[..n].to_vec();
                client_obfs.decrypt.apply_keystream(&mut plain);
                let mut encrypted = plain.clone();
                upstream.encrypt.apply_keystream(&mut encrypted);

                stats_up.bytes_up.fetch_add(n as u64, Ordering::Relaxed);
                stats_up.packets_up.fetch_add(1, Ordering::Relaxed);

                for frame in splitter.push(&encrypted, &plain) {
                    ws_w.send(Message::Binary(frame.into())).await?;
                }
            }

            for frame in splitter.flush() {
                ws_w.send(Message::Binary(frame.into())).await?;
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

struct MtprotoPacketSplitter {
    protocol: [u8; 4],
    encrypted: Vec<u8>,
    plain: Vec<u8>,
    disabled: bool,
}

impl MtprotoPacketSplitter {
    fn new(protocol: [u8; 4]) -> Self {
        Self { protocol, encrypted: Vec::new(), plain: Vec::new(), disabled: false }
    }

    fn push(&mut self, encrypted: &[u8], plain: &[u8]) -> Vec<Vec<u8>> {
        if self.disabled {
            return vec![encrypted.to_vec()];
        }
        if encrypted.len() != plain.len() {
            self.disabled = true;
            return vec![encrypted.to_vec()];
        }

        self.encrypted.extend_from_slice(encrypted);
        self.plain.extend_from_slice(plain);
        let mut frames = Vec::new();

        loop {
            let available = self.plain.len();
            if available == 0 { break; }

            let packet_len = if self.protocol == *b"\xef\xef\xef\xef" {
                let first = self.plain[0];
                if first == 0x7f || first == 0xff {
                    if available < 4 { break; }
                    let words = self.plain[1] as usize
                        | ((self.plain[2] as usize) << 8)
                        | ((self.plain[3] as usize) << 16);
                    if words == 0 { None } else { Some(4usize.saturating_add(words.saturating_mul(4))) }
                } else {
                    let words = (first & 0x7f) as usize;
                    if words == 0 { None } else { Some(1usize.saturating_add(words.saturating_mul(4))) }
                }
            } else {
                if available < 4 { break; }
                let raw = u32::from_le_bytes([self.plain[0], self.plain[1], self.plain[2], self.plain[3]]) & 0x7fff_ffff;
                if raw == 0 { None } else { Some(4usize.saturating_add(raw as usize)) }
            };

            let Some(packet_len) = packet_len else {
                self.disabled = true;
                if !self.encrypted.is_empty() {
                    frames.push(std::mem::take(&mut self.encrypted));
                }
                self.plain.clear();
                return frames;
            };

            if packet_len > self.plain.len() { break; }
            frames.push(self.encrypted.drain(..packet_len).collect());
            self.plain.drain(..packet_len);
        }
        frames
    }

    fn flush(&mut self) -> Vec<Vec<u8>> {
        self.plain.clear();
        if self.encrypted.is_empty() { Vec::new() } else { vec![std::mem::take(&mut self.encrypted)] }
    }
}

fn url_host(req: &tokio_tungstenite::tungstenite::handshake::client::Request) -> String {
    req.uri().host().unwrap_or_default().to_owned()
}

fn target_ip_for_url(url: &str, dc_id: u16, is_test_dc: bool) -> Option<std::net::IpAddr> {
    let host = url.strip_prefix("wss://")?.split('/').next()?;
    if host.ends_with(".web.telegram.org") {
        dc::websocket_target_ipv4_for(dc_id, is_test_dc)
    } else {
        None
    }
}

fn target_from_header(_header: &[u8; 64], dc_id: u16) -> String {
    dc::websocket_target_ipv4(dc_id)
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

pub async fn probe(config: &AppConfig) -> bool {
    probe_route(config, false).await
}

pub async fn probe_media(config: &AppConfig) -> bool {
    probe_route(config, true).await
}

async fn probe_route(config: &AppConfig, media: bool) -> bool {
    let transport = WebSocketTransport::new(config.clone(), Arc::new(Statistics::default()));
    let mut remaining = transport.endpoints(2, media, false).into_iter().take(12);
    let mut attempts = futures_util::stream::FuturesUnordered::new();

    for _ in 0..4 {
        if let Some(url) = remaining.next() {
            let target = target_ip_for_url(&url, 2, false);
            let attempt_url = url.clone();
            let transport_ref = &transport;
            attempts.push(async move {
                let result = timeout(
                    Duration::from_millis(3000),
                    transport_ref.connect(&attempt_url, 2, target),
                ).await;
                (url, result)
            }.boxed());
        }
    }

    while let Some((_url, result)) = attempts.next().await {
        if matches!(result, Ok(Ok(_))) {
            return true;
        }
        if let Some(url) = remaining.next() {
            let target = target_ip_for_url(&url, 2, false);
            let attempt_url = url.clone();
            let transport_ref = &transport;
            attempts.push(async move {
                let result = timeout(
                    Duration::from_millis(3000),
                    transport_ref.connect(&attempt_url, 2, target),
                ).await;
                (url, result)
            }.boxed());
        }
    }
    false
}

#[cfg(test)]
mod packet_splitter_tests {
    use super::MtprotoPacketSplitter;

    #[test]
    fn intermediate_packet_can_arrive_in_multiple_tcp_chunks() {
        let protocol = *b"\xee\xee\xee\xee";
        let mut splitter = MtprotoPacketSplitter::new(protocol);
        let plain = [4u8, 0, 0, 0, 10, 11, 12, 13];
        let encrypted = plain;
        assert!(splitter.push(&encrypted[..3], &plain[..3]).is_empty());
        let frames = splitter.push(&encrypted[3..], &plain[3..]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_slice(), encrypted.as_slice());
        assert!(splitter.flush().is_empty());
    }

    #[test]
    fn coalesced_intermediate_packets_become_separate_frames() {
        let protocol = *b"\xee\xee\xee\xee";
        let mut splitter = MtprotoPacketSplitter::new(protocol);
        let plain = [2u8, 0, 0, 0, 1, 2, 1, 0, 0, 0, 3];
        let encrypted = plain;
        let frames = splitter.push(&encrypted, &plain);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].len(), 6);
        assert_eq!(frames[1].len(), 5);
    }
}

