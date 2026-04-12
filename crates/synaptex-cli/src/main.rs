mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};

use commands::config::ConfigCmd;
use commands::device::DeviceCmd;
use commands::room::RoomCmd;
use commands::routine::RoutineCmd;
use commands::hub::HubCmd;
use commands::router::RouterCmd;

// ─── CLI definition ──────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name    = "synaptex-cli",
    version,
    about   = "CLI client for the synaptex-core REST API",
    long_about = None,
)]
struct Cli {
    /// Base URL for the synaptex-core HTTP REST API.
    #[arg(long, default_value = "http://localhost:8080", env = "SYNAPTEX_HTTP_URL")]
    http_url: String,

    /// Bearer token for the REST API (omit in open/dev mode).
    #[arg(long, env = "SYNAPTEX_API_KEY")]
    api_key: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Manage daemon configuration (REST API).
    #[command(subcommand)]
    Config(ConfigCmd),

    /// Manage devices.
    #[command(subcommand)]
    Device(DeviceCmd),

    /// Manage rooms.
    #[command(subcommand)]
    Room(RoomCmd),

    /// Manage routines.
    #[command(subcommand)]
    Routine(RoutineCmd),

    /// Register and manage hubs (Bond, Matter, etc.).
    #[command(subcommand)]
    Hub(HubCmd),

    /// Inspect the router's device and discovery state.
    #[command(subcommand)]
    Router(RouterCmd),
}

// ─── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let url = cli.http_url.as_str();
    let key = cli.api_key.as_deref();

    match cli.command {
        Commands::Config(cmd)  => commands::config::run(cmd, url, key).await,
        Commands::Device(cmd)  => commands::device::run(cmd, url, key).await,
        Commands::Room(cmd)    => commands::room::run(cmd, url, key).await,
        Commands::Routine(cmd) => commands::routine::run(cmd, url, key).await,
        Commands::Hub(cmd)     => commands::hub::run(cmd, url, key).await,
        Commands::Router(cmd)  => commands::router::run(cmd, url, key).await,
    }
}
