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
        self.client()
            .verify()
            .await
            .map_err(|e| PluginError::Unreachable(e.to_string()))?;
        self.connected.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn disconnect(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    async fn poll_state(&self) -> PluginResult<DeviceState> {
        let raw = self.client()
            .get_device_state(&self.cfg.bond_device_id)
            .await
            .map_err(|e| PluginError::Unreachable(e.to_string()))?;

        self.connected.store(true, Ordering::Relaxed);

        let has_fan = self.info.capabilities.contains(&Capability::Fan);
        let fan_speed = if has_fan {
            Some(match raw.speed {
                0     => FanSpeed::Off,
                1 | 2 => FanSpeed::Low,
                3 | 4 => FanSpeed::Medium,
                _     => FanSpeed::High,
            })
        } else {
            None
        };

        Ok(DeviceState {
            device_id:        self.info.id,
            online:           true,
            updated_at_ms:    now_ms(),
            power:            Some(raw.power != 0),
            brightness:       None,
            color_temp_k:     None,
            rgb:              None,
            switches:         Default::default(),
            fan_speed,
            temp_current:     None,
            temp_set:         None,
            temp_calibration: None,
        })
    }

    async fn execute_command(&self, cmd: DeviceCommand) -> PluginResult<()> {
        let client = self.client();
        let id     = &self.cfg.bond_device_id;

        let result = match cmd {
            DeviceCommand::SetPower(true) => {
                client.execute_action(id, "TurnOn", None).await
            }
            DeviceCommand::SetPower(false) => {
                client.execute_action(id, "TurnOff", None).await
            }
            DeviceCommand::SetFanSpeed(speed) => match speed {
                FanSpeed::Off => {
                    client.execute_action(id, "TurnOff", None).await
                }
                FanSpeed::Low    => client.execute_action(id, "SetSpeed", Some(2)).await,
                FanSpeed::Medium => client.execute_action(id, "SetSpeed", Some(4)).await,
                FanSpeed::High   => client.execute_action(id, "SetSpeed", Some(6)).await,
            },
            DeviceCommand::SetLight { power: Some(true), .. } => {
                client.execute_action(id, "TurnLightOn", None).await
            }
            DeviceCommand::SetLight { power: Some(false), .. } => {
                client.execute_action(id, "TurnLightOff", None).await
            }
            _ => return Err(PluginError::UnsupportedCommand),
        };

        result.map_err(|e| PluginError::Unreachable(e.to_string()))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
