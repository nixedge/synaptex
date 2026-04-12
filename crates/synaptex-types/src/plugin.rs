use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

use crate::{
    capability::{Capability, DeviceCommand, FanSpeed},
    device::DeviceId,
};

// ─── State ────────────────────────────────────────────────────────────────────

/// Complete, self-contained device state snapshot.
///
/// All optional fields are `None` when the device does not support that
/// capability.  Postcard-serializable for zero-copy storage in the sled
/// `state` tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    pub device_id:     DeviceId,
    pub online:        bool,
    /// Unix epoch milliseconds of the last update.
    pub updated_at_ms: u64,

    pub power:        Option<bool>,
    /// Brightness in the range 0–1000 (protocol-independent normalised).
    pub brightness:   Option<u16>,
    /// Colour temperature in Kelvin.
    pub color_temp_k: Option<u16>,
    pub rgb:          Option<(u8, u8, u8)>,
    /// Current mode as reported by the device (e.g. `"white"` or `"colour"` for bulbs).
    /// `None` for devices without a mode DP (switches, fans, etc.).
    pub mode:         Option<String>,
    /// Multi-switch state: index → on/off.
    pub switches:     HashMap<u8, bool>,
    pub fan_speed:    Option<FanSpeed>,
    /// Current ambient temperature reported by the device (device-native unit).
    pub temp_current:     Option<u16>,
    /// Target/set-point temperature (device-native unit).
    pub temp_set:         Option<u16>,
    /// Temperature calibration offset (signed, device-native unit).
    pub temp_calibration: Option<i16>,
}

// ─── Event Bus ────────────────────────────────────────────────────────────────

/// Emitted by a plugin on every physical state change.
/// Cloned cheaply into every subscriber.
#[derive(Debug, Clone)]
pub struct StateChangeEvent {
    pub device_id: DeviceId,
    pub state:     DeviceState,
    /// Raw DP key→value map as received from the device.
    /// Empty for synthetic events (e.g. group aggregation).
    pub raw_dps:   std::collections::HashMap<String, serde_json::Value>,
}

/// Sender half of the internal state broadcast bus.
pub type StateBusSender   = broadcast::Sender<StateChangeEvent>;
/// Receiver half of the internal state broadcast bus.
pub type StateBusReceiver = broadcast::Receiver<StateChangeEvent>;

// ─── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("device unreachable: {0}")]
    Unreachable(String),

    #[error("command not supported by this device")]
    UnsupportedCommand,

    #[error("protocol framing error: {0}")]
    Protocol(String),

    #[error("cipher error: {0}")]
    Cipher(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type PluginResult<T> = Result<T, PluginError>;

// ─── Trait ────────────────────────────────────────────────────────────────────

/// Core abstraction every device protocol adapter must implement.
///
/// # Lifecycle
///
/// 1. `PluginRegistry` calls `connect()` at startup (and on reconnect).
/// 2. The plugin starts an internal background task that reads from the device
///    and pushes `StateChangeEvent` to the `StateBusSender` it received at
///    construction.
/// 3. `poll_state()` is called once after `connect()` to hydrate the hot cache.
/// 4. The registry calls `execute_command()` to deliver commands.
/// 5. On shutdown, `disconnect()` is called.
///
/// # Thread Safety
///
/// `Send + Sync + 'static` is required so plugins can be stored in
/// `Arc<dyn DevicePlugin>` and driven from multiple Tokio tasks.
#[async_trait]
pub trait DevicePlugin: Send + Sync + 'static {
    fn device_id(&self)    -> &DeviceId;
    fn name(&self)         -> &str;
    /// Protocol identifier string, e.g. `"tuya_local_3.3"`.
    fn protocol(&self)     -> &str;
    fn capabilities(&self) -> &[Capability];
    /// Returns `true` when the underlying transport is up and ready to send.
    /// The registry supervisor polls this to decide when to reconnect.
    fn is_connected(&self) -> bool;

    /// Establish (or re-establish) the transport connection.
    /// Must be idempotent — safe to call even if already connected.
    async fn connect(&self)    -> PluginResult<()>;
    async fn disconnect(&self);

    /// One-shot state fetch used for initial cache hydration.
    /// Steady-state updates must be pushed via the bus sender.
    async fn poll_state(&self) -> PluginResult<DeviceState>;

    /// Deliver a command to the physical device.
    async fn execute_command(&self, cmd: DeviceCommand) -> PluginResult<()>;
}

/// Type-erased plugin handle stored in the `PluginRegistry`.
pub type BoxedPlugin = Arc<dyn DevicePlugin>;
