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
            log::warn!("Requested output device not found: {name}");
            return;
        };
        self.adopt(device);
    }

    pub(crate) fn follow_default(&mut self) {
        let Some(device) = DeviceRegistry::default_device() else {
            log::warn!("No output device available, falling back to dummy");
            self.adopt_dummy();
            return;
        };
        if !self.adopt(device) {
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

    fn adopt_dummy(&mut self) {
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

pub(crate) async fn run_device_supervisor(
    mut input: InputManager,
    mut output: OutputManager,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    cancel: CancellationToken,
) {
    let input_fatal = input.fatal_signal();
    let output_fatal = output.fatal_signal();
    loop {
        tokio::select! {
            _ = input_fatal.notified() => {
                if input_fatal.generation() == input.generation() {
                    log::info!("Input stream failed, following default device");
                    input.follow_default();
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
                    log::info!("Output stream failed, following default device");
                    output.follow_default();
                    send_refresh(&app_event_tx, &input, &output);
                } else {
                    log::debug!(
                        "Ignoring stale output failure for generation {}",
                        output_fatal.generation()
                    );
                }
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

    #[tokio::test]
    async fn stale_fatal_sends_no_events() {
        let (input, output) = managers();
        input
            .fatal_signal()
            .signal(input.generation().wrapping_add(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_device_supervisor(
            input,
            output,
            Some(tx),
            cancel.clone(),
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
        cancel.cancel();
        task.await.expect("supervisor shuts down");
    }
}
