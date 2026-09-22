use std::fmt::Debug;

use cpal::{
    Device,
    traits::{DeviceTrait, HostTrait},
};
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry, UNKNOWN_DEVICE_NAME};

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

fn filtered_devices(supports: impl Fn(&Device) -> bool) -> Vec<CpalAudioDevice> {
    cpal::default_host()
        .devices()
        .map(|devices| {
            devices
                .into_iter()
                .filter(|device| supports(device))
                .map(CpalAudioDevice)
                .collect()
        })
        .unwrap_or_default()
}

pub fn list_inputs() -> Vec<CpalAudioDevice> {
    filtered_devices(|device| device.supports_input())
}

pub fn list_outputs() -> Vec<CpalAudioDevice> {
    filtered_devices(|device| device.supports_output())
}

fn find_by_id(devices: Vec<CpalAudioDevice>, id: &str) -> Option<CpalAudioDevice> {
    devices
        .into_iter()
        .find(|d| d.try_id().as_deref() == Some(id))
}

pub fn find_input_by_id(id: &str) -> Option<CpalAudioDevice> {
    find_by_id(list_inputs(), id)
}

pub fn find_output_by_id(id: &str) -> Option<CpalAudioDevice> {
    find_by_id(list_outputs(), id)
}

pub fn default_input_device() -> Option<CpalAudioDevice> {
    cpal::default_host()
        .default_input_device()
        .map(CpalAudioDevice)
}

pub fn default_output_device() -> Option<CpalAudioDevice> {
    cpal::default_host()
        .default_output_device()
        .map(CpalAudioDevice)
}

pub fn default_input_device_id() -> Option<String> {
    default_input_device().and_then(|device| device.try_id())
}

pub fn default_output_device_id() -> Option<String> {
    default_output_device().and_then(|device| device.try_id())
}

pub struct CpalRegistry;

impl AudioDeviceRegistry<CpalAudioDevice> for CpalRegistry {
    fn list_devices() -> Vec<CpalAudioDevice> {
        filtered_devices(|device| device.supports_input() || device.supports_output())
    }

    fn default_device() -> Option<CpalAudioDevice> {
        default_input_device()
    }
}
