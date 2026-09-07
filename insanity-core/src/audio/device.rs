use std::fmt::Debug;

pub const UNKNOWN_DEVICE_NAME: &str = "unknown device";

pub trait AudioDevice: Debug + Send + Sync {
    fn try_name(&self) -> Option<String>;

    fn name(&self) -> String {
        self.try_name().unwrap_or(UNKNOWN_DEVICE_NAME.into())
    }
}

pub trait AudioDeviceRegistry<DeviceT: AudioDevice> {
    fn list_devices() -> Vec<DeviceT>;
    fn default_device() -> Option<DeviceT>;
}
