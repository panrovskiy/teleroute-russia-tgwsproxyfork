#[cfg(not(windows))]
pub struct TunManager;

#[cfg(not(windows))]
impl TunManager {
    pub async fn start(_: &crate::config::AppConfig, _: std::sync::Arc<crate::statistics::Statistics>) -> anyhow::Result<Self> { anyhow::bail!("Wintun is Windows-only") }
    pub async fn run(self, _: tokio_util::sync::CancellationToken) -> anyhow::Result<()> { Ok(()) }
}

#[cfg(windows)]
mod windows_tun {
    use crate::{calls::CallRelayClient, config::AppConfig, statistics::Statistics, udp::{self, FlowKey}};
    use ipnet::IpNet;
    use std::{collections::HashMap, net::{IpAddr, Ipv4Addr, SocketAddr}, sync::{atomic::{AtomicU64, Ordering}, Arc}, time::Duration};
    use tokio::{sync::Mutex, time::{interval, timeout}};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, info, warn};
    use wintun_bindings::{Adapter, AsyncSession, MAX_RING_CAPACITY};

    pub struct TunManager {
        reader: AsyncSession,
        writer: AsyncSession,
        adapter: Option<Arc<Adapter>>,
        config: AppConfig,
        stats: Arc<Statistics>,
        flows: Arc<Mutex<HashMap<FlowKey, Arc<udp::DirectUdpFlow>>>>,
        relay: Option<Arc<CallRelayClient>>,
        relay_routes: Arc<Mutex<HashMap<u64, SocketAddr>>>,
        next_flow_id: AtomicU64,
        routed_prefixes: Vec<Ipv4Net>,
        adapter_index: u32,
    }

    #[derive(Clone, Copy)]
    struct Ipv4Net { network: u32, mask: u32 }
    impl Ipv4Net {
        fn parse(text: &str) -> anyhow::Result<Self> {
            let net: IpNet = text.parse()?;
            let IpNet::V4(v4) = net else { anyhow::bail!("only IPv4 TUN routes are supported: {text}"); };
            Ok(Self { network: u32::from(v4.network()), mask: u32::from(v4.netmask()) })
        }
        fn contains(self, ip: Ipv4Addr) -> bool { u32::from(ip) & self.mask == self.network }
    }

    impl TunManager {
        pub async fn start(config: &AppConfig, stats: Arc<Statistics>) -> anyhow::Result<Self> {
            if config.routing.route_all_traffic { anyhow::bail!("full Internet routing is intentionally disabled: TeleRoute currently provides selective Telegram UDP routing without a TCP userspace stack"); }
            if !crate::platform::windows::is_elevated() {
                anyhow::bail!("Administrator privileges are required to create the Wintun adapter");
            }
            let dll = crate::platform::windows::ensure_wintun_dll()?;
            let wintun = unsafe { wintun_bindings::load_from_path(&dll) }?;
            let adapter = match Adapter::open(&wintun, &config.tun.adapter_name) {
                Ok(a) => a,
                Err(_) => Adapter::create(&wintun, &config.tun.adapter_name, "TeleRoute", None)?,
            };
            adapter.set_mtu(config.tun.mtu)?;
            let address = match config.tun.address { IpAddr::V4(v) => v, _ => anyhow::bail!("TUN address must be IPv4") };
            adapter.set_network_addresses_tuple(address.into(), Ipv4Addr::new(255,255,255,0).into(), None)?;
            let adapter_index = adapter.get_adapter_index()?;
            let session = adapter.start_session(MAX_RING_CAPACITY)?;
            let reader = AsyncSession::from(session.clone());
            let writer = AsyncSession::from(session);
            let routed_prefixes = config.tun.telegram_udp_cidrs.iter().chain(config.tun.extra_udp_cidrs.iter()).map(|s| Ipv4Net::parse(s)).collect::<anyhow::Result<Vec<_>>>()?;
            for route in &config.tun.telegram_udp_cidrs { add_route(route, adapter_index, config.tun.route_metric)?; }
            for route in &config.tun.extra_udp_cidrs { add_route(route, adapter_index, config.tun.route_metric)?; }

            let relay = if config.relay.enabled && config.relay.endpoint.is_some() && config.routing.relay_udp_fallback {
                Some(Arc::new(CallRelayClient::connect(&config.relay).await?))
            } else { None };
            if relay.is_some() { info!("QUIC call relay connected"); }
            let this = Self {
                reader, writer, adapter: Some(adapter), config: config.clone(), stats,
                flows: Arc::new(Mutex::new(HashMap::new())), relay: relay.clone(), relay_routes: Arc::new(Mutex::new(HashMap::new())), next_flow_id: AtomicU64::new(1), routed_prefixes, adapter_index,
            };
            if let Some(relay) = relay { this.spawn_relay_reader(relay); }
            info!(adapter = %this.config.tun.adapter_name, index = adapter_index, "TUN initialized");
            Ok(this)
        }

        fn spawn_relay_reader(&self, relay: Arc<CallRelayClient>) {
            let session = self.writer.clone();
            let routes = self.relay_routes.clone();
            let stats = self.stats.clone();
            let tun_addr = match self.config.tun.address { IpAddr::V4(v) => v, _ => Ipv4Addr::new(10,250,0,1) };
            tokio::spawn(async move {
                loop {
                    let Ok(data) = relay.recv().await else { break; };
                    let Some((flow_id, remote, payload)) = udp::decode_relay_datagram(&data) else { continue; };
                    let Some(local) = routes.lock().await.get(&flow_id).copied() else { continue; };
                    let SocketAddr::V4(remote4) = remote else { continue; };
                    let SocketAddr::V4(local4) = local else { continue; };
                    let packet = build_ipv4_udp_packet(*remote4.ip(), tun_addr, remote4.port(), local4.port(), payload);
                    if let Err(e) = send_to_tun(&session, &packet).await { warn!(error = %e, "failed to inject relay UDP packet into TUN"); break; }
                    udp::record_call_down(&stats, payload.len());
                }
            });
        }

        pub async fn run(mut self, shutdown: CancellationToken) -> anyhow::Result<()> {
            let mut cleanup_tick = interval(Duration::from_secs(10));
            let mut packet_buf = vec![0u8; wintun_bindings::MAX_IP_PACKET_SIZE as usize];
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = cleanup_tick.tick() => self.cleanup_idle_flows().await,
                    result = self.reader.recv(&mut packet_buf) => {
                        match result {
                            Ok(n) => self.handle_packet(&packet_buf[..n]).await?,
                            Err(_) => break,
                        }
                    }
                }
            }
            self.cleanup().await;
            let _ = self.reader.shutdown();
            let _ = self.writer.shutdown();
            // Adapter is reference-counted by wintun-bindings; dropping the final Arc
            // releases the adapter resources. Do not call delete() on Arc<Adapter>.
            drop(self.adapter.take());
            Ok(())
        }

        async fn handle_packet(&self, bytes: &[u8]) -> anyhow::Result<()> {
            if bytes.len() < 28 || bytes[0] >> 4 != 4 || bytes[9] != 17 { return Ok(()); }
            let ihl = (bytes[0] & 0x0f) as usize * 4;
            if ihl < 20 || bytes.len() < ihl + 8 { return Ok(()); }
            let src = Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]);
            let dst = Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]);
            if !self.routed_prefixes.iter().any(|r| r.contains(dst)) { return Ok(()); }
            let src_port = u16::from_be_bytes([bytes[ihl], bytes[ihl+1]]);
            let dst_port = u16::from_be_bytes([bytes[ihl+2], bytes[ihl+3]]);
            let payload = &bytes[ihl+8..];
            let source = SocketAddr::new(IpAddr::V4(src), src_port);
            let destination = SocketAddr::new(IpAddr::V4(dst), dst_port);
            let key = FlowKey { src: source, dst: destination };

            if let Some(relay) = &self.relay {
                let flow_id = {
                    let mut routes = self.relay_routes.lock().await;
                    if let Some((id, _)) = routes.iter().find(|(_, value)| **value == source) { *id }
                    else { let id = self.next_flow_id.fetch_add(1, Ordering::Relaxed); routes.insert(id, source); id }
                };
                match relay.send(flow_id, destination, payload) {
                    Ok(()) => { udp::record_call_up(&self.stats, payload.len()); return Ok(()); }
                    Err(e) => warn!(error = %e, "QUIC relay send failed; attempting direct UDP"),
                }
            }

            let flow = {
                let mut map = self.flows.lock().await;
                if let Some(flow) = map.get(&key) { flow.clone() }
                else {
                    let flow = Arc::new(udp::DirectUdpFlow::new(source, destination).await?);
                    map.insert(key, flow.clone());
                    self.spawn_direct_reader(flow.clone(), key);
                    self.stats.udp_sessions.fetch_add(1, Ordering::Relaxed);
                    flow
                }
            };
            match timeout(Duration::from_millis(1500), flow.socket.send(payload)).await {
                Ok(Ok(n)) => udp::record_call_up(&self.stats, n),
                Ok(Err(_e)) => { self.stats.udp_blocked.fetch_add(1, Ordering::Relaxed); self.flows.lock().await.remove(&key); }
                Err(_) => { self.stats.udp_blocked.fetch_add(1, Ordering::Relaxed); self.flows.lock().await.remove(&key); debug!(%key.src, %key.dst, "UDP send timeout"); }
            }
            Ok(())
        }

        fn spawn_direct_reader(&self, flow: Arc<udp::DirectUdpFlow>, key: FlowKey) {
            let session = self.writer.clone();
            let stats = self.stats.clone();
            let tun_addr = match self.config.tun.address { IpAddr::V4(v) => v, _ => Ipv4Addr::new(10,250,0,1) };
            let idle = Duration::from_millis(self.config.timeouts.udp_idle_ms);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65535];
                loop {
                    let Ok(result) = timeout(idle, flow.socket.recv(&mut buf)).await else { break; };
                    let Ok(n) = result else { break; };
                    let remote = match key.dst.ip() { IpAddr::V4(v) => v, IpAddr::V6(_) => break };
                    let packet = build_ipv4_udp_packet(remote, tun_addr, key.dst.port(), key.src.port(), &buf[..n]);
                    if send_to_tun(&session, &packet).await.is_err() { break; }
                    udp::record_call_down(&stats, n);
                }
            });
        }

        async fn cleanup_idle_flows(&self) {
            let idle_secs = (self.config.timeouts.udp_idle_ms / 1000).max(5);
            let mut map = self.flows.lock().await;
            map.retain(|_, flow| !flow.is_idle(idle_secs));
        }

        async fn cleanup(&self) {
            self.flows.lock().await.clear();
            self.relay_routes.lock().await.clear();
            for route in self.config.tun.telegram_udp_cidrs.iter().chain(self.config.tun.extra_udp_cidrs.iter()) { let _ = delete_route(route, self.adapter_index); }
        }
    }

    async fn send_to_tun(session: &AsyncSession, packet: &[u8]) -> anyhow::Result<()> {
        if packet.len() > u16::MAX as usize { anyhow::bail!("packet too large"); }
        session.send(packet).await?;
        Ok(())
    }

    fn build_ipv4_udp_packet(src: Ipv4Addr, dst: Ipv4Addr, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let total_len = 20 + 8 + payload.len();
        let mut out = vec![0u8; total_len];
        out[0] = 0x45; out[2..4].copy_from_slice(&(total_len as u16).to_be_bytes()); out[8] = 64; out[9] = 17;
        out[12..16].copy_from_slice(&src.octets()); out[16..20].copy_from_slice(&dst.octets());
        out[20..22].copy_from_slice(&src_port.to_be_bytes()); out[22..24].copy_from_slice(&dst_port.to_be_bytes());
        out[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes()); out[28..].copy_from_slice(payload);
        let ip_checksum = checksum_ipv4(&out[..20]);
        let udp_checksum = checksum_udp_ipv4(src, dst, &out[20..]);
        out[10..12].copy_from_slice(&ip_checksum.to_be_bytes());
        out[26..28].copy_from_slice(&udp_checksum.to_be_bytes());
        out
    }
    fn checksum_ipv4(header: &[u8]) -> u16 { checksum(header, 10) }
    fn checksum_udp_ipv4(src: Ipv4Addr, dst: Ipv4Addr, udp: &[u8]) -> u16 {
        let mut pseudo = Vec::with_capacity(12 + udp.len()); pseudo.extend_from_slice(&src.octets()); pseudo.extend_from_slice(&dst.octets()); pseudo.extend_from_slice(&[0,17]); pseudo.extend_from_slice(&(udp.len() as u16).to_be_bytes()); pseudo.extend_from_slice(udp);
        let s = checksum(&pseudo, usize::MAX); if s == 0 { 0xffff } else { s }
    }
    fn checksum(data: &[u8], zero_at: usize) -> u16 {
        let mut sum = 0u32; let mut i = 0usize;
        while i + 1 < data.len() { let word = if i == zero_at { 0 } else { u16::from_be_bytes([data[i], data[i+1]]) }; sum += word as u32; i += 2; }
        if i < data.len() { sum += (data[i] as u32) << 8; }
        while sum >> 16 != 0 { sum = (sum & 0xffff) + (sum >> 16); }
        !(sum as u16)
    }

    fn add_route(prefix: &str, index: u32, metric: u32) -> anyhow::Result<()> {
        let status = std::process::Command::new("netsh").args(["interface", "ipv4", "add", "route", &format!("prefix={prefix}"), &format!("interface={index}"), &format!("metric={metric}"), "store=active"]).status()?;
        if !status.success() { anyhow::bail!("netsh failed while adding route {prefix}"); }
        Ok(())
    }
    fn delete_route(prefix: &str, index: u32) -> anyhow::Result<()> {
        let _ = std::process::Command::new("netsh").args(["interface", "ipv4", "delete", "route", &format!("prefix={prefix}"), &format!("interface={index}"), "store=active"]).status(); Ok(())
    }
}

#[cfg(windows)]
pub use windows_tun::TunManager;
