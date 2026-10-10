use insanity_core::audio::AudioFormat;
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry};
use insanity_tui_adapter::AppEvent;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::audio::cpal_registry::{
    CpalAudioDevice, CpalInputDeviceRegistry, CpalOutputDeviceRegistry,
};

use super::input::InputManager;
use super::output::OutputManager;

use super::handoff::HandoffRequest;
use super::stream_errors::{FatalReporter, FatalSignal};
use insanity_core::audio::config::AudioPipelineConfig;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const DUMMY_DEVICE_NAME: &str = "dummy";

const WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const ASSUME_DEAD_THRESHOLD: u32 = 2;

#[derive(Clone, Debug)]
pub(crate) struct DeviceInfo {
    pub(crate) name: String,
    pub(crate) format: AudioFormat,
}

pub(crate) trait PayloadBuilder {
    type Payload;
    type Stats;

    fn build(
        device: CpalAudioDevice,
        config: &AudioPipelineConfig,
        stats: &Arc<Self::Stats>,
        reporter: FatalReporter,
    ) -> Option<(Self::Payload, DeviceInfo)>;

    fn build_dummy(config: &AudioPipelineConfig) -> (Self::Payload, DeviceInfo);
}

pub(crate) struct DeviceManager<Payload, Stats, DeviceRegistry, Builder> {
    config: AudioPipelineConfig,
    switch_tx: mpsc::Sender<HandoffRequest<Payload, DeviceInfo>>,
    fatal: Arc<FatalSignal>,
    stats: Arc<Stats>,
    info: watch::Receiver<DeviceInfo>,
    generation: Arc<AtomicU64>,
    _payload_builder: PhantomData<Builder>,
    _device_registry: PhantomData<DeviceRegistry>,
}

impl<Payload, Stats, DeviceRegistry, Builder> DeviceManager<Payload, Stats, DeviceRegistry, Builder>
where
    DeviceRegistry: AudioDeviceRegistry<CpalAudioDevice>,
    Builder: PayloadBuilder<Payload = Payload, Stats = Stats>,
{
    pub(crate) fn new(
        config: AudioPipelineConfig,
        switch_tx: mpsc::Sender<HandoffRequest<Payload, DeviceInfo>>,
        fatal: Arc<FatalSignal>,
        stats: Arc<Stats>,
        info: watch::Receiver<DeviceInfo>,
        generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            config,
            switch_tx,
            fatal,
            stats,
            info,
            generation,
            _payload_builder: PhantomData,
            _device_registry: PhantomData,
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<DeviceInfo> {
        self.info.clone()
    }

    pub(crate) fn current_name(&self) -> String {
        self.info.borrow().name.clone()
    }

    pub(crate) fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    pub(crate) fn fatal_signal(&self) -> Arc<FatalSignal> {
        Arc::clone(&self.fatal)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub(crate) fn switch_to(&mut self, id: &str, name: &str) {
        let Some(device) = DeviceRegistry::find(id, name) else {
            log::warn!("Requested device not found: {name}");
            return;
        };
        if !self.adopt(device) {
            self.adopt_dummy();
        }
    }

    /// Select the current default device. Does not follow changes to system default.
    // Resolves system default device name and id and then re-queries
    // the explicit device to avoid building the system default object,
    // which can reroute streams based on system default changes.
    pub(crate) fn select_current_default(&mut self) {
        let Some(default_device) = DeviceRegistry::default_device() else {
            log::warn!("No device available, falling back to dummy");
            self.adopt_dummy();
            return;
        };
        let (Some(id), Some(name)) = (default_device.try_id(), default_device.try_name()) else {
            log::warn!("Failed to get default device id & name, falling back to dummy");
            self.adopt_dummy();
            return;
        };
        let Some(device) = DeviceRegistry::find(&id, &name) else {
            log::warn!("Default device {name} not in enumeration, falling back to dummy");
            self.adopt_dummy();
            return;
        };
        if !self.adopt(device) {
            log::warn!("Failed to adopt device, falling back to dummy");
            self.adopt_dummy();
        }
    }

    fn adopt(&mut self, device: CpalAudioDevice) -> bool {
        let next = self.generation.load(Ordering::Relaxed) + 1;
        let reporter = FatalReporter::new(Arc::clone(&self.fatal), next);
        if let Some((payload, info)) = Builder::build(device, &self.config, &self.stats, reporter)
            && self.send(payload, info, next)
        {
            self.generation.store(next, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub(crate) fn adopt_dummy(&mut self) {
        let (payload, info) = Builder::build_dummy(&self.config);
        let next = self.generation.load(Ordering::Relaxed) + 1;
        if self.send(payload, info, next) {
            self.generation.store(next, Ordering::Relaxed);
        }
    }

    fn send(&self, payload: Payload, info: DeviceInfo, generation: u64) -> bool {
        let name = info.name.clone();
        let request = HandoffRequest {
            payload,
            info,
            generation,
        };
        if self.switch_tx.try_send(request).is_err() {
            log::warn!("Switch channel full, dropping switch to {name}");
            false
        } else {
            true
        }
    }
}

impl<Payload, Stats, DeviceRegistry, Builder> Clone
    for DeviceManager<Payload, Stats, DeviceRegistry, Builder>
{
    fn clone(&self) -> Self {
        Self {
            config: self.config,
            switch_tx: self.switch_tx.clone(),
            fatal: Arc::clone(&self.fatal),
            stats: Arc::clone(&self.stats),
            info: self.info.clone(),
            generation: Arc::clone(&self.generation),
            _payload_builder: PhantomData,
            _device_registry: PhantomData,
        }
    }
}
type DeviceList = Vec<(String, String)>;

fn device_lists() -> (DeviceList, DeviceList) {
    (
        CpalInputDeviceRegistry::list_devices()
            .iter()
            .filter_map(|device| Some((device.try_id()?, device.try_name()?)))
            .collect(),
        CpalOutputDeviceRegistry::list_devices()
            .iter()
            .filter_map(|device| Some((device.try_id()?, device.try_name()?)))
            .collect(),
    )
}

pub(crate) fn refresh_device_events(input: &InputManager, output: &OutputManager) -> Vec<AppEvent> {
    let (inputs, outputs) = device_lists();
    vec![
        AppEvent::SetInputDevices(inputs),
        AppEvent::SetOutputDevices(outputs),
        AppEvent::SetInputDeviceName(input.current_name()),
        AppEvent::SetOutputDeviceName(output.current_name()),
    ]
}

fn send_refresh(
    app_event_tx: &Option<mpsc::UnboundedSender<AppEvent>>,
    input: &InputManager,
    output: &OutputManager,
) {
    let Some(tx) = app_event_tx else {
        return;
    };
    for event in refresh_device_events(input, output) {
        if tx.send(event).is_err() {
            log::warn!("Could not send device refresh to UI");
            break;
        }
    }
}

/// Track lack of callbacks.
struct DeviceLiveness {
    generation: u64,
    count: u64,
    misses: u32,
}

impl DeviceLiveness {
    fn new(generation: u64, count: u64) -> Self {
        DeviceLiveness {
            generation,
            count,
            misses: 0,
        }
    }

    /// True when current device is not dummy and count has skipped consecutive ticks.
    fn poll_check_dead(&mut self, generation: u64, current_name: &str, count: u64) -> bool {
        if generation != self.generation {
            self.generation = generation;
            self.count = count;
            self.misses = 0;
            return false;
        }
        if current_name == DUMMY_DEVICE_NAME {
            self.count = count;
            self.misses = 0;
            return false;
        }
        if count == self.count {
            self.misses += 1;
        } else {
            self.count = count;
            self.misses = 0;
        }
        self.misses >= ASSUME_DEAD_THRESHOLD
    }
}

pub(crate) async fn run_device_supervisor(
    mut input: InputManager,
    mut output: OutputManager,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    cancel: CancellationToken,
) {
    let input_fatal = input.fatal_signal();
    let output_fatal = output.fatal_signal();
    let mut liveness_tick = tokio::time::interval(WATCHDOG_INTERVAL);
    liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut input_liveness =
        DeviceLiveness::new(input.generation(), input.stats().data_callbacks());
    let mut output_liveness =
        DeviceLiveness::new(output.generation(), output.stats().data_callbacks());
    loop {
        tokio::select! {
            _ = input_fatal.notified() => {
                if input_fatal.generation() == input.generation() {
                    log::info!("Input stream failed, falling back to dummy device");
                    input.adopt_dummy();
                    send_refresh(&app_event_tx, &input, &output);
                } else {
                    log::debug!(
                        "Ignoring stale input failure for generation {}",
                        input_fatal.generation()
                    );
                }
            }
            _ = output_fatal.notified() => {
                if output_fatal.generation() == output.generation() {
                    log::info!("Output stream failed, falling back to dummy device");
                    output.adopt_dummy();
                    send_refresh(&app_event_tx, &input, &output);
                } else {
                    log::debug!(
                        "Ignoring stale output failure for generation {}",
                        output_fatal.generation()
                    );
                }
            }
            _ = liveness_tick.tick() => {
                if input_liveness.poll_check_dead(
                    input.generation(),
                    &input.current_name(),
                    input.stats().data_callbacks(),
                ) {
                    log::warn!("Input stalled (no data callbacks), falling back to dummy device");
                    input.adopt_dummy();
                }
                if output_liveness.poll_check_dead(
                    output.generation(),
                    &output.current_name(),
                    output.stats().data_callbacks(),
                ) {
                    log::warn!("Output stalled (no data callbacks), falling back to dummy device");
                    output.adopt_dummy();
                }
                send_refresh(&app_event_tx, &input, &output);
            }
            _ = cancel.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{refresh_device_events, run_device_supervisor};
    use insanity_core::audio::config::AudioPipelineConfig;
    use insanity_tui_adapter::AppEvent;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::super::input::{InputManager, start_input};
    use super::super::output::{OutputManager, start_output};

    fn managers() -> (InputManager, OutputManager) {
        let config = AudioPipelineConfig::default();
        (start_input(config).manager, start_output(config).manager)
    }

    #[tokio::test]
    async fn refresh_lists_devices_then_names() {
        let (input, output) = managers();
        let events = refresh_device_events(&input, &output);
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], AppEvent::SetInputDevices(_)));
        assert!(matches!(events[1], AppEvent::SetOutputDevices(_)));
        let AppEvent::SetInputDeviceName(input_name) = &events[2] else {
            panic!("expected input name third, got {:?}", events[2]);
        };
        let AppEvent::SetOutputDeviceName(output_name) = &events[3] else {
            panic!("expected output name fourth, got {:?}", events[3]);
        };
        assert_eq!(*input_name, input.current_name());
        assert_eq!(*output_name, output.current_name());
    }

    #[tokio::test]
    async fn fresh_fatal_rebuilds_and_refreshes_ui() {
        let (input, output) = managers();
        input.fatal_signal().signal(input.generation());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_device_supervisor(
            input,
            output,
            Some(tx),
            cancel.clone(),
        ));
        let mut events = Vec::new();
        for _ in 0..4 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("supervisor refreshes UI after fresh fatal")
                .expect("sender alive");
            events.push(event);
        }
        cancel.cancel();
        task.await.expect("supervisor shuts down");
        assert!(matches!(events[0], AppEvent::SetInputDevices(_)));
        assert!(matches!(events[1], AppEvent::SetOutputDevices(_)));
        let AppEvent::SetInputDeviceName(input_name) = &events[2] else {
            panic!("expected input name third, got {:?}", events[2]);
        };
        let AppEvent::SetOutputDeviceName(output_name) = &events[3] else {
            panic!("expected output name fourth, got {:?}", events[3]);
        };
        assert!(!input_name.is_empty());
        assert!(!output_name.is_empty());
    }
}
