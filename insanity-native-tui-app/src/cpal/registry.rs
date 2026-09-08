use std::fmt::Debug;

use cpal::{
    Device,
    traits::{DeviceTrait, HostTrait},
};
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry};

pub struct CpalAudioDevice(pub Device);

impl Debug for CpalAudioDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalAudioDevice").finish()
    }
}

impl AudioDevice for CpalAudioDevice {
    fn try_name(&self) -> Option<String> {
        self.0.description().ok().map(|d| d.name().to_string())
    }
}

pub struct CpalRegistry;

impl AudioDeviceRegistry<CpalAudioDevice> for CpalRegistry {
    fn list_devices() -> Vec<CpalAudioDevice> {
        cpal::default_host()
            .devices()
            .map(|d| d.into_iter().collect())
            .unwrap_or(vec![])
            .into_iter()
            .map(CpalAudioDevice)
            .collect()
    }

    fn default_device() -> Option<CpalAudioDevice> {
        let host = cpal::default_host();
        host.default_input_device().map(CpalAudioDevice)
    }
}
