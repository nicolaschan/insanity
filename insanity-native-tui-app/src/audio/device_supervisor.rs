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

pub(crate) const DEFAULT_DEVICE_ID: &str = "default";
pub(crate) const DEFAULT_DEVICE_NAME: &str = "default";
pub(crate) const DUMMY_DEVICE_NAME: &str = "dummy";

const STALL_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
struct StallTracker {
    last_count: u64,
    last_change: std::time::Instant,
    armed: bool,
}

impl StallTracker {
    fn new(now: std::time::Instant) -> Self {
        Self {
            last_count: 0,
            last_change: now,
            armed: false,
        }
    }

    fn observe(&mut self, count: u64, now: std::time::Instant) {
        if count != self.last_count {
            self.last_count = count;
            self.last_change = now;
            self.armed = true;
        }
    }

    fn stalled(&self, now: std::time::Instant) -> bool {
        self.armed && now.duration_since(self.last_change) > STALL_TIMEOUT
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Selection {
    FollowDefault,
    Explicit { name: String },
    Dummy,
}

impl Selection {
    pub(crate) fn display_name(&self) -> String {
        match self {
            Selection::FollowDefault => DEFAULT_DEVICE_NAME.to_string(),
            Selection::Explicit { name } => name.clone(),
            Selection::Dummy => DUMMY_DEVICE_NAME.to_string(),
        }
    }
}

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
    selection: watch::Sender<Selection>,
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
        generation: Arc<AtomicU64>,
    ) -> Self {
        let (selection, _) = watch::channel(Selection::FollowDefault);
        Self {
            config,
            switch_tx,
            fatal,
            stats,
            selection,
            generation,
            _payload_builder: PhantomData,
            _device_registry: PhantomData,
        }
    }

    pub(crate) fn subscribe_selection(&self) -> watch::Receiver<Selection> {
        self.selection.subscribe()
    }

    pub(crate) fn current_name(&self) -> String {
        self.selection.borrow().display_name()
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
        if self.adopt(device) {
            self.selection.send_replace(Selection::Explicit {
                name: name.to_string(),
            });
        }
    }

    pub(crate) fn follow_default(&mut self) {
        let Some(device) = DeviceRegistry::default_device() else {
            self.fallback_dummy();
            return;
        };
        if self.adopt(device) {
            self.selection.send_replace(Selection::FollowDefault);
        } else {
            self.fallback_dummy();
        }
    }

    fn fallback_dummy(&mut self) {
        log::warn!("No device available, falling back to dummy");
        self.adopt_dummy();
        self.selection.send_replace(Selection::Dummy);
    }

    pub(crate) fn recover_if_dummy(&mut self) {
        if matches!(*self.selection.borrow(), Selection::Dummy) {
            self.follow_default();
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
            selection: self.selection.clone(),
            generation: Arc::clone(&self.generation),
            _payload_builder: PhantomData,
            _device_registry: PhantomData,
        }
    }
}
type DeviceList = Vec<(String, String)>;

fn default_entry() -> (String, String) {
    (
        DEFAULT_DEVICE_ID.to_string(),
        DEFAULT_DEVICE_NAME.to_string(),
    )
}

fn device_lists() -> (DeviceList, DeviceList) {
    let mut inputs: DeviceList = CpalInputDeviceRegistry::list_devices()
        .iter()
        .filter_map(|device| Some((device.try_id()?, device.try_name()?)))
        .collect();
    let mut outputs: DeviceList = CpalOutputDeviceRegistry::list_devices()
        .iter()
        .filter_map(|device| Some((device.try_id()?, device.try_name()?)))
        .collect();
    if inputs.is_empty() {
        inputs.push((DEFAULT_DEVICE_ID.to_string(), DUMMY_DEVICE_NAME.into()));
    }
    if outputs.is_empty() {
        outputs.push((DEFAULT_DEVICE_ID.to_string(), DUMMY_DEVICE_NAME.into()));
    }
    inputs.insert(0, default_entry());
    outputs.insert(0, default_entry());
    (inputs, outputs)
}

fn default_device_names() -> (String, String) {
    (
        CpalInputDeviceRegistry::default_device()
            .and_then(|device| device.try_name())
            .unwrap_or_default(),
        CpalOutputDeviceRegistry::default_device()
            .and_then(|device| device.try_name())
            .unwrap_or_default(),
    )
}

pub(crate) fn refresh_device_events(input: &InputManager, output: &OutputManager) -> Vec<AppEvent> {
    let (inputs, outputs) = device_lists();
    let (default_input, default_output) = default_device_names();
    vec![
        AppEvent::SetInputDevices(inputs),
        AppEvent::SetOutputDevices(outputs),
        AppEvent::SetInputDeviceName(input.current_name()),
        AppEvent::SetOutputDeviceName(output.current_name()),
        AppEvent::SetDefaultInputDeviceName(default_input),
        AppEvent::SetDefaultOutputDeviceName(default_output),
    ]
}

