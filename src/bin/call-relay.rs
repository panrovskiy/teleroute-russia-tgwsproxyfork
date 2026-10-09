use std::{env, fs, net::SocketAddr, sync::Arc};
use tele_route::{calls::run_relay_server, statistics::Statistics};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Use the same TLS provider as the main TeleRoute binary.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut bind: SocketAddr = "0.0.0.0:4433".parse()?;
    let mut cert_path: Option<String> = None;
    let mut key_path: Option<String> = None;
    let mut token: Option<String> = None;
    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => { i += 1; bind = args.get(i).ok_or_else(|| anyhow::anyhow!("missing --bind value"))?.parse()?; }
            "--cert-der" => { i += 1; cert_path = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("missing --cert-der value"))?); }
            "--key-der" => { i += 1; key_path = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("missing --key-der value"))?); }
            "--token" => { i += 1; token = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("missing --token value"))?); }
            "--help" => { println!("call-relay --bind 0.0.0.0:4433 --cert-der cert.der --key-der key.der [--token secret]"); return Ok(()); }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }
    tele_route::logging::init(&tele_route::config::AppConfig::default())?;
    let (cert, key) = match (cert_path, key_path) {
        (Some(cert), Some(key)) => (
            rustls::pki_types::CertificateDer::from(fs::read(cert)?),
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(fs::read(key)?)),
        ),
        _ => { eprintln!("A trusted certificate/key pair is required. Use --cert-der and --key-der."); std::process::exit(2); }
    };
    run_relay_server(bind, cert, key, token, Arc::new(Statistics::default())).await
}
