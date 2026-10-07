use crate::{config::RelayConfig, statistics::Statistics, udp::{decode_relay_datagram, encode_relay_datagram}};
use anyhow::Context;
use bytes::Bytes;
use quinn::{ClientConfig, Endpoint, ServerConfig};
use rustls::{pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer}, RootCertStore};
use std::{collections::HashMap, net::SocketAddr, sync::{atomic::Ordering, Arc}};
use tokio::{sync::Mutex, net::UdpSocket};
use tracing::warn;

#[derive(Clone)]
pub struct CallRelayClient { pub connection: quinn::Connection, _endpoint: Arc<Endpoint> }
impl CallRelayClient {
    pub async fn connect(config: &RelayConfig) -> anyhow::Result<Self> {
        let endpoint_text = config.endpoint.as_deref().context("relay endpoint not configured")?;
        let addr_text = endpoint_text.strip_prefix("quic://").unwrap_or(endpoint_text);
        let addr: SocketAddr = tokio::net::lookup_host(addr_text).await?.next().context("relay DNS returned no addresses")?;
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        let quic_tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
        endpoint.set_default_client_config(ClientConfig::new(Arc::new(quic_tls)));
        let server_name = config.server_name.clone().unwrap_or_else(|| addr_text.split(':').next().unwrap_or(addr_text).to_string());
        let connection = endpoint.connect(addr, &server_name)?.await?;
        if let Some(token) = &config.auth_token {
            let mut stream = connection.open_uni().await?;
            stream.write_all(token.as_bytes()).await?;
            stream.finish()?;
        }
        Ok(Self { connection, _endpoint: Arc::new(endpoint) })
    }
    pub fn send(&self, flow_id: u64, dst: SocketAddr, payload: &[u8]) -> anyhow::Result<()> {
        let data = encode_relay_datagram(flow_id, dst, payload).context("relay currently supports IPv4 destinations only")?;
        self.connection.send_datagram(data)?;
        Ok(())
    }
    pub async fn recv(&self) -> anyhow::Result<Bytes> { Ok(self.connection.read_datagram().await?) }
}

struct RelayFlow { socket: Arc<UdpSocket> }

pub async fn run_server(bind: SocketAddr, cert_der: CertificateDer<'static>, key: PrivateKeyDer<'static>, token: Option<String>, stats: Arc<Statistics>) -> anyhow::Result<()> {
    let mut server_config = ServerConfig::with_single_cert(vec![cert_der], key)?;
    server_config.transport_config(Arc::new(quinn::TransportConfig::default()));
    let endpoint = Endpoint::server(server_config, bind)?;
    tracing::info!(%bind, "call relay listening");
    while let Some(incoming) = endpoint.accept().await {
        let stats = stats.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let connection = match incoming.await { Ok(c) => c, Err(e) => { warn!(error = %e, "relay handshake failed"); return; } };
            if let Some(expected) = token {
                let mut stream = match connection.accept_uni().await { Ok(s) => s, Err(_) => return };
                let buf = match stream.read_to_end(4096).await {
                    Ok(buf) => buf,
                    Err(_) => return,
                };
                if buf != expected.as_bytes() { return; }
            }
            let flows: Arc<Mutex<HashMap<u64, RelayFlow>>> = Arc::new(Mutex::new(HashMap::new()));
            loop {
                match connection.read_datagram().await {
                    Ok(data) => {
                        let Some((flow_id, dst, payload)) = decode_relay_datagram(&data) else { continue; };
                        let socket = {
                            let mut map = flows.lock().await;
                            if let Some(flow) = map.get(&flow_id) { flow.socket.clone() }
                            else {
                                let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else { continue; };
                                let socket = Arc::new(socket);
                                let response_conn = connection.clone();
                                let response_socket = socket.clone();
                                tokio::spawn(async move {
                                    let mut buf = vec![0u8; 2048];
                                    loop {
                                        let Ok((n, remote)) = response_socket.recv_from(&mut buf).await else { break; };
                                        if let SocketAddr::V4(v4) = remote {
                                            let mut out = Vec::with_capacity(21 + n);
                                            out.extend_from_slice(crate::udp::RELAY_MAGIC);
                                            out.extend_from_slice(&flow_id.to_be_bytes());
                                            out.push(4);
                                            out.extend_from_slice(&v4.ip().octets());
                                            out.extend_from_slice(&v4.port().to_be_bytes());
                                            out.extend_from_slice(&buf[..n]);
                                            let _ = response_conn.send_datagram(out.into());
                                        }
                                    }
                                });
                                map.insert(flow_id, RelayFlow { socket: socket.clone() });
                                socket
                            }
                        };
                        if socket.send_to(payload, dst).await.is_ok() { stats.udp_sessions.fetch_add(1, Ordering::Relaxed); }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    Ok(())
}

pub fn build_dev_certificate() -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()));
    Ok((cert, key))
}

pub fn relay_ready(config: &RelayConfig) -> bool { config.enabled && config.endpoint.is_some() }
