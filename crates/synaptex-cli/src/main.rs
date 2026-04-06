mod commands;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

use synaptex_proto::device_service_client::DeviceServiceClient;

use commands::device::DeviceCmd;

// ─── CLI definition ──────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name    = "synaptex-cli",
    version,
    about   = "CLI client for the synaptex-core gRPC API",
    long_about = None,
)]
struct Cli {
    /// Path to the synaptex-core Unix domain socket.
    #[arg(long, default_value = "./synaptex.sock", env = "SYNAPTEX_SOCKET")]
    socket: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Manage devices.
    #[command(subcommand)]
    Device(DeviceCmd),
}

// ─── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Connect to the core daemon over the Unix domain socket.
    // The URI is a dummy — tonic uses the connector to establish the transport.
    let socket_path = cli.socket.clone();
    let channel = Endpoint::try_from("http://[::]:50051")
        .context("build endpoint")?
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = socket_path.clone();
            async move {
                // Wrap with TokioIo so hyper 1.x can drive the tokio stream.
                UnixStream::connect(path).await.map(TokioIo::new)
            }
        }))
        .await
        .context("connect to synaptex-core socket")?;

    let mut client = DeviceServiceClient::new(channel);

    match cli.command {
        Commands::Device(cmd) => commands::device::run(cmd, &mut client).await,
    }
}
