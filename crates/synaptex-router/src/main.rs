mod db;
mod dhcp;
mod discovery;
mod firewall;
mod kea;
mod rpc;
mod tls;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::broadcast;
use tonic::transport::{Identity, Server, ServerTlsConfig, Certificate};
use tracing::info;

use synaptex_router_proto::router_service_server::RouterServiceServer;

// ─── CLI args ─────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name    = "synaptex-router",
    version,
    about   = "Synaptex router daemon — firewall, DHCP, and device discovery",
)]
struct Args {
    /// Address to listen on for the gRPC service.
    #[arg(long, default_value = "[::]:50052", env = "SYNAPTEX_ROUTER_LISTEN")]
    listen: SocketAddr,

    /// Path to this router's TLS certificate PEM (auto-generated on first run).
    #[arg(long, default_value = "./router.crt", env = "SYNAPTEX_ROUTER_CERT")]
    cert: PathBuf,

    /// Path to this router's TLS private key PEM (auto-generated on first run).
    #[arg(long, default_value = "./router.key", env = "SYNAPTEX_ROUTER_KEY")]
    key: PathBuf,

    /// Path to synaptex-core's TLS certificate PEM (required for mTLS).
    /// Copy core's certificate here after generating it on the core host.
    #[arg(long, env = "SYNAPTEX_ROUTER_CLIENT_CA")]
    client_ca: Option<PathBuf>,

    /// Path for the sled database directory.
    #[arg(long, default_value = "./router-db", env = "SYNAPTEX_ROUTER_DB")]
    db: std::path::PathBuf,

    /// Network interface(s) to listen on for Tuya UDP broadcasts.
    /// Comma-separated, e.g. "br-iot,br-lan".  Omit to listen on all interfaces.
    #[arg(long, env = "SYNAPTEX_ROUTER_INTERFACES")]
    interfaces: Option<String>,

    /// Unix domain socket path for the Kea hook shim.
    /// Omit to disable the Kea classifier.
    #[arg(long, env = "SYNAPTEX_ROUTER_KEA_SOCKET")]
    kea_socket: Option<std::path::PathBuf>,

    /// Relay agent IP(s) for the IoT VLAN, comma-separated.
    /// Only DHCP requests arriving via these giaddrs are classified.
    /// e.g. "10.10.20.1" or "10.10.20.1,10.10.21.1"
    #[arg(long, env = "SYNAPTEX_ROUTER_KEA_IOT_RELAY", value_delimiter = ',')]
    kea_iot_relay: Vec<std::net::Ipv4Addr>,
}

// ─── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "synaptex_router=info".into()),
        )
        .init();

    info!("synaptex-router starting");

    // ── TLS identity ─────────────────────────────────────────────────────────
    let (cert_pem, key_pem) = tls::load_or_generate(&args.cert, &args.key)
        .context("load/generate TLS certificate")?;

    let identity = Identity::from_pem(&cert_pem, &key_pem);
    let mut tls_config = ServerTlsConfig::new().identity(identity);

    if let Some(ca_path) = &args.client_ca {
        let ca_pem = std::fs::read(ca_path)
            .with_context(|| format!("read client CA cert from {}", ca_path.display()))?;
        tls_config = tls_config.client_ca_root(Certificate::from_pem(ca_pem));
        info!("mTLS enabled — client certificate required");
    } else {
        info!("mTLS disabled — set --client-ca to require client certificates");
    }

    // ── Persistent device database ───────────────────────────────────────────
    std::fs::create_dir_all(&args.db).context("create router db directory")?;
    let sled_db = sled::open(&args.db).context("open router sled database")?;
    let router_db = Arc::new(db::RouterDb::open(&sled_db).context("open router db trees")?);

    // ── Discovery broadcast channel ───────────────────────────────────────────
    // The discovery listener sends DiscoveredDevice events on this channel.
    // Each connected WatchDiscovery RPC stream subscribes to receive them.
    let (discovery_tx, _) = broadcast::channel::<synaptex_router_proto::DiscoveredDevice>(64);
    let discovery_tx = Arc::new(discovery_tx);

    // ── Kea hook domain socket ───────────────────────────────────────────────
    if let Some(socket_path) = args.kea_socket {
        if args.kea_iot_relay.is_empty() {
            anyhow::bail!("--kea-iot-relay must be set when --kea-socket is configured");
        }
        info!(
            path   = %socket_path.display(),
            relays = ?args.kea_iot_relay,
            "kea: starting hook listener",
        );
        kea::spawn(socket_path, args.kea_iot_relay);
    }

    // ── Spawn UDP discovery listener ─────────────────────────────────────────
    let interfaces = args.interfaces
        .as_deref()
        .map(|s| s.split(',').map(str::trim).map(str::to_string).collect::<Vec<_>>());
    discovery::spawn(discovery_tx.clone(), router_db.clone(), interfaces);

    // ── gRPC service ──────────────────────────────────────────────────────────
    let service = rpc::RouterServiceImpl { discovery_tx, db: router_db };

    info!(listen = %args.listen, "gRPC server listening");

    Server::builder()
        .tls_config(tls_config)
        .context("configure TLS")?
        .add_service(RouterServiceServer::new(service))
        .serve(args.listen)
        .await
        .context("gRPC server error")?;

    Ok(())
}
