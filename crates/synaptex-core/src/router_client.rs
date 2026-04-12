/// Cached info about a device most recently seen by the router.
#[derive(Clone, Debug)]
pub struct RouterDiscoveredDevice {
    pub ip:         std::net::Ipv4Addr,
    pub mac:        String,
    pub version:    String,
    pub managed_ip: Option<std::net::Ipv4Addr>,
}

/// Client for the synaptex-router gRPC service.
///
/// synaptex-core connects to synaptex-router over TLS to:
/// - Subscribe to device discovery events (`WatchDiscovery` streaming RPC)
/// - Manage DHCP static reservations
/// - Manage nftables firewall rules
///
/// # TLS setup
/// The router generates a self-signed certificate on first run.  Copy that
/// certificate to this host and point `--router-cert` (or
/// `SYNAPTEX_ROUTER_CERT`) at it.  Core pins that cert as the CA so the
/// connection is authenticated without a PKI.
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use dashmap::DashMap;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

use crate::db::{PluginConfig, Trees};

use synaptex_router_proto::router_service_client::RouterServiceClient;

/// Configuration for the router gRPC connection.
#[derive(Debug, Clone)]
pub struct RouterClientConfig {
    /// gRPC endpoint, e.g. "https://10.40.1.1:50052"
    pub endpoint: String,
    /// PEM bytes of the router's TLS certificate (used as the CA to verify
    /// the server).  Load from the file copied from the router host.
    pub router_cert_pem: Vec<u8>,
}

/// A connected client to the synaptex-router gRPC service.
pub struct RouterClient {
    inner: RouterServiceClient<Channel>,
}

impl RouterClient {
    /// Connect to synaptex-router using server-auth TLS (router cert pinned).
    pub async fn connect(cfg: RouterClientConfig) -> Result<Self> {
        let ca = Certificate::from_pem(&cfg.router_cert_pem);
        let tls = ClientTlsConfig::new()
            .ca_certificate(ca)
            .domain_name("synaptex-router");

        let channel = Channel::from_shared(cfg.endpoint.clone())
            .context("invalid router endpoint")?
            .tls_config(tls)
            .context("configure TLS")?
            .connect_timeout(Duration::from_secs(10))
            .connect()
            .await
            .with_context(|| format!("connect to synaptex-router at {}", cfg.endpoint))?;

        Ok(Self { inner: RouterServiceClient::new(channel) })
    }

    /// Stream discovered devices from the router.
    ///
    /// Returns a tonic streaming response.  The caller should loop over it
    /// and register each device with the plugin registry.
    pub async fn watch_discovery(
        &mut self,
    ) -> Result<tonic::codec::Streaming<synaptex_router_proto::DiscoveredDevice>> {
        let stream = self.inner
            .watch_discovery(synaptex_router_proto::DiscoveryRequest {})
            .await
            .context("WatchDiscovery RPC")?
            .into_inner();
        Ok(stream)
    }

    #[allow(dead_code)]
    pub async fn add_dhcp_reservation(
        &mut self,
        reservation: synaptex_router_proto::DhcpReservation,
    ) -> Result<()> {
        let ack = self.inner
            .add_dhcp_reservation(reservation)
            .await
            .context("AddDhcpReservation RPC")?
            .into_inner();
        if !ack.ok {
            anyhow::bail!("add_dhcp_reservation failed: {}", ack.error);
        }
        Ok(())
    }

    pub async fn register_device(
        &mut self,
        req: synaptex_router_proto::RegisterDeviceRequest,
    ) -> Result<synaptex_router_proto::RegisterDeviceResponse> {
        let resp = self.inner
            .register_device(req)
            .await
            .context("RegisterDevice RPC")?
            .into_inner();
        Ok(resp)
    }

