pub mod capability;
pub mod device;
pub mod plugin;

pub use capability::{Capability, DeviceCommand};
pub use device::{DeviceId, DeviceInfo};
pub use plugin::{
    BoxedPlugin, DevicePlugin, DeviceState, PluginError, PluginResult,
    StateChangeEvent, StateBusReceiver, StateBusSender,
};
