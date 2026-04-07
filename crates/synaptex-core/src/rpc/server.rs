use std::{pin::Pin, sync::Arc};

use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};
use uuid::Uuid;

use synaptex_proto::{
    device_service_server::DeviceService,
    CancelRoutineRequest, CancelRoutineResponse,
    CreateGroupRequest, CreateGroupResponse,
    CreateRoomRequest, CreateRoomResponse,
    CreateRoutineRequest, CreateRoutineResponse,
    DeleteRoomRequest, DeleteRoomResponse,
    DeleteRoutineRequest, DeleteRoutineResponse,
    DeviceStateEvent,
    GetDeviceStateRequest,
    GetDeviceStateResponse,
    GetRoomRequest, GetRoomResponse,
    GetRoutineRequest, GetRoutineResponse,
    ListDevicesRequest,
    ListDevicesResponse,
    ListRoomsRequest, ListRoomsResponse,
    ListRoutinesRequest, ListRoutinesResponse,
    RegisterDeviceRequest,
    RegisterDeviceResponse,
    SendRoomCommandRequest, SendRoomCommandResponse,
    SetDeviceStateRequest,
    SetDeviceStateResponse,
    TriggerRoutineRequest, TriggerRoutineResponse,
    UnregisterDeviceRequest,
    UnregisterDeviceResponse,
    UpdateGroupRequest, UpdateGroupResponse,
    UpdateRoomRequest, UpdateRoomResponse,
    UpdateRoutineRequest, UpdateRoutineResponse,
    WatchDeviceStateRequest,
};
use synaptex_tuya::{TuyaPlugin, plugin::TuyaConfig};
use synaptex_types::{device::DeviceInfo, plugin::StateBusSender};

use crate::{
    cache::StateCache,
    db::{self, GroupConfig, PluginConfig, Room, Routine, Trees},
    group::{self, GroupPlugin},
    plugin::PluginRegistry,
    routine::RoutineRunner,
};

use super::convert::{
    device_info_to_proto, device_state_to_proto,
    internal_id_to_proto, proto_command_to_device_command,
    proto_id_to_internal, proto_register_to_internal,
    proto_routine_step_to_internal, proto_send_room_command_to_device_command,
    room_info_to_proto, routine_info_to_proto,
};

// ─── Service handle ──────────────────────────────────────────────────────────

pub struct DeviceServiceImpl {
    pub cache:          Arc<StateCache>,
    pub registry:       Arc<PluginRegistry>,
    pub trees:          Arc<Trees>,
    pub bus_tx:         StateBusSender,
    pub routine_runner: Arc<RoutineRunner>,
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
        let device_cmd = proto_command_to_device_command(cmd)?;

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
        let (mut info, tuya_cfg) = proto_register_to_internal(req.into_inner())?;

        // If the client didn't send explicit capabilities, derive them from the DP map.
        if info.capabilities.is_empty() {
            info.capabilities = tuya_cfg.dp_map().capabilities();
        }

        let id = info.id;

        db::register_device(&self.trees, &info)
            .map_err(|e| Status::internal(format!("persist device info: {e}")))?;
        db::save_plugin_config(&self.trees, &id, &PluginConfig::Tuya(tuya_cfg.clone()))
            .map_err(|e| Status::internal(format!("persist plugin config: {e}")))?;

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

    // ── CreateGroup ──────────────────────────────────────────────────────────

    async fn create_group(
        &self,
        req: Request<CreateGroupRequest>,
    ) -> Result<Response<CreateGroupResponse>, Status> {
        let req = req.into_inner();

        if req.member_ids.is_empty() {
            return Ok(Response::new(CreateGroupResponse {
                ok: false,
                error_message: "member_ids is required".into(),
                id: None,
            }));
        }

        let member_ids: Result<Vec<_>, _> = req.member_ids
            .iter()
            .map(proto_id_to_internal)
            .collect();
        let member_ids = member_ids.map_err(|e| Status::invalid_argument(e.to_string()))?;

        // Compute union of capabilities from member DeviceInfos.
        let mut capabilities = Vec::new();
        for &mid in &member_ids {
            match db::get::<DeviceInfo>(&self.trees.registry, &mid)
                .map_err(|e| Status::internal(e.to_string()))?
            {
                Some(info) => {
                    for cap in info.capabilities {
                        if !capabilities.contains(&cap) {
                            capabilities.push(cap);
                        }
                    }
                }
                None => {
                    return Ok(Response::new(CreateGroupResponse {
                        ok: false,
                        error_message: format!("member {mid} not found in registry"),
                        id: None,
                    }));
                }
            }
        }

        let group_id = group::new_group_id();
        let info = DeviceInfo {
            id:           group_id,
            name:         req.name,
            model:        req.model,
            protocol:     "group".into(),
            capabilities: capabilities.clone(),
        };

        db::register_device(&self.trees, &info)
            .map_err(|e| Status::internal(format!("persist group info: {e}")))?;
        db::save_plugin_config(
            &self.trees,
            &group_id,
            &PluginConfig::Group(GroupConfig { device_id: group_id, member_ids: member_ids.clone() }),
        )
        .map_err(|e| Status::internal(format!("persist group config: {e}")))?;

        let plugin = GroupPlugin::new(
            info,
            member_ids,
            self.registry.clone(),
            self.cache.clone(),
            self.bus_tx.clone(),
        );
        self.registry.register(Arc::new(plugin));

        info!(group = %group_id, "group created");
        Ok(Response::new(CreateGroupResponse {
            ok: true,
            error_message: String::new(),
            id: Some(internal_id_to_proto(&group_id)),
        }))
    }

