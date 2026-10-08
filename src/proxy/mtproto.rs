use crate::{
    config::AppConfig,
    statistics::Statistics,
    telegram::obfs2,
    websocket::{WebSocketPool, WebSocketTransport},
};
use std::{sync::{atomic::Ordering, Arc}};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    time::{timeout, Duration},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;

#[derive(Clone)]
pub struct MtprotoServer {
    config: AppConfig,
    stats: Arc<Statistics>,
    ws_pool: WebSocketPool,
    secret: [u8; 16],
}

impl MtprotoServer {
    pub fn new(config: AppConfig, stats: Arc<Statistics>, ws_pool: WebSocketPool) -> anyhow::Result<Self> {
        let secret = decode_secret(&config.telegram.mtproto_secret)?;
        Ok(Self { config, stats, ws_pool, secret })
    }

    pub async fn bind(&self) -> anyhow::Result<TcpListener> {
        let addr = format!(
            "{}:{}",
            self.config.telegram.mtproto_bind,
            self.config.telegram.mtproto_port
        );
        Ok(TcpListener::bind(&addr).await?)
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let listener = self.bind().await?;
        self.run_on_listener(listener, shutdown).await
    }

    pub async fn run_on_listener(&self, listener: TcpListener, shutdown: CancellationToken) -> anyhow::Result<()> {
        let addr = listener.local_addr()?;
        info!(%addr, "local Telegram MTProto proxy listening");

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => {
                    let (stream, peer) = accepted?;
                    let this = self.clone();

                    tokio::spawn(async move {
                        this.stats.connections.fetch_add(1, Ordering::Relaxed);
                        this.stats.active_connections.fetch_add(1, Ordering::Relaxed);

                        let result = AssertUnwindSafe(this.handle(stream))
                            .catch_unwind()
                            .await;

                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => debug!(%peer, error = %e, "MTProto client closed"),
                            Err(panic) => error!(%peer, ?panic, "MTProto client handler panicked"),
                        }

                        this.stats.active_connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            }
        }

        Ok(())
    }

    async fn handle(&self, mut stream: TcpStream) -> anyhow::Result<()> {
        let mut header = [0u8; 64];
        timeout(Duration::from_secs(10), stream.read_exact(&mut header)).await??;

        let client_obfs = match obfs2::parse_secret_server_header(header, &self.secret) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "invalid Telegram MTProto handshake");
                return Ok(());
            }
        };

        let transport = WebSocketTransport::with_pool(
            self.config.clone(),
            self.stats.clone(),
            self.ws_pool.clone(),
        );

        transport
            .bridge(&mut stream, header, client_obfs)
            .await
    }
}

fn decode_secret(secret: &str) -> anyhow::Result<[u8; 16]> {
    let text = secret.trim();
    if text.len() != 32 {
        anyhow::bail!("Telegram MTProto secret must contain exactly 32 hexadecimal characters");
    }

    let mut out = [0u8; 16];
    for i in 0..16 {
        let pair = &text[i * 2..i * 2 + 2];
        out[i] = u8::from_str_radix(pair, 16)
            .map_err(|_| anyhow::anyhow!("Telegram MTProto secret contains invalid hexadecimal data"))?;
    }
    Ok(out)
}
