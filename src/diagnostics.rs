use crate::{config::AppConfig, statistics::Statistics};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::{lookup_host, TcpStream, UdpSocket}, time::timeout};

#[derive(Debug, Clone, Default)]
pub struct DiagnosticReport {
    pub dns: String,
    pub telegram_tcp: String,
    pub websocket: String,
    pub tcp_fallback: String,
    pub udp: String,
    pub tun: String,
    pub call_transport: String,
}

pub async fn run_full(config: &AppConfig, _stats: Arc<Statistics>, tun_active: bool) -> DiagnosticReport {
    let dns = match timeout(Duration::from_secs(3), lookup_host(("kws2.web.telegram.org", 443))).await {
        Ok(Ok(mut it)) => if it.next().is_some() { "OK" } else { "FAILED" },
        _ => "FAILED",
    };
    let tcp = match timeout(Duration::from_secs(3), TcpStream::connect(("kws2.web.telegram.org", 443))).await { Ok(Ok(_)) => "OK", _ => "FAILED" };
    let ws = if crate::websocket::probe(&config.websocket).await { "OK" } else { "FAILED" };
    let udp = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(socket) => match socket.connect(SocketAddr::from(([149, 154, 167, 50], 443))).await { Ok(()) => "SOCKET OK", Err(_) => "BLOCKED" },
        Err(_) => "BLOCKED",
    };
    let tun = if tun_active { "ACTIVE" } else { "INACTIVE" };
    let call = if tun_active && (config.relay.endpoint.is_some() || config.routing.direct_udp_fallback) { "READY (transport-level)" } else { "UNAVAILABLE" };
    DiagnosticReport { dns: dns.into(), telegram_tcp: tcp.into(), websocket: ws.into(), tcp_fallback: tcp.into(), udp: udp.into(), tun: tun.into(), call_transport: call.into() }
}
