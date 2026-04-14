use std::{pin::Pin, sync::Arc};

use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::{Request, Response, Status};

use synaptex_router_proto::{
    router_service_server::RouterService,
    Ack, DhcpReservation, DhcpReservationList, DiscoveredDevice, DiscoveryRequest,
    Empty, FirewallRule, FirewallRuleList, MacAddress, RegisterDeviceRequest,
    RegisterDeviceResponse, RuleId, StatusRequest, StatusResponse,
};

use crate::{db::{DeviceKind, NetPolicy, RouterDb, RouterDevice}, dhcp::KeaClient, firewall};

fn build_kind(r: &RegisterDeviceRequest) -> DeviceKind {
    match r.kind.as_str() {
        "bond" => DeviceKind::Bond {
            bond_id:    r.bond_id.clone(),
            bond_token: r.bond_token.clone(),
        },
        "matter" => DeviceKind::Matter { node_id: 0 },
        "mysa"   => DeviceKind::Mysa,
        "sense"  => DeviceKind::Sense,
        "roku"   => DeviceKind::Roku,
        "wled"   => DeviceKind::Wled,
        other    => DeviceKind::Other(other.to_string()),
    }
}

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
                Ok(DiscoveredDevice {
                    tuya_id,
                    ip:         d.ip,
                    mac:        d.mac,
                    version,
                    managed_ip: d.managed_ip.unwrap_or_default(),
                })
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

    // ── Device registration ───────────────────────────────────────────────────

    async fn register_device(
        &self,
        req: Request<RegisterDeviceRequest>,
    ) -> Result<Response<RegisterDeviceResponse>, Status> {
        let r = req.get_ref();

        if r.mac.is_empty() {
            return Err(Status::invalid_argument("mac is required"));
        }
        if !matches!(r.kind.as_str(), "bond" | "matter" | "mysa" | "sense" | "roku" | "wled" | "other") {
            return Err(Status::invalid_argument("kind must be bond, matter, mysa, sense, roku, wled, or other"));
        }

        let ie = |e: anyhow::Error| Status::internal(e.to_string());

        // Reuse an existing record (matched by MAC) or create a new one.
        let device = match self.db.get_by_mac(&r.mac).map_err(ie)? {
            Some(mut existing) => {
                if !r.ip.is_empty() {
                    existing.ip = r.ip.clone();
                }
                // Refresh kind fields (token rotation, bond_id correction).
                existing.kind = build_kind(r);
                // Backfill managed_ip if the record predates IP allocation.
                if existing.managed_ip.is_none() {
                    existing.managed_ip = self.db
                        .allocate_ip(&existing.device_id)
                        .ok()
                        .map(|a| a.to_string());
                }
                existing
            }
            None => {
                let device_id = uuid::Uuid::new_v4().to_string();
                let managed_ip = self.db
                    .allocate_ip(&device_id)
                    .map_err(ie)?
                    .to_string();
                RouterDevice {
                    device_id,
                    ip:         r.ip.clone(),
                    mac:        r.mac.clone(),
                    managed_ip: Some(managed_ip),
                    kind:       build_kind(r),
                    net_policy: NetPolicy::Provisioned,
                }
            }
        };

        self.db.upsert(&device).map_err(ie)?;

        // Push Kea reservation so the device migrates on next DHCP renewal.
        if let (Some(ref kea), Some(ref managed_ip)) = (&self.kea, &device.managed_ip) {
            if let Err(e) = kea.reservation_add(&device.mac, managed_ip).await {
                tracing::warn!(mac = %device.mac, %managed_ip,
                    "register_device: kea reservation: {e:#}");
            }
        }

        tracing::info!(
            mac        = %device.mac,
            managed_ip = ?device.managed_ip,
            kind       = %r.kind,
            "router: device registered",
        );

        Ok(Response::new(RegisterDeviceResponse {
            device_id:  device.device_id,
            managed_ip: device.managed_ip.unwrap_or_default(),
        }))
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