fn send_current_names(
    app_event_tx: &Option<mpsc::UnboundedSender<AppEvent>>,
    input: &InputManager,
    output: &OutputManager,
) {
    if let Some(tx) = app_event_tx {
        let _ = tx.send(AppEvent::SetInputDeviceName(input.current_name()));
        let _ = tx.send(AppEvent::SetOutputDeviceName(output.current_name()));
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
    let input_stats = input.stats();
    let output_stats = output.stats();
    let start = std::time::Instant::now();
    let mut input_stall = StallTracker::new(start);
    let mut output_stall = StallTracker::new(start);
    let mut stall_check = tokio::time::interval(STALL_CHECK_INTERVAL);
    loop {
        tokio::select! {
            _ = input_fatal.notified() => {
                if input_fatal.generation() == input.generation() {
                    log::info!("Input stream failed, following default device");
                    input.follow_default();
                    send_current_names(&app_event_tx, &input, &output);
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
                    send_current_names(&app_event_tx, &input, &output);
                } else {
                    log::debug!(
                        "Ignoring stale output failure for generation {}",
                        output_fatal.generation()
                    );
                }
            }
            _ = stall_check.tick() => {
                let now = std::time::Instant::now();
                input_stall.observe(input_stats.fills(), now);
                output_stall.observe(output_stats.fills() as u64, now);
                if input_stall.stalled(now)
                    && !matches!(*input.selection.borrow(), Selection::Dummy)
                {
                    log::warn!("Input stream stalled, following default device");
                    input_fatal.signal(input.generation());
                }
                if output_stall.stalled(now)
                    && !matches!(*output.selection.borrow(), Selection::Dummy)
                {
                    log::warn!("Output stream stalled, following default device");
                    output_fatal.signal(output.generation());
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_DEVICE_ID, DEFAULT_DEVICE_NAME, DUMMY_DEVICE_NAME, STALL_TIMEOUT, Selection,
        StallTracker, refresh_device_events, run_device_supervisor,
    };
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

    #[test]
    fn selection_display_names() {
        assert_eq!(Selection::FollowDefault.display_name(), DEFAULT_DEVICE_NAME);
        assert_eq!(Selection::Dummy.display_name(), DUMMY_DEVICE_NAME);
        assert_eq!(
            Selection::Explicit {
                name: "Mic".to_string(),
            }
            .display_name(),
            "Mic"
        );
    }

    #[test]
    fn stall_tracker_fires_only_after_armed_timeout() {
        let start = std::time::Instant::now();
        let mut tracker = StallTracker::new(start);
        assert!(!tracker.stalled(start + STALL_TIMEOUT + std::time::Duration::from_secs(60)));
        tracker.observe(7, start);
        assert!(!tracker.stalled(start + STALL_TIMEOUT));
        assert!(tracker.stalled(start + STALL_TIMEOUT + std::time::Duration::from_millis(1)));
        tracker.observe(8, start + STALL_TIMEOUT);
        assert!(
            !tracker.stalled(
                start + STALL_TIMEOUT + STALL_TIMEOUT - std::time::Duration::from_millis(1)
            )
        );
        assert!(
            tracker.stalled(
                start + STALL_TIMEOUT + STALL_TIMEOUT + std::time::Duration::from_millis(1)
            )
        );
    }

    #[tokio::test]
    async fn recover_if_dummy_leaves_live_selections_alone() {
        let (mut input, mut output) = managers();
        input.recover_if_dummy();
        output.recover_if_dummy();
        for name in [input.current_name(), output.current_name()] {
            assert!(
                name == DEFAULT_DEVICE_NAME || name == DUMMY_DEVICE_NAME,
                "unexpected selection after recover: {name}"
            );
        }
    }

    #[tokio::test]
    async fn managers_start_following_default_or_dummy() {
        let (input, output) = managers();
        for name in [input.current_name(), output.current_name()] {
            assert!(
                name == DEFAULT_DEVICE_NAME || name == DUMMY_DEVICE_NAME,
                "unexpected initial selection: {name}"
            );
        }
    }

    #[tokio::test]
    async fn refresh_lists_devices_then_names_and_defaults() {
        let (input, output) = managers();
        let events = refresh_device_events(&input, &output);
        assert_eq!(events.len(), 6);
        let AppEvent::SetInputDevices(inputs) = &events[0] else {
            panic!("expected input list first, got {:?}", events[0]);
        };
        let AppEvent::SetOutputDevices(outputs) = &events[1] else {
            panic!("expected output list second, got {:?}", events[1]);
        };
        for list in [inputs, outputs] {
            assert!(!list.is_empty(), "empty list renders dummy");
            assert_eq!(
                list.first(),
                Some(&(
                    DEFAULT_DEVICE_ID.to_string(),
                    DEFAULT_DEVICE_NAME.to_string()
                )),
                "lists start with default"
            );
        }
        let AppEvent::SetInputDeviceName(input_name) = &events[2] else {
            panic!("expected input name third, got {:?}", events[2]);
        };
        let AppEvent::SetOutputDeviceName(output_name) = &events[3] else {
            panic!("expected output name fourth, got {:?}", events[3]);
        };
        assert_eq!(*input_name, input.current_name());
        assert_eq!(*output_name, output.current_name());
        assert!(
            matches!(events[4], AppEvent::SetDefaultInputDeviceName(_)),
            "expected input default fifth, got {:?}",
            events[4]
        );
        assert!(
            matches!(events[5], AppEvent::SetDefaultOutputDeviceName(_)),
            "expected output default sixth, got {:?}",
            events[5]
        );
    }

    #[tokio::test]
    async fn unknown_device_keeps_selection() {
        let (mut input, _) = managers();
        let before = input.current_name();
        input.switch_to("no-such-id", "No Such Device");
        assert_eq!(input.current_name(), before);
        input.follow_default();
        let after = input.current_name();
        assert!(
            after == DEFAULT_DEVICE_NAME || after == DUMMY_DEVICE_NAME,
            "unexpected selection after follow_default: {after}"
        );
    }

    #[tokio::test]
    async fn fresh_fatal_resends_names() {
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
        for _ in 0..2 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("supervisor resends names after fresh fatal")
                .expect("sender alive");
            events.push(event);
        }
        cancel.cancel();
        task.await.expect("supervisor shuts down");
        let AppEvent::SetInputDeviceName(input_name) = &events[0] else {
            panic!("expected input name first, got {:?}", events[0]);
        };
        let AppEvent::SetOutputDeviceName(output_name) = &events[1] else {
            panic!("expected output name second, got {:?}", events[1]);
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
