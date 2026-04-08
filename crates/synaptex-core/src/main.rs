mod bus;
mod cache;
mod db;
mod group;
mod plugin;
mod rest;
mod room;
mod routine;
mod router_client;
mod tuya_cloud;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use synaptex_tuya::{TuyaPlugin, plugin::TuyaConfig};

use db::PluginConfig;

#[derive(Debug, Parser)]
#[command(name = "synaptex-core", version, about = "Synaptex smart home controller daemon")]
struct Args {
    /// Path for the sled database directory.
    #[arg(long, default_value = "./db", env = "SYNAPTEX_DB")]
    db: std::path::PathBuf,

    /// Port for the HTTP REST API.
    #[arg(long, default_value_t = 8080u16, env = "SYNAPTEX_HTTP_PORT")]
    http_port: u16,

    /// gRPC endpoint of synaptex-router, e.g. "https://10.40.1.1:50052".
    /// When set, core connects to the router and streams device discovery events.
    /// Requires --router-cert.
    #[arg(long, env = "SYNAPTEX_ROUTER_URL")]
    router_url: Option<String>,

    /// Path to synaptex-router's TLS certificate PEM.
    /// Copy router.crt from the router host after its first run.
    /// Required when --router-url is set.
    #[arg(long, env = "SYNAPTEX_ROUTER_CERT")]
    router_cert: Option<std::path::PathBuf>,
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
                        ip:            tuya_cfg.ip,
                        port:          tuya_cfg.port,
                        tuya_id:       tuya_cfg.tuya_id.clone(),
                        local_key:     tuya_cfg.local_key.clone(),
                        dp_map:        tuya_cfg.dp_map(),
                        protocol_hint: tuya_cfg.protocol_hint.clone(),
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

    // ── Router device cache (shared between discovery loop and REST handlers) ─
    let router_devices = Arc::new(dashmap::DashMap::new());

    // ── Router client (optional) ─────────────────────────────────────────────
    match (&args.router_url, &args.router_cert) {
        (Some(url), Some(cert_path)) => {
            let cert_pem = std::fs::read(cert_path)
                .with_context(|| format!("read router cert {}", cert_path.display()))?;
            let cfg = router_client::RouterClientConfig {
                endpoint:        url.clone(),
                router_cert_pem: cert_pem,
            };
            tokio::spawn(router_client::run_discovery_loop(cfg, router_devices.clone(), trees.clone()));
            info!(endpoint = %url, "router client starting");
        }
        (Some(_), None) => anyhow::bail!("--router-cert is required when --router-url is set"),
        (None, Some(_)) => anyhow::bail!("--router-url is required when --router-cert is set"),
        (None, None) => {}
    }

    // ── HTTP REST API (blocks until shutdown) ────────────────────────────────
    let app_state = rest::AppState {
        cache:          cache.clone(),
        registry:       registry.clone(),
        trees:          trees.clone(),
        bus_tx:         bus_tx.clone(),
        routine_runner: routine_runner.clone(),
        router_devices,
    };
    let http_addr = std::net::SocketAddr::from(([0, 0, 0, 0], args.http_port));
    let tcp = tokio::net::TcpListener::bind(http_addr)
        .await
        .context("bind HTTP port")?;
    info!(addr = %http_addr, "HTTP API listening");
    axum::serve(tcp, rest::mk_router(app_state))
        .await
        .context("HTTP server error")?;

    Ok(())
}
