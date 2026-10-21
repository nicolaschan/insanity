use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
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

fn stall_timeout_for_period(period: Option<std::time::Duration>) -> std::time::Duration {
    match period {
        None => MIN_STALL_TIMEOUT,
        Some(p) => (p * STALL_PERIOD_MULT).clamp(MIN_STALL_TIMEOUT, MAX_STALL_TIMEOUT),
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

    pub(crate) fn pipeline_config(&self) -> AudioPipelineConfig {
        self.config
    }

    fn _switch_to(&mut self, id: &str, name: &str) -> anyhow::Result<()> {
        let device = DeviceRegistry::find(id, name).context("Requested device not found")?;
        self.adopt(device)?;
        Ok(())
    }

    /// Switch to the specified device.
    /// Does not follow changes to system default.
    /// Makes no change if fails to build or adopt new device.
    pub(crate) fn switch_to(&mut self, id: &str, name: &str) {
        if let Err(e) = self._switch_to(id, name) {
            log::warn!("Failed to switch to specified device: {:#}", e);
        }
    }

    fn _switch_to_current_default(&mut self) -> anyhow::Result<()> {
        let default_device = DeviceRegistry::default_device().context("No device available")?;
        let id = default_device
            .try_id()
            .context("Failed to get default device id")?;
        let name = default_device
            .try_name()
            .context("Failed to get default device name")?;
        self._switch_to(&id, &name)
    }

    /// Switch to the current default device.
    /// Does not follow changes to system default.
    /// Makes no change if fails to build or adopt new device.
    // Resolves system default device name and id and then re-queries
    // the explicit device to avoid building the system default object,
    // which can reroute streams based on system default changes.
    pub(crate) fn switch_to_current_default(&mut self) {
        if let Err(e) = self._switch_to_current_default() {
            log::warn!("Failed to switch to current default: {:#}", e);
        }
    }

    fn adopt(&mut self, device: CpalAudioDevice) -> anyhow::Result<()> {
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

    pub(crate) fn adopt_dummy(&mut self) {
        let (payload, info) = Builder::build_dummy(&self.config);
        let next = self.generation.load(Ordering::Relaxed) + 1;
        match self.send(payload, info, next) {
            Ok(_) => self.generation.store(next, Ordering::Relaxed),
            Err(e) => log::warn!("{:#}", e),
        }
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
    fn poll_check_dead(
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

pub(crate) async fn run_device_supervisor(
    mut input: InputManager,
    mut output: OutputManager,
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
    loop {
        tokio::select! {
            _ = input_fatal.notified() => {
                if input_fatal.generation() == input.generation() {
                    log::info!(
                        "Input stream failed ({:?}), falling back to dummy device",
                        super::stream_errors::input_fatal_kind()
                    );
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
                    log::info!(
                        "Output stream failed ({:?}), falling back to dummy device",
                        super::stream_errors::output_fatal_kind()
                    );
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
                let now = std::time::Instant::now();
                let input_period = input.stats().callback_period();
                if input_liveness.poll_check_dead(
                    now,
                    input.generation(),
                    &input.current_name(),
                    input.stats().data_callbacks(),
                    input_period,
                ) {
                    log::warn!(
                        "Input stalled (no data callbacks for {:?}, deadline {:?}, \
                         gen {} {}), falling back to dummy device",
                        input_liveness.frozen_for(now),
                        stall_timeout_for_period(input_period),
                        input.generation(),
                        input.current_name()
                    );
                    input.adopt_dummy();
                }
                let output_period = output.stats().callback_period();
                if output_liveness.poll_check_dead(
                    now,
                    output.generation(),
                    &output.current_name(),
                    output.stats().data_callbacks(),
                    output_period,
                ) {
                    log::warn!(
                        "Output stalled (no data callbacks for {:?}, deadline {:?}, \
                         gen {} {}), falling back to dummy device",
                        output_liveness.frozen_for(now),
                        stall_timeout_for_period(output_period),
                        output.generation(),
                        output.current_name()
                    );
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
    use std::time::{Duration, Instant};

    use super::{
        DeviceLiveness, MAX_STALL_TIMEOUT, MIN_STALL_TIMEOUT, STARTUP_GRACE,
        stall_timeout_for_period,
    };

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
                !liveness.poll_check_dead(t0 + ms(dt), 1, "device", 10, Some(ms(100))),
                "frozen count at +{dt}ms must not trip inside grace"
            );
        }
    }

    #[test]
    fn frozen_count_past_grace_and_deadline_is_dead() {
        let t0 = Instant::now();
        let mut liveness = live_at(t0);
        // Inside grace but past deadline: grace wins.
        assert!(!liveness.poll_check_dead(t0 + ms(1000), 1, "device", 10, Some(ms(100))));
        // Past both: dead.
        assert!(liveness.poll_check_dead(
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
        // New generation carries a longer (5s) deadline.
        assert!(!liveness.poll_check_dead(
            switch_at,
            2,
            "device",
            10,
            Some(Duration::from_secs(2))
        ));
        // Past grace and past the old 500ms deadline, but within the new one:
        // alive, proving the fresh deadline governs.
        assert!(!liveness.poll_check_dead(
            switch_at + STARTUP_GRACE + ms(600),
            2,
            "device",
            10,
            Some(Duration::from_secs(2))
        ));
        // Past the new deadline: dead.
        assert!(liveness.poll_check_dead(
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
