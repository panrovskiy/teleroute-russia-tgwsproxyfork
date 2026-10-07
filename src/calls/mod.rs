use crate::{config::{AppConfig, Mode}, statistics::Statistics, tun::TunManager};
use std::{net::SocketAddr, sync::Arc};
use tokio_util::sync::CancellationToken;

pub struct CallEngine { config: AppConfig, pub stats: Arc<Statistics> }
impl CallEngine {
    pub fn new(config: AppConfig, stats: Arc<Statistics>) -> Self { Self { config, stats } }
    pub async fn start(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        if !self.config.tun.enabled || matches!(self.config.routing.mode, Mode::Proxy) { return Ok(()); }
        #[cfg(windows)] { TunManager::start(&self.config, self.stats.clone()).await?.run(shutdown).await?; }
        #[cfg(not(windows))] { let _ = shutdown; tracing::warn!("Call/TUN path is Windows-only"); }
        Ok(())
    }
}

#[cfg(windows)]
mod relay;
#[cfg(windows)]
pub use relay::CallRelayClient;

#[cfg(windows)]
pub async fn run_relay_server(bind: SocketAddr, cert: rustls::pki_types::CertificateDer<'static>, key: rustls::pki_types::PrivateKeyDer<'static>, token: Option<String>, stats: Arc<Statistics>) -> anyhow::Result<()> {
    relay::run_server(bind, cert, key, token, stats).await
}

#[cfg(not(windows))]
pub async fn run_relay_server(_: SocketAddr, _: rustls::pki_types::CertificateDer<'static>, _: rustls::pki_types::PrivateKeyDer<'static>, _: Option<String>, _: Arc<Statistics>) -> anyhow::Result<()> { anyhow::bail!("call relay server is intended for Windows-independent deployment but this binary was not built with the relay module") }
