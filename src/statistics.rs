use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

#[derive(Default)]
pub struct Statistics {
    pub connections: AtomicU64,
    pub active_connections: AtomicU64,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
    pub packets_up: AtomicU64,
    pub packets_down: AtomicU64,
    pub ping_ms: AtomicI64,
    pub reconnects: AtomicU64,
    pub ws_success: AtomicU64,
    pub ws_attempts: AtomicU64,
    pub tcp_fallbacks: AtomicU64,
    pub udp_sessions: AtomicU64,
    pub udp_blocked: AtomicU64,
    pub call_bytes_up: AtomicU64,
    pub call_bytes_down: AtomicU64,
    pub call_packets_up: AtomicU64,
    pub call_packets_down: AtomicU64,
    pub call_transport_rtt_ms: AtomicI64,
    pub current_dc: AtomicI64,
    pub current_transport: AtomicU64,
    pub call_jitter_ms: AtomicI64,
}

#[derive(Debug, Clone, Default)]
pub struct StatsSnapshot {
    pub connections: u64,
    pub active_connections: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub packets_up: u64,
    pub packets_down: u64,
    pub ping_ms: Option<i64>,
    pub reconnects: u64,
    pub ws_success_rate: f64,
    pub tcp_fallbacks: u64,
    pub udp_sessions: u64,
    pub udp_blocked: u64,
    pub call_bytes_up: u64,
    pub call_bytes_down: u64,
    pub call_packets_up: u64,
    pub call_packets_down: u64,
    pub call_transport_rtt_ms: Option<i64>,
    pub current_dc: Option<u16>,
    pub current_transport: &'static str,
    pub call_jitter_ms: Option<i64>,
}

impl Statistics {
    pub fn snapshot(&self) -> StatsSnapshot {
        let attempts = self.ws_attempts.load(Ordering::Relaxed);
        StatsSnapshot {
            connections: self.connections.load(Ordering::Relaxed),
            active_connections: self.active_connections.load(Ordering::Relaxed),
            bytes_up: self.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.bytes_down.load(Ordering::Relaxed),
            packets_up: self.packets_up.load(Ordering::Relaxed),
            packets_down: self.packets_down.load(Ordering::Relaxed),
            ping_ms: some_positive(self.ping_ms.load(Ordering::Relaxed)),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            ws_success_rate: if attempts == 0 { 0.0 } else { self.ws_success.load(Ordering::Relaxed) as f64 / attempts as f64 * 100.0 },
            tcp_fallbacks: self.tcp_fallbacks.load(Ordering::Relaxed),
            udp_sessions: self.udp_sessions.load(Ordering::Relaxed),
            udp_blocked: self.udp_blocked.load(Ordering::Relaxed),
            call_bytes_up: self.call_bytes_up.load(Ordering::Relaxed),
            call_bytes_down: self.call_bytes_down.load(Ordering::Relaxed),
            call_packets_up: self.call_packets_up.load(Ordering::Relaxed),
            call_packets_down: self.call_packets_down.load(Ordering::Relaxed),
            call_transport_rtt_ms: some_positive(self.call_transport_rtt_ms.load(Ordering::Relaxed)),
            current_dc: { let dc = self.current_dc.load(Ordering::Relaxed); (dc > 0 && dc <= 65535).then_some(dc as u16) },
            current_transport: match self.current_transport.load(Ordering::Relaxed) { 1 => "WebSocket", 2 => "TCP fallback", 3 => "Direct TCP", _ => "—" },
            call_jitter_ms: some_positive(self.call_jitter_ms.load(Ordering::Relaxed)),
        }
    }
}

fn some_positive(value: i64) -> Option<i64> { (value >= 0).then_some(value).filter(|v| *v > 0) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn counters_snapshot() {
        let s = Statistics::default();
        s.connections.fetch_add(2, Ordering::Relaxed);
        s.ws_attempts.fetch_add(4, Ordering::Relaxed);
        s.ws_success.fetch_add(3, Ordering::Relaxed);
        let snap = s.snapshot();
        assert_eq!(snap.connections, 2);
        assert!((snap.ws_success_rate - 75.0).abs() < f64::EPSILON);
    }
}
