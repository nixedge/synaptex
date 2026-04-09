pub mod auth;
pub mod dto;
pub mod error;
pub mod handlers;

use std::sync::Arc;

use axum::{
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use dashmap::DashMap;
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use synaptex_types::plugin::StateBusSender;

use crate::{
    cache::StateCache,
    db::Trees,
    plugin::PluginRegistry,
    routine::RoutineRunner,
    router_client::RouterDiscoveredDevice,
};

use handlers::{config, devices, events, groups, pairing, rooms, routines};

// ─── Shared application state ────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub cache:           Arc<StateCache>,
    pub registry:        Arc<PluginRegistry>,
    pub trees:           Arc<Trees>,
    pub bus_tx:          StateBusSender,
    pub routine_runner:  Arc<RoutineRunner>,
    /// Devices most recently seen by the synaptex-router gRPC stream,
    /// keyed by Tuya device ID.  Empty when router integration is disabled.
    pub router_devices:  Arc<DashMap<String, RouterDiscoveredDevice>>,
}

// ─── Router factory ──────────────────────────────────────────────────────────

pub fn mk_router(state: AppState) -> Router {
    let api = api_router(state.clone());
    Router::new()
        .nest("/api/v1", api)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

fn api_router(state: AppState) -> Router {
    // The api-key bootstrap endpoint is always unauthenticated.
    let open = Router::new()
        .route("/config/api-key", put(config::put_api_key).delete(config::delete_api_key))
        .with_state(state.clone());

    // All other endpoints go through the bearer-auth middleware.
    let protected = Router::new()
        // Config
        .route("/config",            get(config::get_config))
        .route("/config/tuya-cloud", put(config::put_tuya_cloud))
        // Devices
        .route("/devices",           get(devices::list_devices).post(devices::register_device))
        .route("/devices/:mac",      get(devices::get_device).patch(devices::patch_device).delete(devices::unregister_device))
        .route("/devices/:mac/command",      post(devices::device_command))
        .route("/devices/:mac/debug-config", get(devices::device_debug_config))
        // Groups
        .route("/groups",            get(groups::list_groups).post(groups::create_group))
        .route("/groups/:mac",       patch(groups::patch_group).delete(groups::delete_group))
        // Rooms
        .route("/rooms",             get(rooms::list_rooms).post(rooms::create_room))
        .route("/rooms/:id",
            get(rooms::get_room)
            .patch(rooms::patch_room)
            .delete(rooms::delete_room))
        .route("/rooms/:id/command", post(rooms::room_command))
        // Routines
        .route("/routines",          get(routines::list_routines).post(routines::create_routine))
        .route("/routines/:id",
            get(routines::get_routine)
            .put(routines::put_routine)
            .delete(routines::delete_routine))
        .route("/routines/:id/trigger", post(routines::trigger_routine))
        .route("/routines/:id/run",     delete(routines::cancel_routine))
        // Events (SSE)
        .route("/events",            get(events::sse_events))
        // Pairing
        .route("/pairing/cloud-devices",
            get(pairing::list_cloud_devices))
        .route("/pairing/cloud-devices/:tuya_id",
            get(pairing::get_cloud_device))
        .route("/pairing/import",
            post(pairing::import_cloud_devices))
        .route_layer(middleware::from_fn_with_state(
            state.trees.clone(),
            auth::bearer_auth,
        ))
        .with_state(state);

    Router::new().merge(open).merge(protected)
}
