use bytes::Bytes;
use std::{collections::HashMap, net::{IpAddr, SocketAddr, SocketAddrV4, Ipv4Addr}, sync::{Arc, atomic::{AtomicU64, Ordering}}, time::{SystemTime, UNIX_EPOCH}};
use tokio::net::UdpSocket;
use tracing::debug;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct FlowKey { pub src: SocketAddr, pub dst: SocketAddr }

pub const RELAY_MAGIC: &[u8; 6] = b"TRUDP1";

pub fn encode_relay_datagram(flow_id: u64, destination: SocketAddr, payload: &[u8]) -> Option<Bytes> {
    let SocketAddr::V4(dst) = destination else { return None; };
    let mut out = Vec::with_capacity(6 + 8 + 1 + 4 + 2 + payload.len());
    out.extend_from_slice(RELAY_MAGIC);
    out.extend_from_slice(&flow_id.to_be_bytes());
    out.push(4);
    out.extend_from_slice(&dst.ip().octets());
    out.extend_from_slice(&dst.port().to_be_bytes());
    out.extend_from_slice(payload);
    Some(Bytes::from(out))
}

pub fn decode_relay_datagram(data: &[u8]) -> Option<(u64, SocketAddr, &[u8])> {
    if data.len() < 21 || &data[0..6] != RELAY_MAGIC || data[14] != 4 { return None; }
    let flow = u64::from_be_bytes(data[6..14].try_into().ok()?);
    let ip = Ipv4Addr::new(data[15], data[16], data[17], data[18]);
    let port = u16::from_be_bytes([data[19], data[20]]);
    Some((flow, SocketAddr::V4(SocketAddrV4::new(ip, port)), &data[21..]))
}

pub struct DirectUdpFlow { pub socket: Arc<UdpSocket>, pub source: SocketAddr, pub destination: SocketAddr, pub last_used: AtomicU64 }

impl DirectUdpFlow {
    pub fn touch(&self) { self.last_used.store(now_secs(), Ordering::Relaxed); }
    pub fn is_idle(&self, idle_secs: u64) -> bool { now_secs().saturating_sub(self.last_used.load(Ordering::Relaxed)) > idle_secs }

    pub async fn new(source: SocketAddr, destination: SocketAddr) -> anyhow::Result<Self> {
        let bind = if destination.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(destination).await?;
        Ok(Self { socket: Arc::new(socket), source, destination, last_used: AtomicU64::new(now_secs()) })
    }
}

pub type FlowMap = Arc<parking_lot::Mutex<HashMap<FlowKey, Arc<DirectUdpFlow>>>>;

pub fn record_call_up(stats: &crate::statistics::Statistics, n: usize) {
    stats.call_bytes_up.fetch_add(n as u64, Ordering::Relaxed);
    stats.call_packets_up.fetch_add(1, Ordering::Relaxed);
}

pub fn record_call_down(stats: &crate::statistics::Statistics, n: usize) {
    stats.call_bytes_down.fetch_add(n as u64, Ordering::Relaxed);
    stats.call_packets_down.fetch_add(1, Ordering::Relaxed);
}

pub fn log_flow(key: FlowKey) { debug!(src = %key.src, dst = %key.dst, "UDP flow"); }

#[allow(dead_code)]
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => v.is_loopback() || v.is_unique_local() || v.is_unicast_link_local(),
    }
}

fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }
