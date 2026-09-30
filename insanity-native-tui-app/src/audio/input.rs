use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::Device;
use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::{AudioChunk, ChunkSource, SampleChunker};
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::device::UNKNOWN_DEVICE_NAME;
use insanity_core::audio::sample::SampleSource;
use rubato_audio_source::RubatoResampler;
use tokio::sync::{mpsc, watch};

use super::cpal_registry::{default_real_input, device_name, find_input_by_id_name};
use super::cpal_stream_receiver::{CpalStreamReceiver, InputStats, make_single_input};
use super::handoff::{HANDOFF_BOUND, HandoffRequest, Selection};
use super::stream_errors::{FatalReporter, FatalSignal};

pub type LiveChain = SampleChunker<RubatoResampler<CpalStreamReceiver>>;

pub struct SilentChunkSource {
    format: AudioFormat,
    frames: usize,
    next_sequence: u128,
}

impl SilentChunkSource {
    pub fn new(format: AudioFormat, frames: usize) -> Self {
        Self {
            format,
            frames,
            next_sequence: 0,
        }
    }
}

impl ChunkSource for SilentChunkSource {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        let sequence_number = self.next_sequence;
        self.next_sequence += 1;
        Some(AudioChunk::new(
            sequence_number,
            self.format.clone(),
            vec![0.0; self.frames * self.format.channel_count as usize],
        ))
    }
}

pub enum InputChain {
    Live(Box<LiveChain>),
    Silent(SilentChunkSource),
}

impl ChunkSource for InputChain {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        match self {
            Self::Live(chain) => chain.next_chunk().await,
            Self::Silent(chain) => chain.next_chunk().await,
        }
    }
}

#[derive(Clone, Debug)]
pub struct InputInfo {
    pub name: String,
    pub format: AudioFormat,
    pub selection: Selection,
}

pub struct SwitchingInputSource<ChainT> {
    current: Option<ChainT>,
    next_sequence: u128,
    swap_rx: mpsc::Receiver<HandoffRequest<ChainT, InputInfo>>,
    info_tx: watch::Sender<InputInfo>,
}

impl<ChainT: ChunkSource + Send> SwitchingInputSource<ChainT> {
    fn apply(&mut self, request: HandoffRequest<ChainT, InputInfo>) {
        let name = request.info.name.clone();
        self.current = Some(request.payload);
        self.info_tx.send_replace(request.info);
        log::info!(
            "Input device switched to {name} (generation {})",
            request.generation
        );
    }
}

impl<ChainT: ChunkSource + Send> ChunkSource for SwitchingInputSource<ChainT> {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        loop {
            let mut current = match self.current.take() {
                Some(current) => current,
                None => {
                    let request = self.swap_rx.recv().await?;
                    self.apply(request);
                    continue;
                }
            };
            tokio::select! {
                biased;
                swapped = self.swap_rx.recv() => {
                    let request = swapped?;
                    self.apply(request);
                }
                chunk = current.next_chunk() => match chunk {
                    Some(mut chunk) => {
                        chunk.sequence_number = self.next_sequence;
                        self.next_sequence += 1;
                        self.current = Some(current);
                        return Some(chunk);
                    }
                    None => {
                        log::warn!("Input stream ended, parking until a device is set");
                        self.current = None;
                    }
                },
            }
        }
    }
}

#[derive(Clone)]
pub struct InputManager {
    config: AudioPipelineConfig,
    switch_tx: mpsc::Sender<HandoffRequest<InputChain, InputInfo>>,
    fatal: Arc<FatalSignal>,
    stats: Arc<InputStats>,
    info: watch::Receiver<InputInfo>,
    generation: Arc<AtomicU64>,
    current: Selection,
}

impl InputManager {
    pub fn selection(&self) -> &Selection {
        &self.current
    }

    pub fn subscribe(&self) -> watch::Receiver<InputInfo> {
        self.info.clone()
    }

    pub fn current_name(&self) -> String {
        self.info.borrow().name.clone()
    }

