use std::collections::HashSet;

use anyhow::Context;
use cpal::{
    Device,
    traits::{DeviceTrait, HostTrait},
};

pub(crate) const UNKNOWN_DEVICE_NAME: &str = "unknown device";

const SYNTHETIC_DEVICE_NAMES: [&str; 3] = ["default_input", "default_output", "default_sink"];

pub(crate) fn shared_host() -> &'static cpal::Host {
    static HOST: std::sync::OnceLock<cpal::Host> = std::sync::OnceLock::new();
    HOST.get_or_init(cpal::default_host)
}

fn device_name_opt(device: &Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_owned())
}

pub(crate) fn device_name(device: &Device) -> String {
    device_name_opt(device).unwrap_or_else(|| UNKNOWN_DEVICE_NAME.into())
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

fn first_usable_device(devices: impl IntoIterator<Item = Device>) -> Option<Device> {
    let mut seen = HashSet::new();
    devices.into_iter().find(|device| {
        let Some(name) = device_name_opt(device) else {
            return false;
        };
        keep_device(&name) && seen.insert(name)
    })
}

fn log_fallback(direction: &str, err: &anyhow::Error, fallback: Option<&Device>) {
    log::debug!(
        "No usable {direction} default ({err:#} fallback={}); falling back to first device",
        fallback
            .and_then(device_name_opt)
            .as_deref()
            .unwrap_or("none"),
    );
}

fn try_default_input() -> anyhow::Result<Device> {
    let device = shared_host()
        .default_input_device()
        .context("no system default")?;
    let name = device_name_opt(&device).context("default has no name")?;
    anyhow::ensure!(
        device.supports_input(),
        "default {name} does not support input"
    );
    anyhow::ensure!(keep_device(&name), "default {name} filtered");
    Ok(device)
}

pub(crate) fn default_input_device() -> Option<Device> {
    match try_default_input() {
        Ok(device) => Some(device),
        Err(err) => {
            let fallback = shared_host()
                .input_devices()
                .ok()
                .and_then(first_usable_device);
            log_fallback("input", &err, fallback.as_ref());
            fallback
        }
    }
}

fn try_default_output() -> anyhow::Result<Device> {
    let device = shared_host()
        .default_output_device()
        .context("no system default")?;
    let name = device_name_opt(&device).context("default has no name")?;
    anyhow::ensure!(
        device.supports_output(),
        "default {name} does not support output"
    );
    anyhow::ensure!(keep_device(&name), "default {name} filtered");
    Ok(device)
}

pub(crate) fn default_output_device() -> Option<Device> {
    match try_default_output() {
        Ok(device) => Some(device),
        Err(err) => {
            let fallback = shared_host()
                .output_devices()
                .ok()
                .and_then(first_usable_device);
            log_fallback("output", &err, fallback.as_ref());
            fallback
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
