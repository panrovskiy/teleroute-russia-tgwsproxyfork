use crate::{config::AppConfig, statistics::Statistics};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::{lookup_host, TcpStream, UdpSocket}, time::timeout};

#[derive(Debug, Clone, Default)]
pub struct DiagnosticReport {
    pub socks5: String,
    pub mtproto: String,
    pub dns: String,
    pub telegram_tcp: String,
    pub websocket: String,
    pub tcp_fallback: String,
    pub udp: String,
    pub tun: String,
    pub call_transport: String,
}

pub async fn run_full(config: &AppConfig, _stats: Arc<Statistics>, tun_active: bool) -> DiagnosticReport {
    let socks5 = match timeout(
        Duration::from_secs(2),
        TcpStream::connect((config.proxy.bind.as_str(), config.proxy.port)),
    ).await {
        Ok(Ok(_)) => "LISTENING",
        _ => "NOT LISTENING",
    };

    let mtproto = match timeout(
        Duration::from_secs(2),
        TcpStream::connect((
            config.telegram.mtproto_bind.as_str(),
            config.telegram.mtproto_port,
        )),
    ).await {
        Ok(Ok(_)) => "LISTENING",
        _ => "NOT LISTENING",
    };

    let dns = match timeout(
        Duration::from_secs(3),
        lookup_host(("kws2.web.telegram.org", 443)),
    ).await {
        Ok(Ok(mut it)) => {
            if it.next().is_some() { "OK" } else { "FAILED" }
        }
        _ => "FAILED",
    };

    let tcp = match timeout(
        Duration::from_secs(3),
        TcpStream::connect(("kws2.web.telegram.org", 443)),
    ).await {
        Ok(Ok(_)) => "OK",
        _ => "FAILED",
    };

    let ws = if crate::websocket::probe(config).await {
        "OK"
    } else {
        "FAILED"
    };

    let udp = match UdpSocket::bind("0.0.0.0:0").await {
        Ok(socket) => match socket
            .connect(SocketAddr::from(([149, 154, 167, 50], 443)))
            .await
        {
            Ok(()) => "SOCKET OK",
            Err(_) => "BLOCKED",
        },
        Err(_) => "BLOCKED",
    };

    let tun = if tun_active { "ACTIVE" } else { "INACTIVE" };
    let call = if config.telegram.frontend == crate::config::TelegramFrontend::MtprotoWs {
        if ws == "OK" {
            "READY (MTProto media WS)"
        } else {
            "WAITING FOR WSS"
        }
    } else if tun_active {
        "READY (TUN)"
    } else {
        "UNAVAILABLE"
    };

    DiagnosticReport {
        socks5: socks5.into(),
        mtproto: mtproto.into(),
        dns: dns.into(),
        telegram_tcp: tcp.into(),
        websocket: ws.into(),
        tcp_fallback: if tcp == "OK" {
            "AVAILABLE"
        } else {
            "DIRECT BLOCKED"
        }
        .into(),
        udp: udp.into(),
        tun: tun.into(),
        call_transport: call.into(),
    }
}
