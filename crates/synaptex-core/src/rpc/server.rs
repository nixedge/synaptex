use std::{pin::Pin, sync::Arc};

use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use synaptex_proto::{
    device_service_server::DeviceService,
    DeviceStateEvent,
    GetDeviceStateRequest,
    GetDeviceStateResponse,
    ListDevicesRequest,
    ListDevicesResponse,
    RegisterDeviceRequest,
    RegisterDeviceResponse,
    SetDeviceStateRequest,
    SetDeviceStateResponse,
    UnregisterDeviceRequest,
    UnregisterDeviceResponse,
    WatchDeviceStateRequest,
};
use synaptex_tuya::{TuyaPlugin, plugin::TuyaConfig};
use synaptex_types::plugin::StateBusSender;

use crate::{
    cache::StateCache,
    db::{self, PluginConfig, Trees},
    plugin::PluginRegistry,
};

use super::convert::{
    device_info_to_proto, device_state_to_proto,
    proto_command_to_device_command, proto_id_to_internal,
    proto_register_to_internal,
};

// ─── Service handle ──────────────────────────────────────────────────────────

pub struct DeviceServiceImpl {
    pub cache:    Arc<StateCache>,
    pub registry: Arc<PluginRegistry>,
    pub trees:    Arc<Trees>,
    pub bus_tx:   StateBusSender,
}

type BoxStream<T> =
    Pin<Box<dyn futures_core::Stream<Item = Result<T, Status>> + Send + 'static>>;

// ─── Trait impl ──────────────────────────────────────────────────────────────

#[tonic::async_trait]
impl DeviceService for DeviceServiceImpl {
    // ── GetDeviceState ───────────────────────────────────────────────────────

    async fn get_device_state(
        &self,
        req: Request<GetDeviceStateRequest>,
    ) -> Result<Response<GetDeviceStateResponse>, Status> {
        let req = req.into_inner();
        let id  = proto_id_to_internal(req.device_id.as_ref().ok_or_else(|| {
            Status::invalid_argument("device_id is required")
        })?)?;

        debug!(device = %id, "get_state");

        let state = self
            .cache
            .get(&id)
            .ok_or_else(|| Status::not_found(format!("device {id} not found")))?;

        Ok(Response::new(GetDeviceStateResponse {
            state: Some(device_state_to_proto(state)),
        }))
    }

    // ── SetDeviceState ───────────────────────────────────────────────────────

    async fn set_device_state(
        &self,
        req: Request<SetDeviceStateRequest>,
    ) -> Result<Response<SetDeviceStateResponse>, Status> {
        let req = req.into_inner();
        let id  = proto_id_to_internal(req.device_id.as_ref().ok_or_else(|| {
            Status::invalid_argument("device_id is required")
        })?)?;

        let cmd        = req.command.ok_or_else(|| Status::invalid_argument("command is required"))?;
        let device_cmd = proto_command_to_device_command(cmd);

        info!(device = %id, cmd = ?device_cmd, "command received");

        match self.registry.execute_command(&id, device_cmd).await {
            Ok(()) => {
                info!(device = %id, "command dispatched ok");
                Ok(Response::new(SetDeviceStateResponse {
                    ok: true, error_message: String::new(),
                }))
            }
            Err(e) => {
                warn!(device = %id, error = %e, "command failed");
                Ok(Response::new(SetDeviceStateResponse {
                    ok: false, error_message: e.to_string(),
                }))
            }
        }
    }

    // ── ListDevices ──────────────────────────────────────────────────────────

    async fn list_devices(
        &self,
        _req: Request<ListDevicesRequest>,
    ) -> Result<Response<ListDevicesResponse>, Status> {
        let devices = db::list_all_devices(&self.trees)
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(device_info_to_proto)
            .collect();

        Ok(Response::new(ListDevicesResponse { devices }))
    }

    // ── WatchDeviceState ─────────────────────────────────────────────────────

    type WatchDeviceStateStream = BoxStream<DeviceStateEvent>;

    async fn watch_device_state(
        &self,
        req: Request<WatchDeviceStateRequest>,
    ) -> Result<Response<Self::WatchDeviceStateStream>, Status> {
        let filter_ids: Vec<_> = req
            .into_inner()
            .device_ids
            .iter()
            .filter_map(|id| proto_id_to_internal(id).ok())
            .collect();

        let rx     = self.bus_tx.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(move |item| {
            match item {
                Ok(event) => {
                    let pass = filter_ids.is_empty()
                        || filter_ids.contains(&event.device_id);
                    if pass {
                        Some(Ok(DeviceStateEvent {
                            state: Some(device_state_to_proto(event.state)),
                        }))
                    } else {
                        None
                    }
                }
                Err(_) => None, // lagged — skip rather than kill the stream
            }
        });

        info!("WatchDeviceState stream opened");
        Ok(Response::new(Box::pin(stream)))
    }

    // ── RegisterDevice ───────────────────────────────────────────────────────

    async fn register_device(
        &self,
        req: Request<RegisterDeviceRequest>,
    ) -> Result<Response<RegisterDeviceResponse>, Status> {
        let (info, tuya_cfg) = proto_register_to_internal(req.into_inner())?;
        let id = info.id;

        // Persist both the device metadata and the plugin config atomically
        // (sled doesn't do multi-tree transactions, but both writes are
        // idempotent so a partial failure on restart is recoverable).
        db::register_device(&self.trees, &info)
            .map_err(|e| Status::internal(format!("persist device info: {e}")))?;
        db::save_plugin_config(&self.trees, &id, &PluginConfig::Tuya(tuya_cfg.clone()))
            .map_err(|e| Status::internal(format!("persist plugin config: {e}")))?;

        // Instantiate and register the plugin immediately — the supervisor
        // will connect it in the background.
        let plugin = TuyaPlugin::new(
            info,
            TuyaConfig {
                ip:        tuya_cfg.ip,
                port:      tuya_cfg.port,
                tuya_id:   tuya_cfg.tuya_id.clone(),
                local_key: tuya_cfg.local_key.clone(),
                dp_map:    tuya_cfg.dp_map(),
            },
            self.bus_tx.clone(),
        );
        self.registry.register(std::sync::Arc::new(plugin));

        info!(%id, "device registered");
        Ok(Response::new(RegisterDeviceResponse {
            ok: true, error_message: String::new(),
        }))
    }

    // ── UnregisterDevice ─────────────────────────────────────────────────────

    async fn unregister_device(
        &self,
        req: Request<UnregisterDeviceRequest>,
    ) -> Result<Response<UnregisterDeviceResponse>, Status> {
        let id = proto_id_to_internal(
            req.into_inner()
                .device_id
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("device_id is required"))?,
        )?;

        self.registry.deregister(&id).await;
        db::remove_device(&self.trees, &id)
            .map_err(|e| Status::internal(format!("remove device info: {e}")))?;
        db::remove_plugin_config(&self.trees, &id)
            .map_err(|e| Status::internal(format!("remove plugin config: {e}")))?;

        info!(%id, "device unregistered");
        Ok(Response::new(UnregisterDeviceResponse { ok: true }))
    }
}