    pub fn stats(&self) -> Arc<InputStats> {
        Arc::clone(&self.stats)
    }

    pub fn fatal_signal(&self) -> Arc<FatalSignal> {
        Arc::clone(&self.fatal)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn switch_to(&mut self, id: &str, name: &str) {
        let Some(device) = find_input_by_id_name(id, name).map(|d| d.0) else {
            log::warn!("Requested input device not found: {name}");
            return;
        };
        self.adopt(
            Selection::Explicit {
                id: id.to_owned(),
                name: name.to_owned(),
            },
            device,
        );
    }

    pub fn follow_default(&mut self) {
        let Some(device) = default_real_input().map(|d| d.0) else {
            log::warn!("No input device available, starting silent");
            self.adopt_silent();
            return;
        };
        let before = self.generation();
        self.adopt(Selection::FollowDefault, device);
        if self.generation() == before {
            self.adopt_silent();
        }
    }

    fn adopt(&mut self, selection: Selection, device: Device) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let reporter = FatalReporter::new(Arc::clone(&self.fatal), generation);
        if let Some((chain, info)) =
            build_input_chain(device, self.config, reporter, &self.stats, &selection)
        {
            self.current = selection;
            self.send(chain, info, generation);
        }
    }

    fn adopt_silent(&mut self) {
        let (chain, info) = build_silent(self.config);
        self.current = Selection::FollowDefault;
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(chain, info, generation);
    }

    fn send(&self, chain: InputChain, info: InputInfo, generation: u64) {
        let name = info.name.clone();
        let request = HandoffRequest {
            payload: chain,
            info,
            generation,
        };
        if self.switch_tx.try_send(request).is_err() {
            log::warn!("Input switch channel full, dropping switch to {name}");
        }
    }
}

pub struct AudioInput {
    pub manager: InputManager,
    pub source: SwitchingInputSource<InputChain>,
}

pub fn start_input(config: AudioPipelineConfig) -> AudioInput {
    let stats = Arc::new(InputStats::default());
    let fatal = Arc::new(FatalSignal::new());
    let (switch_tx, swap_rx) = mpsc::channel(HANDOFF_BOUND);
    let (initial_chain, initial_info, generation) = match default_real_input().map(|d| d.0) {
        Some(device) => {
            let selection = Selection::FollowDefault;
            let reporter = FatalReporter::new(Arc::clone(&fatal), 1);
            match build_input_chain(device, config, reporter, &stats, &selection) {
                Some(built) => (Some(built.0), built.1, 1),
                None => {
                    let (chain, info) = build_silent(config);
                    (Some(chain), info, 0)
                }
            }
        }
        None => {
            log::warn!("No input device available, starting silent");
            let (chain, info) = build_silent(config);
            (Some(chain), info, 0)
        }
    };
    let (info_tx, info_rx) = watch::channel(initial_info);
    let source = SwitchingInputSource {
        current: initial_chain,
        next_sequence: 0,
        swap_rx,
        info_tx: info_tx.clone(),
    };
    let manager = InputManager {
        config,
        switch_tx,
        fatal,
        stats,
        info: info_rx,
        generation: Arc::new(AtomicU64::new(generation)),
        current: Selection::FollowDefault,
    };
    AudioInput { manager, source }
}

fn build_silent(config: AudioPipelineConfig) -> (InputChain, InputInfo) {
    let format = config.audio_format();
    let chain = InputChain::Silent(SilentChunkSource::new(format.clone(), config.frames()));
    let info = InputInfo {
        name: UNKNOWN_DEVICE_NAME.into(),
        format,
        selection: Selection::FollowDefault,
    };
    (chain, info)
}

fn build_input_chain(
    device: Device,
    config: AudioPipelineConfig,
    reporter: FatalReporter,
    stats: &Arc<InputStats>,
    selection: &Selection,
) -> Option<(InputChain, InputInfo)> {
    let name = device_name(&device);
    match make_single_input(device, config, reporter, stats) {
        Ok(receiver) => {
            let format = receiver.format().clone();
            let resampled = RubatoResampler::new(receiver, config.sample_rate(), config.frames());
            let chain = InputChain::Live(Box::new(SampleChunker::new(resampled, config.frames())));
            let info = InputInfo {
                name,
                format,
                selection: selection.clone(),
            };
            Some((chain, info))
        }
        Err(e) => {
            log::warn!("Failed to build input for {name}, keeping current: {e:?}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InputChain, InputInfo, InputManager, SilentChunkSource, SwitchingInputSource, build_silent,
    };
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
    use insanity_core::audio::config::AudioPipelineConfig;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use tokio::sync::{mpsc, watch};

    use super::super::handoff::{HandoffRequest, Selection};
    use super::super::stream_errors::FatalSignal;

    #[tokio::test]
    async fn silent_source_emits_zeros_in_pipeline_format() {
        let format = AudioFormat::new(2, 48000);
        let mut silent = SilentChunkSource::new(format.clone(), 2);
        for expected_seq in 0..3 {
            let chunk = silent.next_chunk().await.unwrap();
            assert_eq!(chunk.sequence_number, expected_seq);
            assert_eq!(chunk.format, format);
            assert_eq!(chunk.audio_data, vec![0.0; 4]);
        }
    }

    #[tokio::test]
    async fn silent_payload_flows_through_switching_source_with_rewritten_sequence() {
        let format = AudioFormat::new(2, 48000);
        let (_switch_tx, swap_rx) = mpsc::channel(8);
        let (info_tx, _info_rx) = watch::channel(InputInfo {
            name: "test".into(),
            format: format.clone(),
            selection: Selection::FollowDefault,
        });
        let mut source: SwitchingInputSource<InputChain> = SwitchingInputSource {
            current: Some(InputChain::Silent(SilentChunkSource::new(
                format.clone(),
                2,
            ))),
            next_sequence: 41,
            swap_rx,
            info_tx,
        };
        let chunk = source.next_chunk().await.unwrap();
        assert_eq!(chunk.sequence_number, 41);
        assert_eq!(chunk.format, format);
        assert_eq!(chunk.audio_data, vec![0.0; 4]);
    }

    struct Script {
        format: AudioFormat,
        samples: VecDeque<f32>,
        frames: usize,
        sequence: u128,
    }

    impl Script {
        fn new(channels: u16, chunks: usize, frames: usize) -> Self {
            Self {
                format: AudioFormat::new(channels, 48000),
                samples: VecDeque::from(vec![1.0; chunks * frames * channels as usize]),
                frames,
                sequence: 0,
            }
        }
    }

    impl ChunkSource for Script {
        async fn next_chunk(&mut self) -> Option<AudioChunk> {
            let len = self.frames * self.format.channel_count as usize;
            if self.samples.len() < len {
                return None;
            }
            let audio_data = self.samples.drain(..len).collect();
            let sequence = self.sequence;
            self.sequence += 1;
            Some(AudioChunk::new(sequence, self.format.clone(), audio_data))
        }
    }

    fn info(name: &str, channels: u16) -> InputInfo {
        InputInfo {
            name: name.into(),
            format: AudioFormat::new(channels, 48000),
            selection: Selection::FollowDefault,
        }
    }

    fn script_source(
        initial: Option<Script>,
    ) -> (
        SwitchingInputSource<Script>,
        mpsc::Sender<HandoffRequest<Script, InputInfo>>,
        watch::Receiver<InputInfo>,
    ) {
        let (switch_tx, swap_rx) = mpsc::channel(8);
        let (info_tx, info_rx) = watch::channel(info("initial", 2));
        let source = SwitchingInputSource {
            current: initial,
            next_sequence: 0,
            swap_rx,
            info_tx,
        };
        (source, switch_tx, info_rx)
    }

    fn handoff(chain: Script, name: &str, channels: u16) -> HandoffRequest<Script, InputInfo> {
        HandoffRequest {
            payload: chain,
            info: info(name, channels),
            generation: 1,
        }
    }

    #[tokio::test]
    async fn swap_carries_format_and_rewrites_sequence() {
        let (mut source, switch_tx, mut info_rx) = script_source(Some(Script::new(2, 4, 2)));
        let first = source.next_chunk().await.unwrap();
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.format.channel_count, 2);
        switch_tx
            .send(handoff(Script::new(1, 4, 2), "usb", 1))
            .await
            .unwrap();
        let second = source.next_chunk().await.unwrap();
        assert_eq!(second.sequence_number, 1);
        assert_eq!(second.format.channel_count, 1);
        assert_eq!(info_rx.borrow_and_update().name, "usb");
        let third = source.next_chunk().await.unwrap();
        assert_eq!(third.sequence_number, 2);
    }

    #[tokio::test]
    async fn dead_inner_parks_until_next_swap() {
        let (mut source, switch_tx, _info_rx) = script_source(Some(Script::new(2, 1, 2)));
        let first = source.next_chunk().await.unwrap();
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.audio_data, vec![1.0; 4]);
        switch_tx
            .send(handoff(Script::new(2, 1, 2), "recovered", 2))
            .await
            .unwrap();
        let recovered = source.next_chunk().await.unwrap();
        assert_eq!(recovered.sequence_number, 1);
        assert_eq!(recovered.audio_data, vec![1.0; 4]);
    }

    #[tokio::test]
    async fn silent_start_waits_for_first_switch() {
        let (mut source, switch_tx, _info_rx) = script_source(None);
        // Keep a sender alive: a closed swap channel means shutdown.
        let _live = switch_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            switch_tx
                .send(handoff(Script::new(2, 2, 2), "late", 2))
                .await
                .unwrap();
        });
        let first = source.next_chunk().await.unwrap();
        assert_eq!(first.sequence_number, 0);
        assert_eq!(first.audio_data, vec![1.0; 4]);
    }

    #[tokio::test]
    async fn last_pending_swap_wins() {
        let (mut source, switch_tx, mut info_rx) = script_source(Some(Script::new(2, 8, 2)));
        switch_tx
            .send(handoff(Script::new(2, 1, 2), "first", 2))
            .await
            .unwrap();
        switch_tx
            .send(handoff(Script::new(1, 1, 2), "second", 1))
            .await
            .unwrap();
        let _ = source.next_chunk().await.unwrap();
        assert_eq!(info_rx.borrow_and_update().name, "second");
    }

    fn test_manager() -> InputManager {
        let (switch_tx, _) = mpsc::channel(8);
        let (_, info_rx) = watch::channel(info("test", 2));
        InputManager {
            config: AudioPipelineConfig::default(),
            switch_tx,
            fatal: Arc::new(FatalSignal::new()),
            stats: Default::default(),
            info: info_rx,
            generation: Arc::new(AtomicU64::new(0)),
            current: Selection::FollowDefault,
        }
    }

    #[test]
    fn unknown_device_switch_warns_and_keeps_selection() {
        let mut manager = test_manager();
        manager.switch_to("no-such-id", "no-such-device");
        assert!(matches!(manager.selection(), Selection::FollowDefault));
    }

    #[test]
    fn follow_default_never_panics_without_devices() {
        let mut manager = test_manager();
        manager.follow_default();
        assert!(matches!(manager.selection(), Selection::FollowDefault));
    }

    #[tokio::test]
    async fn build_silent_matches_pipeline_format() {
        let config = AudioPipelineConfig::default();
        let (chain, info) = build_silent(config);
        let InputChain::Silent(mut silent) = chain else {
            panic!("expected silent chain");
        };
        let chunk = silent.next_chunk().await.unwrap();
        assert_eq!(chunk.audio_data, vec![0.0; config.block_samples()]);
        assert_eq!(info.format, config.audio_format());
        assert_eq!(info.name, insanity_core::audio::device::UNKNOWN_DEVICE_NAME);
        assert!(matches!(info.selection, Selection::FollowDefault));
    }
}
