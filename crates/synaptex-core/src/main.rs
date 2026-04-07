mod bus;
mod cache;
mod db;
mod group;
mod plugin;
mod rest;
mod room;
mod routine;
mod rpc;
mod tuya_cloud;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::info;

use synaptex_proto::device_service_server::DeviceServiceServer;
use synaptex_tuya::{TuyaPlugin, plugin::TuyaConfig};

use db::PluginConfig;

#[derive(Debug, Parser)]
#[command(name = "synaptex-core", version, about = "Synaptex smart home controller daemon")]
struct Args {
    /// Path for the Unix domain socket.
    #[arg(long, default_value = "./synaptex.sock", env = "SYNAPTEX_SOCKET")]
    socket: PathBuf,

    /// Path for the sled database directory.
    #[arg(long, default_value = "./db", env = "SYNAPTEX_DB")]
    db: PathBuf,

    /// Port for the HTTP REST API.
    #[arg(long, default_value_t = 8080u16, env = "SYNAPTEX_HTTP_PORT")]
    http_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // ── Tracing ───────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "synaptex_core=info,synaptex_tuya=info".into()),
        )
        .init();

    info!("synaptex-core starting");

    // ── Storage ───────────────────────────────────────────────────────────────
    std::fs::create_dir_all(&args.db).context("create db directory")?;
    let sled_db = sled::open(&args.db).context("open sled database")?;
    let trees   = Arc::new(db::Trees::open(&sled_db).context("open sled trees")?);

    // ── Hot cache — hydrate from persisted state ──────────────────────────────
    let cache = Arc::new(cache::StateCache::new());
    for item in trees.state.iter() {
        let (_k, v) = item?;
        if let Ok(state) = postcard::from_bytes(&v) {
            cache.insert(state);
        }
    }

    // ── Message bus ───────────────────────────────────────────────────────────
    let bus_tx = bus::new_bus();
    bus::spawn_persist_task(bus_tx.clone(), trees.clone(), cache.clone());

    // ── Plugin registry ───────────────────────────────────────────────────────
    let registry = Arc::new(plugin::PluginRegistry::new(cache.clone(), bus_tx.clone()));

    let configs = db::load_all_plugin_configs(&trees)
        .context("load plugin configs from sled")?;

    let config_count = configs.len();
    for cfg in configs {
        match cfg {
            PluginConfig::Tuya(tuya_cfg) => {
                let info = match db::get(&trees.registry, &tuya_cfg.device_id)
                    .context("read device info")?
                {
                    Some(info) => info,
                    None => {
                        tracing::warn!(
                            device = %tuya_cfg.device_id,
                            "config found but no registry entry — skipping"
                        );
                        continue;
                    }
                };

                let plugin = TuyaPlugin::new(
                    info,
                    TuyaConfig {
                        ip:        tuya_cfg.ip,
                        port:      tuya_cfg.port,
                        tuya_id:   tuya_cfg.tuya_id.clone(),
                        local_key: tuya_cfg.local_key.clone(),
                        dp_map:    tuya_cfg.dp_map(),
                    },
                    bus_tx.clone(),
                );
                registry.register(Arc::new(plugin));
            }
            PluginConfig::Group(group_cfg) => {
                let info = match db::get(&trees.registry, &group_cfg.device_id)
                    .context("read group device info")?
                {
                    Some(info) => info,
                    None => {
                        tracing::warn!(
                            device = %group_cfg.device_id,
                            "group config found but no registry entry — skipping"
                        );
                        continue;
                    }
                };

                let plugin = group::GroupPlugin::new(
                    info,
                    group_cfg.member_ids,
                    registry.clone(),
                    cache.clone(),
                    bus_tx.clone(),
                );
                registry.register(Arc::new(plugin));
            }
        }
    }

    info!(loaded = config_count, "plugin configs loaded");

    // ── Routine runner + cron tasks ──────────────────────────────────────────
    let routine_runner = Arc::new(routine::RoutineRunner::new());

    let saved_routines = db::list_routines(&trees)
        .context("load routines from sled")?;
    for r in saved_routines {
        if r.schedule.is_some() {
            if let Err(e) = routine_runner.start_cron(r, registry.clone(), trees.clone()) {
                tracing::warn!("failed to start cron task: {e}");
            }
        }
    }

    // ── HTTP REST API ────────────────────────────────────────────────────────
    {
        let app_state = rest::AppState {
            cache:          cache.clone(),
            registry:       registry.clone(),
            trees:          trees.clone(),
            bus_tx:         bus_tx.clone(),
            routine_runner: routine_runner.clone(),
        };
        let http_addr = std::net::SocketAddr::from(([0, 0, 0, 0], args.http_port));
        tokio::spawn(async move {
            let tcp = tokio::net::TcpListener::bind(http_addr)
                .await
                .expect("bind HTTP port");
            info!(addr = %http_addr, "HTTP API listening");
            axum::serve(tcp, rest::mk_router(app_state))
                .await
                .expect("HTTP server error");
        });
    }

    // ── gRPC server on UDS ────────────────────────────────────────────────────
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent).context("create socket directory")?;
    }
    if args.socket.exists() {
        std::fs::remove_file(&args.socket).context("remove stale socket")?;
    }

    let listener   = UnixListener::bind(&args.socket).context("bind UDS")?;
    let uds_stream = UnixListenerStream::new(listener);

    let service = rpc::DeviceServiceImpl {
        cache,
        registry,
        trees,
        bus_tx,
        routine_runner,
    };

    info!(socket = %args.socket.display(), "gRPC server listening");

    Server::builder()
        .add_service(DeviceServiceServer::new(service))
        .serve_with_incoming(uds_stream)
        .await
        .context("gRPC server error")?;

    Ok(())
}
