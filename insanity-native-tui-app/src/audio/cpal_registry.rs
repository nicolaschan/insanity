use std::collections::HashSet;
use std::fmt::Debug;

use cpal::{
    Device,
    traits::{DeviceTrait, HostTrait},
};
use insanity_core::audio::device::select_by_id_name;
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

    fn try_id(&self) -> Option<String> {
        self.0.id().ok().map(|id| id.to_string())
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
    supports_direction && !is_synthetic_name(name) && !is_monitor_name(name)
}

fn raw_devices() -> Vec<Device> {
    cpal::default_host()
        .devices()
        .map(|d| d.into_iter().collect::<Vec<_>>())
        .unwrap_or_default()
}

fn filtered_devices(supports: impl Fn(&Device) -> bool) -> Vec<CpalAudioDevice> {
    let mut seen = HashSet::new();
    raw_devices()
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

// Returns (input devices, output devices)
pub fn enumerate_devices() -> (Vec<CpalAudioDevice>, Vec<CpalAudioDevice>) {
    let mut seen_inputs = HashSet::new();
    let mut seen_outputs = HashSet::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for device in raw_devices() {
        let Some(name) = device_name_opt(&device) else {
            continue;
        };
        if keep_device(&name, device.supports_input()) && seen_inputs.insert(name.clone()) {
            inputs.push(CpalAudioDevice(device.clone()));
        }
        if keep_device(&name, device.supports_output()) && seen_outputs.insert(name) {
            outputs.push(CpalAudioDevice(device));
        }
    }
    (inputs, outputs)
}

pub fn find_input_by_id_name(id: &str, name: &str) -> Option<CpalAudioDevice> {
    select_by_id_name(list_inputs(), id, name)
}

pub fn find_output_by_id_name(id: &str, name: &str) -> Option<CpalAudioDevice> {
    select_by_id_name(list_outputs(), id, name)
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

fn fallback_first(devices: Vec<CpalAudioDevice>, kind: &str) -> Option<CpalAudioDevice> {
    log::info!(
        "Available {kind} devices: {:?}",
        devices
            .iter()
            .filter_map(|d| d.try_name())
            .collect::<Vec<_>>()
    );
    devices.into_iter().next()
}

pub fn default_real_input() -> Option<CpalAudioDevice> {
    let host = cpal::default_host();
    if let Some(device) = host_default_usable(host.default_input_device(), |d| d.supports_input()) {
        return Some(device);
    }
    fallback_first(list_inputs(), "input")
}

pub fn default_real_output() -> Option<CpalAudioDevice> {
    let host = cpal::default_host();
    if let Some(device) = host_default_usable(host.default_output_device(), |d| d.supports_output())
    {
        return Some(device);
    }
    fallback_first(list_outputs(), "output")
}

pub fn default_input_device() -> Option<CpalAudioDevice> {
    default_real_input()
}

pub fn default_output_device() -> Option<CpalAudioDevice> {
    default_real_output()
}

pub struct CpalRegistry;

impl AudioDeviceRegistry<CpalAudioDevice> for CpalRegistry {
    fn list_devices() -> Vec<CpalAudioDevice> {
        filtered_devices(|device| device.supports_input() || device.supports_output())
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
