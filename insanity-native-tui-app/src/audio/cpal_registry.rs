use cpal::{
    Device,
    traits::{DeviceTrait, HostTrait},
};

pub(crate) fn shared_host() -> &'static cpal::Host {
    static HOST: std::sync::OnceLock<cpal::Host> = std::sync::OnceLock::new();
    HOST.get_or_init(cpal::default_host)
}

pub(crate) fn device_name(device: &Device) -> String {
    device
        .description()
        .ok()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|| "unknown device".into())
}

pub(crate) fn default_input_device() -> Option<Device> {
    shared_host().default_input_device()
}

pub(crate) fn default_output_device() -> Option<Device> {
    shared_host().default_output_device()
}
