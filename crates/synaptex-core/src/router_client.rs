/// Client for the synaptex-router gRPC service.
///
/// synaptex-core connects to synaptex-router over mTLS to:
/// - Subscribe to device discovery events (`WatchDiscovery` streaming RPC)
/// - Manage DHCP static reservations
/// - Manage nftables firewall rules
///
/// # TLS setup
/// The router generates a self-signed certificate on first run.  Copy that
/// certificate to this host and configure `--router-cert` (or
/// `SYNAPTEX_ROUTER_CERT`) to point at it.  Optionally provide a client
/// certificate (`--core-cert` / `--core-key`) to enable full mTLS.
///
/// # Current state
/// Stub — connection logic is in place but callers are not yet wired in.
use anyhow::{Context, Result};
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

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
