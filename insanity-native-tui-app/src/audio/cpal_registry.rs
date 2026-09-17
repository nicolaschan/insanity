use std::collections::HashSet;
use std::fmt::Debug;

use cpal::{
    Device,
    traits::{DeviceTrait, HostTrait},
};
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry, UNKNOWN_DEVICE_NAME};

const SYNTHETIC_DEVICE_NAMES: [&str; 3] = ["default_input", "default_output", "default_sink"];

pub struct CpalAudioDevice(pub Device);

impl Debug for CpalAudioDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalAudioDevice").finish()
    }
}

impl AudioDevice for CpalAudioDevice {
    fn try_name(&self) -> Option<String> {
        device_name_opt(&self.0)
    }
}

pub fn device_name(device: &Device) -> String {
    device_name_opt(device).unwrap_or_else(|| UNKNOWN_DEVICE_NAME.into())
}

fn device_name_opt(device: &Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

pub(crate) fn is_synthetic_name(name: &str) -> bool {
    SYNTHETIC_DEVICE_NAMES.contains(&name)
}

pub(crate) fn is_monitor_name(name: &str) -> bool {
    name.to_lowercase().contains("monitor")
}

pub(crate) fn keep_device(name: &str, supports_direction: bool) -> bool {
    if !supports_direction {
        return false;
    }
    if is_synthetic_name(name) {
        return false;
    }
    if is_monitor_name(name) {
        return false;
    }
    true
}

fn filtered_devices(supports: impl Fn(&Device) -> bool) -> Vec<CpalAudioDevice> {
    let devices = cpal::default_host()
        .devices()
        .map(|d| d.into_iter().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut seen = HashSet::new();
    devices
        .into_iter()
        .filter_map(|device| {
            let name = device_name_opt(&device)?;
            if !keep_device(&name, supports(&device)) {
                return None;
            }
            if !seen.insert(name) {
                return None;
            }
            Some(CpalAudioDevice(device))
        })
        .collect()
}

pub fn list_inputs() -> Vec<CpalAudioDevice> {
    filtered_devices(|device| device.supports_input())
}

pub fn list_outputs() -> Vec<CpalAudioDevice> {
    filtered_devices(|device| device.supports_output())
}

fn host_default_usable(
    device: Option<Device>,
    supports: impl Fn(&Device) -> bool,
) -> Option<CpalAudioDevice> {
    let device = device?;
    let name = device_name_opt(&device)?;
    if !keep_device(&name, supports(&device)) {
        return None;
    }
    log::info!("Using host default audio device: {name}");
    Some(CpalAudioDevice(device))
}

pub fn default_real_input() -> Option<CpalAudioDevice> {
    let host = cpal::default_host();
    if let Some(device) = host_default_usable(host.default_input_device(), |d| d.supports_input()) {
        return Some(device);
    }
    let devices = list_inputs();
    log::info!(
        "Available input devices: {:?}",
        devices
            .iter()
            .filter_map(|d| d.try_name())
            .collect::<Vec<_>>()
    );
    devices.into_iter().next()
}

pub fn default_real_output() -> Option<CpalAudioDevice> {
    let host = cpal::default_host();
    if let Some(device) = host_default_usable(host.default_output_device(), |d| d.supports_output())
    {
        return Some(device);
    }
    let devices = list_outputs();
    log::info!(
        "Available output devices: {:?}",
        devices
            .iter()
            .filter_map(|d| d.try_name())
            .collect::<Vec<_>>()
    );
    devices.into_iter().next()
}

pub struct CpalRegistry;

impl AudioDeviceRegistry<CpalAudioDevice> for CpalRegistry {
    fn list_devices() -> Vec<CpalAudioDevice> {
        let mut seen = HashSet::new();
        list_inputs()
            .into_iter()
            .chain(list_outputs())
            .filter(|device| {
                device
                    .try_name()
                    .map(|name| seen.insert(name))
                    .unwrap_or(false)
            })
            .collect()
    }

    fn default_device() -> Option<CpalAudioDevice> {
        default_real_input()
    }
}

#[cfg(test)]
mod tests {
    use super::{is_monitor_name, is_synthetic_name, keep_device};

    #[test]
    fn synthetic_defaults_are_dropped() {
        for name in ["default_input", "default_output", "default_sink"] {
            assert!(is_synthetic_name(name));
            assert!(!keep_device(name, true));
        }
        assert!(!is_synthetic_name("Logitech HD Pro Webcam C920"));
        assert!(keep_device("Logitech HD Pro Webcam C920", true));
    }

    #[test]
    fn monitors_are_dropped() {
        assert!(is_monitor_name("Monitor of Built-in Audio"));
        assert!(!keep_device("Monitor of Built-in Audio", true));
        assert!(!keep_device("monitor", true));
        assert!(keep_device("Built-in Audio", true));
    }

    #[test]
    fn unsupported_direction_is_dropped() {
        assert!(!keep_device("Logitech HD Pro Webcam C920", false));
    }
}
