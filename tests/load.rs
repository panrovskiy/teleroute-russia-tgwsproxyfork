use std::{sync::Arc, time::Duration};
use tele_route::{config::AppConfig, proxy::socks5::Socks5Server, statistics::Statistics};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, time::timeout};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn socks5_handles_120_simultaneous_connections() -> anyhow::Result<()> {
    let echo = TcpListener::bind("127.0.0.1:0").await?;
    let echo_addr = echo.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = echo.accept().await else { break; };
            tokio::spawn(async move {
                let mut buf = [0u8; 128];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 { break; }
                    if s.write_all(&buf[..n]).await.is_err() { break; }
                }
            });
        }
    });

    let mut cfg = AppConfig::default();
    cfg.proxy.port = 0;
    cfg.routing.prefer_wss = false;
    let stats = Arc::new(Statistics::default());
    let server = Socks5Server::new(cfg, stats);
    let listener = server.bind().await?;
    let proxy_addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();

    // Move the server into the spawned future so the borrow created by
    // run_on_listener(&self, ...) is owned by that future instead of the test stack frame.
    let server_shutdown = shutdown.clone();
    let server_task = tokio::spawn(async move {
        server.run_on_listener(listener, server_shutdown).await
    });

    let mut clients = Vec::new();
    for i in 0..120u16 {
        let addr = proxy_addr;
        let target = echo_addr;
        clients.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await?;
            s.write_all(&[5, 1, 0]).await?;
            let mut r = [0u8; 2];
            s.read_exact(&mut r).await?;
            if r != [5, 0] { anyhow::bail!("bad SOCKS greeting: {:?}", r); }
            let ip = match target.ip() {
                std::net::IpAddr::V4(v) => v.octets(),
                _ => unreachable!(),
            };
            let mut req = vec![5, 1, 0, 1];
            req.extend_from_slice(&ip);
            req.extend_from_slice(&target.port().to_be_bytes());
            s.write_all(&req).await?;
            let mut resp = [0u8; 10];
            s.read_exact(&mut resp).await?;
            if resp[1] != 0 { anyhow::bail!("SOCKS connect failed: {}", resp[1]); }
            let msg = format!("load-{i}");
            s.write_all(msg.as_bytes()).await?;
            let mut got = vec![0u8; msg.len()];
            s.read_exact(&mut got).await?;
            if got != msg.as_bytes() { anyhow::bail!("echo mismatch"); }
            Ok::<(), anyhow::Error>(())
        }));
    }

    for client in clients {
        timeout(Duration::from_secs(10), client).await??;
    }

    shutdown.cancel();
    let _ = timeout(Duration::from_secs(2), server_task).await;
    Ok(())
}