    // ── UpdateGroup ──────────────────────────────────────────────────────────

    async fn update_group(
        &self,
        req: Request<UpdateGroupRequest>,
    ) -> Result<Response<UpdateGroupResponse>, Status> {
        let req      = req.into_inner();
        let group_id = proto_id_to_internal(
            req.group_id.as_ref()
                .ok_or_else(|| Status::invalid_argument("group_id is required"))?,
        )?;

        let mut info: DeviceInfo = db::get(&self.trees.registry, &group_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("group {group_id} not found")))?;

        if !req.name.is_empty() {
            info.name = req.name;
        }

        let member_ids = if !req.member_ids.is_empty() {
            let ids: Result<Vec<_>, _> = req.member_ids.iter().map(proto_id_to_internal).collect();
            let ids = ids.map_err(|e| Status::invalid_argument(e.to_string()))?;

            // Recompute capability union.
            let mut capabilities = Vec::new();
            for &mid in &ids {
                match db::get::<DeviceInfo>(&self.trees.registry, &mid)
                    .map_err(|e| Status::internal(e.to_string()))?
                {
                    Some(minfo) => {
                        for cap in minfo.capabilities {
                            if !capabilities.contains(&cap) {
                                capabilities.push(cap);
                            }
                        }
                    }
                    None => {
                        return Ok(Response::new(UpdateGroupResponse {
                            ok: false,
                            error_message: format!("member {mid} not found"),
                        }));
                    }
                }
            }
            info.capabilities = capabilities;
            ids
        } else {
            // Keep existing members from persisted config.
            let cfg: PluginConfig = db::get(&self.trees.configs, &group_id)
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::not_found("group config not found"))?;
            match cfg {
                PluginConfig::Group(g) => g.member_ids,
                _ => return Err(Status::internal("expected group config")),
            }
        };

        db::register_device(&self.trees, &info)
            .map_err(|e| Status::internal(format!("persist group info: {e}")))?;
        db::save_plugin_config(
            &self.trees,
            &group_id,
            &PluginConfig::Group(GroupConfig { device_id: group_id, member_ids: member_ids.clone() }),
        )
        .map_err(|e| Status::internal(format!("persist group config: {e}")))?;

        // Replace the running plugin.
        self.registry.deregister(&group_id).await;
        let plugin = GroupPlugin::new(
            info,
            member_ids,
            self.registry.clone(),
            self.cache.clone(),
            self.bus_tx.clone(),
        );
        self.registry.register(Arc::new(plugin));

        info!(group = %group_id, "group updated");
        Ok(Response::new(UpdateGroupResponse { ok: true, error_message: String::new() }))
    }

    // ── CreateRoom ───────────────────────────────────────────────────────────

    async fn create_room(
        &self,
        req: Request<CreateRoomRequest>,
    ) -> Result<Response<CreateRoomResponse>, Status> {
        let req = req.into_inner();

        let device_ids: Result<Vec<_>, _> = req.device_ids
            .iter()
            .map(proto_id_to_internal)
            .collect();
        let device_ids = device_ids.map_err(|e| Status::invalid_argument(e.to_string()))?;

        // Validate all device IDs exist.
        for &did in &device_ids {
            let exists: bool = db::get::<DeviceInfo>(&self.trees.registry, &did)
                .map_err(|e| Status::internal(e.to_string()))?
                .is_some();
            if !exists {
                return Ok(Response::new(CreateRoomResponse {
                    ok: false,
                    error_message: format!("device {did} not found"),
                    id: String::new(),
                }));
            }
        }

        let room_id = Uuid::new_v4().to_string();
        let room = Room { id: room_id.clone(), name: req.name, device_ids };
        db::save_room(&self.trees, &room)
            .map_err(|e| Status::internal(format!("persist room: {e}")))?;

        info!(room_id = %room_id, "room created");
        Ok(Response::new(CreateRoomResponse {
            ok: true, error_message: String::new(), id: room_id,
        }))
    }

    // ── UpdateRoom ───────────────────────────────────────────────────────────

    async fn update_room(
        &self,
        req: Request<UpdateRoomRequest>,
    ) -> Result<Response<UpdateRoomResponse>, Status> {
        let req = req.into_inner();

        let mut room = db::get_room(&self.trees, &req.room_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("room {} not found", req.room_id)))?;

        if !req.name.is_empty() {
            room.name = req.name;
        }

        if !req.device_ids.is_empty() {
            let device_ids: Result<Vec<_>, _> = req.device_ids
                .iter()
                .map(proto_id_to_internal)
                .collect();
            let device_ids = device_ids.map_err(|e| Status::invalid_argument(e.to_string()))?;

            for &did in &device_ids {
                let exists = db::get::<DeviceInfo>(&self.trees.registry, &did)
                    .map_err(|e| Status::internal(e.to_string()))?
                    .is_some();
                if !exists {
                    return Ok(Response::new(UpdateRoomResponse {
                        ok: false,
                        error_message: format!("device {did} not found"),
                    }));
                }
            }

            room.device_ids = device_ids;
        }

        db::save_room(&self.trees, &room)
            .map_err(|e| Status::internal(format!("persist room: {e}")))?;

        info!(room_id = %room.id, "room updated");
        Ok(Response::new(UpdateRoomResponse { ok: true, error_message: String::new() }))
    }

    // ── DeleteRoom ───────────────────────────────────────────────────────────

    async fn delete_room(
        &self,
        req: Request<DeleteRoomRequest>,
    ) -> Result<Response<DeleteRoomResponse>, Status> {
        let room_id = req.into_inner().room_id;
        db::remove_room(&self.trees, &room_id)
            .map_err(|e| Status::internal(format!("remove room: {e}")))?;

        info!(room_id = %room_id, "room deleted");
        Ok(Response::new(DeleteRoomResponse { ok: true, error_message: String::new() }))
    }

    // ── ListRooms ────────────────────────────────────────────────────────────

    async fn list_rooms(
        &self,
        _req: Request<ListRoomsRequest>,
    ) -> Result<Response<ListRoomsResponse>, Status> {
        let rooms = db::list_rooms(&self.trees)
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(room_info_to_proto)
            .collect();

        Ok(Response::new(ListRoomsResponse { rooms }))
    }

    // ── GetRoom ──────────────────────────────────────────────────────────────

    async fn get_room(
        &self,
        req: Request<GetRoomRequest>,
    ) -> Result<Response<GetRoomResponse>, Status> {
        let room_id = req.into_inner().room_id;
        let room = db::get_room(&self.trees, &room_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("room {room_id} not found")))?;

        Ok(Response::new(GetRoomResponse {
            room: Some(room_info_to_proto(room)),
        }))
    }

    // ── SendRoomCommand ──────────────────────────────────────────────────────

    async fn send_room_command(
        &self,
        req: Request<SendRoomCommandRequest>,
    ) -> Result<Response<SendRoomCommandResponse>, Status> {
        let req     = req.into_inner();
        let room_id = req.room_id.clone();

        let room = db::get_room(&self.trees, &room_id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("room {room_id} not found")))?;

        let cmd = proto_send_room_command_to_device_command(req.command)?;

        match crate::room::execute_room_command(&room, cmd, &self.registry).await {
            Ok(()) => Ok(Response::new(SendRoomCommandResponse {
                ok: true, error_message: String::new(),
            })),
            Err(e) => Ok(Response::new(SendRoomCommandResponse {
                ok: false, error_message: e.to_string(),
            })),
        }
    }

    // ── CreateRoutine ────────────────────────────────────────────────────────

    async fn create_routine(
        &self,
        req: Request<CreateRoutineRequest>,
    ) -> Result<Response<CreateRoutineResponse>, Status> {
        let req = req.into_inner();

        let steps: Result<Vec<_>, _> = req.steps
            .into_iter()
            .map(proto_routine_step_to_internal)
            .collect();
        let steps = steps?;

        let id      = Uuid::new_v4().to_string();
        let routine = Routine {
            id:       id.clone(),
            name:     req.name,
            schedule: if req.schedule.is_empty() { None } else { Some(req.schedule) },
            steps,
        };
        let has_schedule = routine.schedule.is_some();

        db::save_routine(&self.trees, &routine)
            .map_err(|e| Status::internal(format!("persist routine: {e}")))?;

        if has_schedule {
            self.routine_runner
                .start_cron(routine, self.registry.clone(), self.trees.clone())
                .map_err(|e| Status::invalid_argument(format!("invalid cron expression: {e}")))?;
        }

        info!(routine_id = %id, "routine created");
        Ok(Response::new(CreateRoutineResponse { ok: true, error_message: String::new(), id }))
    }

    // ── UpdateRoutine ────────────────────────────────────────────────────────

    async fn update_routine(
        &self,
        req: Request<UpdateRoutineRequest>,
    ) -> Result<Response<UpdateRoutineResponse>, Status> {
        let req = req.into_inner();

        let mut routine = db::get_routine(&self.trees, &req.id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("routine {} not found", req.id)))?;

        if !req.name.is_empty() {
            routine.name = req.name;
        }

        let schedule_changed = !req.schedule.is_empty();
        if schedule_changed {
            routine.schedule = Some(req.schedule);
        }

        if !req.steps.is_empty() {
            let steps: Result<Vec<_>, _> = req.steps
                .into_iter()
                .map(proto_routine_step_to_internal)
                .collect();
            routine.steps = steps?;
        }

        let id = routine.id.clone();

        db::save_routine(&self.trees, &routine)
            .map_err(|e| Status::internal(format!("persist routine: {e}")))?;

        if schedule_changed {
            self.routine_runner.stop_cron(&id);
            if routine.schedule.is_some() {
                self.routine_runner
                    .start_cron(routine, self.registry.clone(), self.trees.clone())
                    .map_err(|e| Status::invalid_argument(format!("invalid cron: {e}")))?;
            }
        }

        info!(routine_id = %id, "routine updated");
        Ok(Response::new(UpdateRoutineResponse { ok: true, error_message: String::new() }))
    }

    // ── DeleteRoutine ────────────────────────────────────────────────────────

    async fn delete_routine(
        &self,
        req: Request<DeleteRoutineRequest>,
    ) -> Result<Response<DeleteRoutineResponse>, Status> {
        let id = req.into_inner().id;
        self.routine_runner.stop_cron(&id);
        self.routine_runner.cancel(&id);
        db::remove_routine(&self.trees, &id)
            .map_err(|e| Status::internal(format!("remove routine: {e}")))?;

        info!(routine_id = %id, "routine deleted");
        Ok(Response::new(DeleteRoutineResponse { ok: true, error_message: String::new() }))
    }

    // ── ListRoutines ─────────────────────────────────────────────────────────

    async fn list_routines(
        &self,
        _req: Request<ListRoutinesRequest>,
    ) -> Result<Response<ListRoutinesResponse>, Status> {
        let routines = db::list_routines(&self.trees)
            .map_err(|e| Status::internal(e.to_string()))?
            .into_iter()
            .map(routine_info_to_proto)
            .collect();

        Ok(Response::new(ListRoutinesResponse { routines }))
    }

    // ── GetRoutine ───────────────────────────────────────────────────────────

    async fn get_routine(
        &self,
        req: Request<GetRoutineRequest>,
    ) -> Result<Response<GetRoutineResponse>, Status> {
        let id = req.into_inner().id;
        let routine = db::get_routine(&self.trees, &id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("routine {id} not found")))?;

        Ok(Response::new(GetRoutineResponse {
            routine: Some(routine_info_to_proto(routine)),
        }))
    }

    // ── TriggerRoutine ───────────────────────────────────────────────────────

    async fn trigger_routine(
        &self,
        req: Request<TriggerRoutineRequest>,
    ) -> Result<Response<TriggerRoutineResponse>, Status> {
        let id = req.into_inner().id;
        let routine = db::get_routine(&self.trees, &id)
            .map_err(|e| Status::internal(e.to_string()))?
            .ok_or_else(|| Status::not_found(format!("routine {id} not found")))?;

        self.routine_runner.trigger(routine, self.registry.clone(), self.trees.clone());

        Ok(Response::new(TriggerRoutineResponse { ok: true, error_message: String::new() }))
    }

    // ── CancelRoutine ────────────────────────────────────────────────────────

    async fn cancel_routine(
        &self,
        req: Request<CancelRoutineRequest>,
    ) -> Result<Response<CancelRoutineResponse>, Status> {
        let id = req.into_inner().id;
        self.routine_runner.cancel(&id);
        Ok(Response::new(CancelRoutineResponse { ok: true, error_message: String::new() }))
    }
}
