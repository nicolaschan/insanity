use std::collections::HashSet;
use std::fmt::Debug;

use cpal::{
    Device,
    traits::{DeviceTrait, HostTrait},
};
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry};

const SYNTHETIC_DEVICE_NAMES: [&str; 3] = ["default_input", "default_output", "default_sink"];

pub(crate) struct CpalAudioDevice(pub(crate) Device);

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

fn device_name_opt(device: &Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

fn is_synthetic_name(name: &str) -> bool {
    SYNTHETIC_DEVICE_NAMES.contains(&name)
}

fn is_monitor_name(name: &str) -> bool {
    name.to_lowercase().contains("monitor")
}

fn keep_device(name: &str) -> bool {
    !is_synthetic_name(name) && !is_monitor_name(name)
}

pub(crate) fn shared_host() -> &'static cpal::Host {
    static HOST: std::sync::OnceLock<cpal::Host> = std::sync::OnceLock::new();
    HOST.get_or_init(cpal::default_host)
}

fn filtered_devices(devices: impl IntoIterator<Item = Device>) -> Vec<CpalAudioDevice> {
    let mut seen = HashSet::new();
    devices
        .into_iter()
        .map(CpalAudioDevice)
        .filter_map(|device| {
            let name = device.try_name()?;
            let id = device.try_id()?;
            if !keep_device(&name) || !seen.insert((name, id)) {
                None
            } else {
                Some(device)
            }
        })
        .collect()
}

pub(crate) struct CpalInputDeviceRegistry;
pub(crate) struct CpalOutputDeviceRegistry;

impl AudioDeviceRegistry<CpalAudioDevice> for CpalInputDeviceRegistry {
    fn list_devices() -> Vec<CpalAudioDevice> {
        match shared_host().input_devices() {
            Ok(devices) => filtered_devices(devices),
            Err(error) => {
                log::warn!("Failed to enumerate input devices: {error}");
                Vec::new()
            }
        }
    }

    fn default_device() -> Option<CpalAudioDevice> {
        if let Some(device) = shared_host().default_input_device()
            && let Some(name) = device_name_opt(&device)
            && device.supports_input()
            && keep_device(&name)
        {
            Some(CpalAudioDevice(device))
        } else {
            Self::list_devices().into_iter().next()
        }
    }
}

impl AudioDeviceRegistry<CpalAudioDevice> for CpalOutputDeviceRegistry {
    fn list_devices() -> Vec<CpalAudioDevice> {
        match shared_host().output_devices() {
            Ok(devices) => filtered_devices(devices),
            Err(error) => {
                log::warn!("Failed to enumerate output devices: {error}");
                Vec::new()
            }
        }
    }

    fn default_device() -> Option<CpalAudioDevice> {
        if let Some(device) = shared_host().default_output_device()
            && let Some(name) = device_name_opt(&device)
            && device.supports_output()
            && keep_device(&name)
        {
            Some(CpalAudioDevice(device))
        } else {
            Self::list_devices().into_iter().next()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_monitor_name, is_synthetic_name, keep_device};

    #[test]
    fn synthetic_defaults_are_dropped() {
        for name in ["default_input", "default_output", "default_sink"] {
            assert!(is_synthetic_name(name));
            assert!(!keep_device(name));
        }
        assert!(!is_synthetic_name("Logitech HD Pro Webcam C920"));
        assert!(keep_device("Logitech HD Pro Webcam C920"));
    }

    #[test]
    fn monitors_are_dropped() {
        assert!(is_monitor_name("Monitor of Built-in Audio"));
        assert!(!keep_device("Monitor of Built-in Audio"));
        assert!(!keep_device("monitor"));
        assert!(keep_device("Built-in Audio"));
    }
}
