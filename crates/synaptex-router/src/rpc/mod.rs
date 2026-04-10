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

use crate::{db::{DeviceKind, RouterDb}, dhcp::KeaClient, firewall};

// ─── Service implementation ───────────────────────────────────────────────────

pub struct RouterServiceImpl {
    pub discovery_tx: Arc<broadcast::Sender<DiscoveredDevice>>,
    pub db:           Arc<RouterDb>,
    /// Kea control client — None when --kea-ctrl-socket is not configured.
    pub kea:          Option<Arc<KeaClient>>,
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
            devices_seen_last_minute: 0,
            version:                  env!("CARGO_PKG_VERSION").to_string(),
        }))
    }

    // ── Discovery ─────────────────────────────────────────────────────────────

    type WatchDiscoveryStream = BoxStream<DiscoveredDevice>;

    async fn watch_discovery(
        &self,
        _req: Request<DiscoveryRequest>,
    ) -> Result<Response<Self::WatchDiscoveryStream>, Status> {
        let rx = self.discovery_tx.subscribe();

        let known = self.db.list_all()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(|d| {
                let (tuya_id, version) = match &d.kind {
                    DeviceKind::Tuya { tuya_id, version } => (tuya_id.clone(), version.clone()),
                    _ => (String::new(), String::new()),
                };
                Ok(DiscoveredDevice { tuya_id, ip: d.ip, mac: d.mac, version })
            });

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
        let r = req.get_ref();
        let Some(ref kea) = self.kea else {
            return Ok(Response::new(Ack {
                ok:    false,
                error: "kea control socket not configured".into(),
            }));
        };
        kea.reservation_add(&r.mac, &r.ip)
            .await
            .map(|_| Response::new(Ack { ok: true, error: String::new() }))
            .map_err(|e| Status::internal(e.to_string()))
    }

    async fn remove_dhcp_reservation(
        &self,
        req: Request<MacAddress>,
    ) -> Result<Response<Ack>, Status> {
        let Some(ref kea) = self.kea else {
            return Ok(Response::new(Ack {
                ok:    false,
                error: "kea control socket not configured".into(),
            }));
        };
        kea.reservation_del(&req.get_ref().mac)
            .await
            .map(|_| Response::new(Ack { ok: true, error: String::new() }))
            .map_err(|e| Status::internal(e.to_string()))
    }

    async fn list_dhcp_reservations(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<DhcpReservationList>, Status> {
        // List from the router DB — these are the devices whose reservations
        // we have pushed to Kea (or will push on next sync).
        let reservations = self.db.list_all()
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .filter(|d| !d.mac.is_empty() && !d.ip.is_empty())
            .map(|d| {
                let hostname = match &d.kind {
                    DeviceKind::Tuya { tuya_id, .. } => tuya_id.clone(),
                    _ => d.device_id.clone(),
                };
                DhcpReservation { mac: d.mac, ip: d.ip, hostname }
            })
            .collect();
        Ok(Response::new(DhcpReservationList { reservations }))
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
