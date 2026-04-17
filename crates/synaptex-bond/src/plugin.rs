use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use synaptex_types::{
    capability::{Capability, DeviceCommand, FanSpeed},
    device::DeviceId,
    plugin::{DevicePlugin, DeviceState, PluginError, PluginResult, StateBusSender},
    DeviceInfo,
};

use tracing::{debug, info, warn};

use crate::{client::BondClient, types::BondConfig};

pub struct BondPlugin {
    info:      DeviceInfo,
    cfg:       BondConfig,
    connected: Arc<AtomicBool>,
    #[allow(dead_code)]
    bus_tx:    StateBusSender,
}

impl BondPlugin {
    pub fn new(info: DeviceInfo, cfg: BondConfig, bus_tx: StateBusSender) -> Self {
        Self {
            info,
            cfg,
            connected: Arc::new(AtomicBool::new(false)),
            bus_tx,
        }
    }

    fn client(&self) -> BondClient {
        BondClient::new(&self.cfg.hub_ip, &self.cfg.bond_token)
    }
}

#[async_trait]
impl DevicePlugin for BondPlugin {
    fn device_id(&self)    -> &DeviceId  { &self.info.id }
    fn name(&self)         -> &str       { &self.info.name }
    fn protocol(&self)     -> &str       { "bond_local" }
    fn capabilities(&self) -> &[Capability] { &self.info.capabilities }
    fn is_connected(&self) -> bool       { self.connected.load(Ordering::Relaxed) }

    async fn connect(&self) -> PluginResult<()> {
        debug!(device = %self.info.id, hub_ip = %self.cfg.hub_ip, "bond: connecting");
        self.client()
            .verify()
            .await
            .map_err(|e| {
                warn!(device = %self.info.id, hub_ip = %self.cfg.hub_ip, "bond: connect failed: {e}");
                PluginError::Unreachable(e.to_string())
            })?;
        self.connected.store(true, Ordering::Relaxed);
        info!(device = %self.info.id, hub_ip = %self.cfg.hub_ip, "bond: connected");
        Ok(())
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    async fn poll_state(&self) -> PluginResult<DeviceState> {
        let id = self.info.id;
        debug!(device = %id, bond_device = %self.cfg.bond_device_id, "bond: polling state");
        let raw = self.client()
            .get_device_state(&self.cfg.bond_device_id)
            .await
            .map_err(|e| {
                warn!(device = %id, "bond: poll_state failed: {e}");
                self.connected.store(false, Ordering::Relaxed);
                PluginError::Unreachable(e.to_string())
            })?;

        self.connected.store(true, Ordering::Relaxed);

        let has_fan   = self.info.capabilities.contains(&Capability::Fan);
        let has_light = self.info.capabilities.contains(&Capability::Light);
        debug!(device = %id, power = raw.power, light = raw.light, speed = raw.speed, "bond: raw state");

        // For fan+light devices Bond tracks the motor and light independently:
        //   raw.power = fan motor on/off
        //   raw.light = light on/off
        // DeviceState.power represents the light for fan+light devices, or the
        // motor on/off for plain fans/switches.  Fan speed reports Off when the
        // motor is stopped so the stale stored speed doesn't leak through.
        let power = if has_light {
            Some(raw.light != 0)
        } else {
            Some(raw.power != 0)
        };

        let fan_speed = if has_fan {
            Some(if raw.power == 0 {
                FanSpeed::Off
            } else {
                speed_from_bond(raw.speed, self.cfg.max_speed)
            })
        } else {
            None
        };

        Ok(DeviceState {
            device_id:        self.info.id,
            online:           true,
            updated_at_ms:    now_ms(),
            power,
            brightness:       None,
            color_temp_k:     None,
            rgb:              None,
            mode:             None,
            switches:         Default::default(),
            fan_speed,
            temp_current:     None,
            temp_set:         None,
            temp_calibration: None,
        })
    }

    async fn execute_command(&self, cmd: DeviceCommand) -> PluginResult<()> {
        let client     = self.client();
        let bond_id    = &self.cfg.bond_device_id;
        let device_id  = self.info.id;

        let (action, arg): (&str, Option<u32>) = match cmd {
            DeviceCommand::SetPower(true)  => ("TurnOn",       None),
            DeviceCommand::SetPower(false) => ("TurnOff",      None),
            DeviceCommand::SetFanSpeed(FanSpeed::Off) => ("TurnOff", None),
            DeviceCommand::SetFanSpeed(speed) => {
                let arg = speed_to_bond(speed, self.cfg.max_speed);
                ("SetSpeed", Some(arg))
            }
            DeviceCommand::SetLight { power: Some(true),  .. } => ("TurnLightOn",  None),
            DeviceCommand::SetLight { power: Some(false), .. } => ("TurnLightOff", None),
            _ => return Err(PluginError::UnsupportedCommand),
        };

        info!(device = %device_id, action, ?arg, "bond: → action");
        client.execute_action(bond_id, action, arg)
            .await
            .map_err(|e| {
                warn!(device = %device_id, action, "bond: action failed: {e}");
                PluginError::Unreachable(e.to_string())
            })
    }
}

/// Map a Bond speed value (1..=max_speed) to a FanSpeed level.
fn speed_from_bond(speed: u8, max_speed: u8) -> FanSpeed {
    let max = max_speed.max(1);
    if speed == 0 {
        FanSpeed::Off
    } else if speed <= max / 3 {
        FanSpeed::Low
    } else if speed <= (max * 2) / 3 {
        FanSpeed::Medium
    } else {
        FanSpeed::High
    }
}

/// Map a FanSpeed level to a Bond speed value (1..=max_speed).
fn speed_to_bond(speed: FanSpeed, max_speed: u8) -> u32 {
    let max = max_speed.max(1) as u32;
    match speed {
        FanSpeed::Off    => 0,
        FanSpeed::Low    => 1,
        FanSpeed::Medium => ((max + 1) / 2).max(1),
        FanSpeed::High   => max,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
