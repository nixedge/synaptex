use std::{pin::Pin, sync::Arc};

use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::{Request, Response, Status};

use synaptex_router_proto::{
    router_service_server::RouterService,
    Ack, DhcpReservation, DhcpReservationList, DiscoveredDevice, DiscoveryRequest,
    Empty, FirewallRule, FirewallRuleList, MacAddress, RuleId,
    StatusRequest, StatusResponse,
};

use crate::{db::RouterDb, dhcp, firewall};

// ─── Service implementation ───────────────────────────────────────────────────

pub struct RouterServiceImpl {
    pub discovery_tx: Arc<broadcast::Sender<DiscoveredDevice>>,
    pub db:           Arc<RouterDb>,
}

type BoxStream<T> = Pin<Box<dyn futures_core::Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl RouterService for RouterServiceImpl {
    // ── Status ────────────────────────────────────────────────────────────────

    async fn get_status(
        &self,
        _req: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        Ok(Response::new(StatusResponse {
            discovery_active:         true,
            devices_seen_last_minute: 0, // TODO: track with an AtomicU32 + timestamp
            version:                  env!("CARGO_PKG_VERSION").to_string(),
        }))
    }

    // ── Discovery ─────────────────────────────────────────────────────────────

    type WatchDiscoveryStream = BoxStream<DiscoveredDevice>;

    async fn watch_discovery(
        &self,
        _req: Request<DiscoveryRequest>,
    ) -> Result<Response<Self::WatchDiscoveryStream>, Status> {
        // Subscribe before reading the DB snapshot to avoid a race where a
        // change fires between the list and the subscribe.
        let rx = self.discovery_tx.subscribe();

        // Send all currently-known devices as the initial burst.
        let known = self.db.list_all()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(|r| Ok(DiscoveredDevice {
                tuya_id: r.tuya_id,
                ip:      r.ip,
                mac:     r.mac,
                version: r.version,
            }));

        let initial = tokio_stream::iter(known);
        let changes = BroadcastStream::new(rx)
            .filter_map(|item| item.ok())
            .map(Ok);

        Ok(Response::new(Box::pin(initial.chain(changes))))
    }

    // ── DHCP ──────────────────────────────────────────────────────────────────

    async fn add_dhcp_reservation(
        &self,
        req: Request<DhcpReservation>,
    ) -> Result<Response<Ack>, Status> {
        dhcp::add(req.get_ref()).await
            .map(|_| Response::new(Ack { ok: true, error: String::new() }))
            .map_err(|e| Status::internal(e.to_string()))
    }

    async fn remove_dhcp_reservation(
        &self,
        req: Request<MacAddress>,
    ) -> Result<Response<Ack>, Status> {
        dhcp::remove(&req.get_ref().mac).await
            .map(|_| Response::new(Ack { ok: true, error: String::new() }))
            .map_err(|e| Status::internal(e.to_string()))
    }

    async fn list_dhcp_reservations(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<DhcpReservationList>, Status> {
        dhcp::list().await
            .map(|reservations| Response::new(DhcpReservationList { reservations }))
            .map_err(|e| Status::internal(e.to_string()))
    }

    // ── Firewall ──────────────────────────────────────────────────────────────

    async fn upsert_firewall_rule(
        &self,
        req: Request<FirewallRule>,
    ) -> Result<Response<Ack>, Status> {
        firewall::upsert(req.get_ref()).await
            .map(|_| Response::new(Ack { ok: true, error: String::new() }))
            .map_err(|e| Status::internal(e.to_string()))
    }

    async fn remove_firewall_rule(
        &self,
        req: Request<RuleId>,
    ) -> Result<Response<Ack>, Status> {
        firewall::remove(&req.get_ref().id).await
            .map(|_| Response::new(Ack { ok: true, error: String::new() }))
            .map_err(|e| Status::internal(e.to_string()))
    }

    async fn list_firewall_rules(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<FirewallRuleList>, Status> {
        firewall::list().await
            .map(|rules| Response::new(FirewallRuleList { rules }))
            .map_err(|e| Status::internal(e.to_string()))
    }
}
