use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use crossbeam_utils::atomic::AtomicCell;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::handoff::HandoffRequest;
use super::input::InputManager;
use super::output::OutputManager;
use super::stream_errors::{FatalReporter, FatalSignal};
use crate::audio::cpal_registry::{
    CpalAudioDevice, CpalInputDeviceRegistry, CpalOutputDeviceRegistry,
};
use insanity_core::audio::AudioFormat;
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry};
use insanity_tui_adapter::AppEvent;

pub(crate) const DUMMY_DEVICE_NAME: &str = "dummy";

const WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Grace period after generation switch
const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// Steady-state stall deadline = this multiple of the callback period (clamped)
const STALL_PERIOD_MULT: u32 = 3;
const MIN_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const MAX_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const FALLBACK_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

fn stall_timeout_for_period(period: Option<std::time::Duration>) -> std::time::Duration {
    match period {
        None => MIN_STALL_TIMEOUT,
        Some(p) => (p * STALL_PERIOD_MULT).clamp(MIN_STALL_TIMEOUT, MAX_STALL_TIMEOUT),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum FallbackMode {
    Auto = 0,
    Explicit = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FallbackDecision {
    RetryDefault,
    ParkOnDummy,
    DoNothing,
}

struct FallbackPolicy {
    last_default_attempt: Option<std::time::Instant>,
    min_interval: std::time::Duration,
}

impl FallbackPolicy {
    fn new(min_interval: std::time::Duration) -> Self {
        FallbackPolicy {
            last_default_attempt: None,
            min_interval,
        }
    }

    fn on_device_lost(
        &mut self,
        now: std::time::Instant,
        mode: FallbackMode,
        is_live: bool,
    ) -> FallbackDecision {
        if !is_live {
            return FallbackDecision::DoNothing;
        }
        match mode {
            FallbackMode::Explicit => FallbackDecision::ParkOnDummy,
            FallbackMode::Auto => {
                if let Some(attempted) = self.last_default_attempt
                    && now.saturating_duration_since(attempted) < self.min_interval
                {
                    FallbackDecision::DoNothing
                } else {
                    self.last_default_attempt = Some(now);
                    FallbackDecision::RetryDefault
                }
            }
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
    ) -> anyhow::Result<(Self::Payload, DeviceInfo)>;

    fn build_dummy(config: &AudioPipelineConfig) -> (Self::Payload, DeviceInfo);
}

/// A `Clone`d handle for switching the current audio device.
/// Switches must be serialized by callers: concurrent switch calls are not supported.
pub(crate) struct DeviceManager<Payload, Stats, DeviceRegistry, Builder> {
    config: AudioPipelineConfig,
    switch_tx: mpsc::Sender<HandoffRequest<Payload, DeviceInfo>>,
    fatal: Arc<FatalSignal>,
    stats: Arc<Stats>,
    info: watch::Receiver<DeviceInfo>,
    generation: Arc<AtomicU64>,
    fallback_mode: Arc<AtomicCell<FallbackMode>>,
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
            fallback_mode: Arc::new(AtomicCell::new(FallbackMode::Auto)),
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

    pub(crate) fn pipeline_config(&self) -> AudioPipelineConfig {
        self.config
    }

    pub(crate) fn mode(&self) -> FallbackMode {
        self.fallback_mode.load()
    }

    pub(crate) fn set_mode(&self, mode: FallbackMode) {
        self.fallback_mode.store(mode);
    }

    pub(crate) fn is_live(&self) -> bool {
        self.info.borrow().name != DUMMY_DEVICE_NAME
    }

    fn switch_to_device(&self, id: &str, name: &str) -> anyhow::Result<()> {
        let device = DeviceRegistry::find(id, name).context("Requested device not found")?;
        self.adopt(device)?;
        Ok(())
    }

    /// Switch to the specified device.
    /// Does not follow changes to system default.
    /// Makes no change if fails to build or adopt new device.
    pub(crate) fn switch_to(&self, id: &str, name: &str) -> anyhow::Result<()> {
        self.switch_to_device(id, name)?;
        self.set_mode(FallbackMode::Explicit);
        Ok(())
    }

    /// Switch to the current default device.
    /// Does not follow changes to system default.
    /// Sets fallback mode to auto, regardless of success/failure of switching.
    /// Resolves the default device name and id and then re-queries the
    /// explicit device to avoid building the system default object,
    /// which can reroute streams based on system default changes.
    pub(crate) fn switch_to_default(&self) -> anyhow::Result<()> {
        self.set_mode(FallbackMode::Auto);
        let default_device = DeviceRegistry::default_device().context("No device available")?;
        let id = default_device
            .try_id()
            .context("Failed to get default device id")?;
        let name = default_device
            .try_name()
            .context("Failed to get default device name")?;
        self.switch_to_device(&id, &name)
    }

    fn adopt(&self, device: CpalAudioDevice) -> anyhow::Result<()> {
        let next = self.generation.load(Ordering::Relaxed) + 1;
        let reporter = FatalReporter::new(Arc::clone(&self.fatal), next);
        let started = std::time::Instant::now();
        let (payload, info) = Builder::build(device, &self.config, &self.stats, reporter)
            .context("Failed to build device")?;
        log::debug!(
            "Built device {} (generation {}) in {:?}",
            info.name,
            next,
            started.elapsed()
        );
        self.send(payload, info, next)?;
        self.generation.store(next, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn adopt_dummy(&self) {
        let (payload, info) = Builder::build_dummy(&self.config);
        let next = self.generation.load(Ordering::Relaxed) + 1;
        match self.send(payload, info, next) {
            Ok(_) => self.generation.store(next, Ordering::Relaxed),
            Err(e) => log::warn!("{e:#}"),
        }
    }

    fn park_if_live(&self) -> bool {
        if !self.is_live() {
            return false;
        }
        self.adopt_dummy();
        true
    }

    fn send(&self, payload: Payload, info: DeviceInfo, generation: u64) -> anyhow::Result<()> {
        let request = HandoffRequest {
            payload,
            info,
            generation,
        };
        self.switch_tx
            .try_send(request)
            .map_err(|e| anyhow::anyhow!("Failed to send switch request: {e}"))
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
            fallback_mode: Arc::clone(&self.fallback_mode),
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
    log::trace!(
        "Audio devices: inputs={:?} outputs={:?}",
        inputs.iter().map(|(_, name)| name).collect::<Vec<_>>(),
        outputs.iter().map(|(_, name)| name).collect::<Vec<_>>()
    );
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
        let _ = tx.send(event);
    }
}

/// Track lack of callbacks.
struct DeviceLiveness {
    generation: u64,
    count: u64,
    switched_at: std::time::Instant,
    last_advance: std::time::Instant,
    advanced_since_switch: bool,
}

impl DeviceLiveness {
    fn new(now: std::time::Instant, generation: u64, count: u64) -> Self {
        DeviceLiveness {
            generation,
            count,
            switched_at: now,
            last_advance: now,
            advanced_since_switch: false,
        }
    }

    fn frozen_for(&self, now: std::time::Instant) -> std::time::Duration {
        now.saturating_duration_since(self.last_advance)
    }

    /// True when current device is not dummy, the startup grace has elapsed,
    /// and no callback has advanced the count within the stall deadline.
    fn check_stalled(
        &mut self,
        now: std::time::Instant,
        generation: u64,
        current_name: &str,
        count: u64,
        period: Option<std::time::Duration>,
    ) -> bool {
        let stall_timeout = stall_timeout_for_period(period);
        if generation != self.generation {
            self.generation = generation;
            self.count = count;
            self.switched_at = now;
            self.last_advance = now;
            self.advanced_since_switch = false;
            log::info!(
                "DeviceLiveness: device {current_name} switched to generation {generation}: period={period:?} deadline={stall_timeout:?}"
            );
            return false;
        }
        if current_name == DUMMY_DEVICE_NAME {
            self.count = count;
            self.last_advance = now;
            return false;
        }
        if count != self.count {
            self.count = count;
            self.last_advance = now;
            if !self.advanced_since_switch {
                self.advanced_since_switch = true;
                log::debug!(
                    "DeviceLiveness: device {current_name} first callback {:?} \
                     after switch (generation {generation})",
                    now.duration_since(self.switched_at)
                );
            }
            return false;
        }
        if now < self.switched_at + STARTUP_GRACE {
            return false;
        }
        now.duration_since(self.last_advance) > stall_timeout
    }
}

fn handle_device_loss<Payload, Stats, Registry, Builder>(
    manager: &DeviceManager<Payload, Stats, Registry, Builder>,
    policy: &mut FallbackPolicy,
    now: std::time::Instant,
) -> bool
where
    Registry: AudioDeviceRegistry<CpalAudioDevice>,
    Builder: PayloadBuilder<Payload = Payload, Stats = Stats>,
{
    let decision = policy.on_device_lost(now, manager.mode(), manager.is_live());
    match decision {
        FallbackDecision::DoNothing => {
            log::debug!("no fallback action");
            false
        }
        FallbackDecision::ParkOnDummy => {
            log::warn!("Parking explicit device on dummy");
            manager.park_if_live()
        }
        FallbackDecision::RetryDefault => match manager.switch_to_default() {
            Ok(()) => {
                log::warn!("Fell back to default device");
                true
            }
            Err(e) => {
                log::warn!("Default fallback failed ({e:#}), parking on dummy");
                manager.park_if_live()
            }
        },
    }
}

pub(crate) async fn run_device_supervisor(
    input: InputManager,
    output: OutputManager,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    cancel: CancellationToken,
) {
    let input_fatal = input.fatal_signal();
    let output_fatal = output.fatal_signal();
    {
        let pipeline = input.pipeline_config();
        log::info!(
            "Audio supervisor starting: host={} pipeline=({}ch {}Hz frames={}) input={} output={}",
            super::cpal_registry::shared_host().id().name(),
            pipeline.channels(),
            pipeline.sample_rate(),
            pipeline.frames(),
            input.current_name(),
            output.current_name()
        );
    }
    let mut liveness_tick = tokio::time::interval(WATCHDOG_INTERVAL);
    liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut input_liveness = DeviceLiveness::new(
        std::time::Instant::now(),
        input.generation(),
        input.stats().data_callbacks(),
    );
    let mut output_liveness = DeviceLiveness::new(
        std::time::Instant::now(),
        output.generation(),
        output.stats().data_callbacks(),
    );
    let mut input_fallback = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
    let mut output_fallback = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
    loop {
        tokio::select! {
            _ = input_fatal.notified() => {
                let generation = input_fatal.generation();
                if generation != input.generation() {
                    log::debug!("Ignoring stale input failure for generation {generation}");
                } else {
                    let kind = super::stream_errors::input_fatal_kind();
                    log::warn!("Input stream failed ({kind:?}) gen {generation}");
                    let now = std::time::Instant::now();
                    if handle_device_loss(&input, &mut input_fallback, now) {
                        send_refresh(&app_event_tx, &input, &output);
                    }
                }
            }
            _ = output_fatal.notified() => {
                let generation = output_fatal.generation();
                if generation != output.generation() {
                    log::debug!("Ignoring stale output failure for generation {generation}");
                } else {
                    let kind = super::stream_errors::output_fatal_kind();
                    log::warn!("Output stream failed ({kind:?}) gen {generation}");
                    let now = std::time::Instant::now();
                    if handle_device_loss(&output, &mut output_fallback, now) {
                        send_refresh(&app_event_tx, &input, &output);
                    }
                }
            }
            _ = liveness_tick.tick() => {
                let now = std::time::Instant::now();
                let input_period = input.stats().callback_period();
                let input_dead = input_liveness.check_stalled(
                    now,
                    input.generation(),
                    &input.current_name(),
                    input.stats().data_callbacks(),
                    input_period,
                );
                if input_dead {
                    log::debug!(
                        "Input stalled (no data callbacks for {:?}, deadline {:?}, gen {} {})",
                        input_liveness.frozen_for(now),
                        stall_timeout_for_period(input_period),
                        input.generation(),
                        input.current_name(),
                    );
                    if handle_device_loss(&input, &mut input_fallback, now) {
                        send_refresh(&app_event_tx, &input, &output);
                    }
                }
                let output_period = output.stats().callback_period();
                let output_dead = output_liveness.check_stalled(
                    now,
                    output.generation(),
                    &output.current_name(),
                    output.stats().data_callbacks(),
                    output_period,
                );
                if output_dead {
                    log::debug!(
                        "Output stalled (no data callbacks for {:?}, deadline {:?}, gen {} {})",
                        output_liveness.frozen_for(now),
                        stall_timeout_for_period(output_period),
                        output.generation(),
                        output.current_name(),
                    );
                    if handle_device_loss(&output, &mut output_fallback, now) {
                        send_refresh(&app_event_tx, &input, &output);
                    }
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    use tokio::sync::{mpsc, watch};

    use super::super::handoff::HandoffRequest;
    use super::super::stream_errors::{FatalReporter, FatalSignal};
    use super::{
        DUMMY_DEVICE_NAME, DeviceInfo, DeviceLiveness, DeviceManager, FALLBACK_MIN_INTERVAL,
        FallbackDecision, FallbackMode, FallbackPolicy, MAX_STALL_TIMEOUT, MIN_STALL_TIMEOUT,
        PayloadBuilder, STARTUP_GRACE, handle_device_loss, stall_timeout_for_period,
    };
    use crate::audio::cpal_registry::CpalAudioDevice;
    use insanity_core::audio::config::AudioPipelineConfig;
    use insanity_core::audio::device::AudioDeviceRegistry;

    struct EmptyRegistry;

    impl AudioDeviceRegistry<CpalAudioDevice> for EmptyRegistry {
        fn list_devices() -> Vec<CpalAudioDevice> {
            Vec::new()
        }

        fn default_device() -> Option<CpalAudioDevice> {
            None
        }
    }

    struct DummyBuilder;

    impl PayloadBuilder for DummyBuilder {
        type Payload = String;
        type Stats = ();

        fn build(
            _device: CpalAudioDevice,
            _config: &AudioPipelineConfig,
            _stats: &Arc<Self::Stats>,
            _reporter: FatalReporter,
        ) -> anyhow::Result<(Self::Payload, DeviceInfo)> {
            anyhow::bail!("no hardware in unit test")
        }

        fn build_dummy(config: &AudioPipelineConfig) -> (Self::Payload, DeviceInfo) {
            (
                String::from("dummy"),
                DeviceInfo {
                    name: String::from(DUMMY_DEVICE_NAME),
                    format: config.audio_format(),
                },
            )
        }
    }

    type TestManager = DeviceManager<String, (), EmptyRegistry, DummyBuilder>;

    fn test_manager_named(
        name: &str,
    ) -> (
        TestManager,
        mpsc::Receiver<HandoffRequest<String, DeviceInfo>>,
    ) {
        let config = AudioPipelineConfig::default();
        let (switch_tx, switch_rx) = mpsc::channel(8);
        let (_, info_rx) = watch::channel(DeviceInfo {
            name: String::from(name),
            format: config.audio_format(),
        });
        let manager = TestManager::new(
            config,
            switch_tx,
            Arc::new(FatalSignal::new()),
            Arc::new(()),
            info_rx,
            Arc::new(AtomicU64::new(0)),
        );
        (manager, switch_rx)
    }

    fn test_manager() -> (
        TestManager,
        mpsc::Receiver<HandoffRequest<String, DeviceInfo>>,
    ) {
        test_manager_named(DUMMY_DEVICE_NAME)
    }

    fn set_explicit(manager: &TestManager) {
        manager.set_mode(FallbackMode::Explicit);
    }

    #[test]
    fn new_starts_in_auto() {
        let (manager, _rx) = test_manager();
        assert_eq!(manager.mode(), FallbackMode::Auto);
        assert_eq!(manager.generation(), 0);
    }

    #[test]
    fn failed_switch_leaves_auto() {
        let (manager, mut rx) = test_manager();
        assert!(manager.switch_to("missing-id", "missing-name").is_err());
        assert_eq!(manager.mode(), FallbackMode::Auto);
        assert_eq!(manager.generation(), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn switch_to_default_stays_auto() {
        let (manager, mut rx) = test_manager_named("device");
        set_explicit(&manager);
        assert!(manager.switch_to_default().is_err());
        assert_eq!(manager.mode(), FallbackMode::Auto);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn policy_throttles_default_retries() {
        let t0 = Instant::now();
        let mut policy = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
        assert_eq!(
            policy.on_device_lost(t0, FallbackMode::Auto, true),
            FallbackDecision::RetryDefault
        );
        assert_eq!(
            policy.on_device_lost(t0 + ms(100), FallbackMode::Auto, true),
            FallbackDecision::DoNothing
        );
        assert_eq!(
            policy.on_device_lost(t0 + FALLBACK_MIN_INTERVAL + ms(1), FallbackMode::Auto, true),
            FallbackDecision::RetryDefault
        );
    }

    #[test]
    fn policy_explicit_parks_without_throttling() {
        let t0 = Instant::now();
        let mut policy = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
        assert_eq!(
            policy.on_device_lost(t0, FallbackMode::Explicit, true),
            FallbackDecision::ParkOnDummy
        );
        assert_eq!(
            policy.on_device_lost(t0 + ms(100), FallbackMode::Explicit, true),
            FallbackDecision::ParkOnDummy
        );
        assert!(policy.last_default_attempt.is_none());
    }

    #[test]
    fn policy_already_dummy_is_noop() {
        let t0 = Instant::now();
        let mut policy = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
        for mode in [FallbackMode::Auto, FallbackMode::Explicit] {
            assert_eq!(
                policy.on_device_lost(t0, mode, false),
                FallbackDecision::DoNothing
            );
        }
        assert!(policy.last_default_attempt.is_none());
    }

    #[test]
    fn handle_auto_parks_on_failed_default() {
        let (manager, mut rx) = test_manager_named("device");
        let mut policy = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
        assert!(handle_device_loss(&manager, &mut policy, Instant::now()));
        assert_eq!(manager.generation(), 1);
        let request = rx.try_recv().expect("must park on dummy");
        assert_eq!(request.info.name, DUMMY_DEVICE_NAME);
    }

    #[test]
    fn handle_throttled_keeps_current() {
        let (manager, mut rx) = test_manager_named("device");
        let t0 = Instant::now();
        let mut policy = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
        assert!(handle_device_loss(&manager, &mut policy, t0));
        let _ = rx.try_recv().expect("must park on dummy");
        assert!(!handle_device_loss(&manager, &mut policy, t0 + ms(100)));
        assert_eq!(manager.generation(), 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handle_already_dummy_is_noop() {
        let (manager, mut rx) = test_manager();
        let mut policy = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
        assert!(!handle_device_loss(&manager, &mut policy, Instant::now()));
        set_explicit(&manager);
        assert!(!handle_device_loss(&manager, &mut policy, Instant::now()));
        assert_eq!(manager.generation(), 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn handle_explicit_parks() {
        let (manager, mut rx) = test_manager_named("device");
        set_explicit(&manager);
        let mut policy = FallbackPolicy::new(FALLBACK_MIN_INTERVAL);
        assert!(handle_device_loss(&manager, &mut policy, Instant::now()));
        assert_eq!(manager.generation(), 1);
        let request = rx.try_recv().expect("explicit must park");
        assert_eq!(request.info.name, DUMMY_DEVICE_NAME);
        assert!(policy.last_default_attempt.is_none());
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn live_at(now: Instant) -> DeviceLiveness {
        DeviceLiveness::new(now, 1, 10)
    }

    #[test]
    fn frozen_count_inside_grace_is_not_dead() {
        let t0 = Instant::now();
        let mut liveness = live_at(t0);
        for dt in [100, 200, 1000] {
            assert!(
                !liveness.check_stalled(t0 + ms(dt), 1, "device", 10, Some(ms(100))),
                "frozen count at +{dt}ms must not trip inside grace"
            );
        }
    }

    #[test]
    fn frozen_count_past_grace_and_deadline_is_dead() {
        let t0 = Instant::now();
        let mut liveness = live_at(t0);
        assert!(!liveness.check_stalled(t0 + ms(1000), 1, "device", 10, Some(ms(100))));
        assert!(liveness.check_stalled(
            t0 + STARTUP_GRACE + ms(501),
            1,
            "device",
            10,
            Some(ms(100))
        ));
    }

    #[test]
    fn generation_switch_resnapshots_deadline() {
        let t0 = Instant::now();
        let mut liveness = live_at(t0);
        let switch_at = t0 + STARTUP_GRACE + ms(10_000);
        assert!(!liveness.check_stalled(switch_at, 2, "device", 10, Some(Duration::from_secs(2))));
        assert!(!liveness.check_stalled(
            switch_at + STARTUP_GRACE + ms(600),
            2,
            "device",
            10,
            Some(Duration::from_secs(2))
        ));
        assert!(liveness.check_stalled(
            switch_at + STARTUP_GRACE + ms(5001),
            2,
            "device",
            10,
            Some(Duration::from_secs(2))
        ));
    }

    #[test]
    fn stall_timeout_scales_with_period_and_clamps() {
        // 480 frames @ 48kHz = 10ms period -> 30ms, floored to the minimum.
        assert_eq!(
            stall_timeout_for_period(Some(Duration::from_millis(10))),
            MIN_STALL_TIMEOUT
        );
        // Unknown period -> minimum.
        assert_eq!(stall_timeout_for_period(None), MIN_STALL_TIMEOUT);
        // 96000 frames @ 48kHz = 2s period -> 6s, capped at the maximum.
        assert_eq!(
            stall_timeout_for_period(Some(Duration::from_secs(2))),
            MAX_STALL_TIMEOUT
        );
        // A mid-range period passes through scaled.
        assert_eq!(stall_timeout_for_period(Some(ms(300))), ms(900));
    }
}