    #[allow(dead_code)]
    pub async fn upsert_firewall_rule(
        &mut self,
        rule: synaptex_router_proto::FirewallRule,
    ) -> Result<()> {
        let ack = self.inner
            .upsert_firewall_rule(rule)
            .await
            .context("UpsertFirewallRule RPC")?
            .into_inner();
        if !ack.ok {
            anyhow::bail!("upsert_firewall_rule failed: {}", ack.error);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn remove_firewall_rule(&mut self, id: String) -> Result<()> {
        let ack = self.inner
            .remove_firewall_rule(synaptex_router_proto::RuleId { id })
            .await
            .context("RemoveFirewallRule RPC")?
            .into_inner();
        if !ack.ok {
            anyhow::bail!("remove_firewall_rule failed: {}", ack.error);
        }
        Ok(())
    }
}

// ─── Discovery loop ───────────────────────────────────────────────────────────

/// Sync router-observed fields back into the stored `TuyaDeviceConfig`:
/// - `protocol_version` is set when it was previously `None`
/// - `ip` is updated whenever it differs from the router-observed IP
///
/// No-op for unknown tuya_ids.
fn sync_device_from_router(trees: &Trees, tuya_id: &str, version: &str, new_ip: std::net::Ipv4Addr) {
    let result = (|| -> anyhow::Result<()> {
        let new_ip_addr = std::net::IpAddr::V4(new_ip);
        for item in trees.configs.iter() {
            let (k, v) = item?;
            if let Ok(PluginConfig::Tuya(mut cfg)) = postcard::from_bytes::<PluginConfig>(&v) {
                if cfg.tuya_id != tuya_id {
                    continue;
                }
                let mut changed = false;

                if cfg.protocol_version.is_none() && !version.is_empty() {
                    cfg.protocol_version = Some(version.to_string());
                    changed = true;
                    tracing::info!(tuya_id, version, "backfilled protocol_version from router");
                }

                if cfg.ip != new_ip_addr {
                    tracing::info!(tuya_id, old_ip = %cfg.ip, new_ip = %new_ip_addr, "updating device IP from router");
                    cfg.ip = new_ip_addr;
                    changed = true;
                }

                if changed {
                    let new_bytes = postcard::to_allocvec(&PluginConfig::Tuya(cfg))?;
                    trees.configs.insert(k, new_bytes)?;
                }
                return Ok(());
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!(tuya_id, "failed to sync device from router: {e}");
    }
}

/// Connect to synaptex-router and stream discovered devices indefinitely,
/// reconnecting with exponential backoff on any failure.
///
/// Each `DiscoveredDevice` received from the router is upserted into
/// `cache` (keyed by `tuya_id`) so that `POST /pairing/import` can use
/// router-side discovery as a fallback when core cannot see the device
/// subnet directly.
pub async fn run_discovery_loop(
    cfg:   RouterClientConfig,
    cache: Arc<DashMap<String, RouterDiscoveredDevice>>,
    trees: Arc<Trees>,
) {
    let mut backoff = Duration::from_secs(2);

    loop {
        tracing::info!(endpoint = %cfg.endpoint, "router: connecting");

        match RouterClient::connect(cfg.clone()).await {
            Err(e) => {
                tracing::warn!("router: connection failed: {e}; retry in {backoff:?}");
            }
            Ok(mut client) => {
                backoff = Duration::from_secs(2);

                match client.watch_discovery().await {
                    Err(e) => tracing::warn!("router: WatchDiscovery RPC failed: {e}"),
                    Ok(mut stream) => {
                        tracing::info!("router: discovery stream open");
                        loop {
                            match stream.message().await {
                                Ok(Some(device)) => {
                                    tracing::info!(
                                        tuya_id = %device.tuya_id,
                                        ip      = %device.ip,
                                        mac     = %device.mac,
                                        version = %device.version,
                                        "router: device discovered",
                                    );
                                    // Parse the IP — skip on failure rather than crashing.
                                    if let Ok(ip) = device.ip.parse::<std::net::Ipv4Addr>() {
                                        let managed_ip = device.managed_ip.parse::<std::net::Ipv4Addr>().ok();
                                        cache.insert(device.tuya_id.clone(), RouterDiscoveredDevice {
                                            ip,
                                            mac:        device.mac.clone(),
                                            version:    device.version.clone(),
                                            managed_ip,
                                        });

                                        // Sync IP + protocol_version back to stored config.
                                        sync_device_from_router(&trees, &device.tuya_id, &device.version, ip);
                                    }
                                }
                                Ok(None) => {
                                    tracing::info!("router: discovery stream closed by server");
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!("router: stream error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}
