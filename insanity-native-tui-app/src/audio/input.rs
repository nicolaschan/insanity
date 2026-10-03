use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::{AudioChunk, ChunkSource, SampleChunker};
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::device::{AudioDevice, AudioDeviceRegistry, UNKNOWN_DEVICE_NAME};
use insanity_core::audio::sample::SampleSource;
use rubato_audio_source::RubatoResampler;
use tokio::sync::{mpsc, watch};

use crate::audio::cpal_registry::{CpalAudioDevice, CpalInputDeviceRegistry};
use crate::audio::device_supervisor::{DeviceInfo, DeviceManager, PayloadBuilder};

use super::cpal_stream_receiver::{CpalStreamReceiver, InputStats, make_single_input};
use super::handoff::{HANDOFF_BOUND, HandoffRequest};
use super::stream_errors::{FatalReporter, FatalSignal};

pub(crate) struct SilentChunkSource {
    format: AudioFormat,
    frames: usize,
    next_sequence: u128,
}

impl SilentChunkSource {
    fn new(format: AudioFormat, frames: usize) -> Self {
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

pub(crate) type LiveChain = SampleChunker<RubatoResampler<CpalStreamReceiver>>;

pub(crate) enum InputChain {
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

pub(crate) struct SwitchingInputSource<ChainT> {
    current: Option<ChainT>,
    next_sequence: u128,
    swap_rx: mpsc::Receiver<HandoffRequest<ChainT, DeviceInfo>>,
    info_tx: watch::Sender<DeviceInfo>,
}

impl<ChainT: ChunkSource + Send> SwitchingInputSource<ChainT> {
    fn apply(&mut self, request: HandoffRequest<ChainT, DeviceInfo>) {
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

pub(crate) struct InputChainBuilder;

impl PayloadBuilder for InputChainBuilder {
    type Payload = InputChain;
    type Stats = InputStats;

    fn build(
        device: CpalAudioDevice,
        config: &AudioPipelineConfig,
        stats: &Arc<Self::Stats>,
        reporter: FatalReporter,
    ) -> Option<(Self::Payload, DeviceInfo)> {
        let name = device.name();
        match make_single_input(device.0, config, reporter, stats) {
            Ok(receiver) => {
                let format = receiver.format().clone();
                let resampled =
                    RubatoResampler::new(receiver, config.sample_rate(), config.frames());
                let chain =
                    InputChain::Live(Box::new(SampleChunker::new(resampled, config.frames())));
                let info = DeviceInfo { name, format };
                Some((chain, info))
            }
            Err(e) => {
                log::warn!("Failed to build input for {name}, keeping current: {e:?}");
                None
            }
        }
    }

    fn build_dummy(config: &AudioPipelineConfig) -> (Self::Payload, DeviceInfo) {
        let format = config.audio_format();
        let chain = InputChain::Silent(SilentChunkSource::new(format.clone(), config.frames()));
        let info = DeviceInfo {
            name: UNKNOWN_DEVICE_NAME.into(),
            format,
        };
        (chain, info)
    }
}

pub(crate) type InputManager =
    DeviceManager<InputChain, InputStats, CpalInputDeviceRegistry, InputChainBuilder>;

pub(crate) struct AudioInput {
    pub(crate) manager: InputManager,
    pub(crate) source: SwitchingInputSource<InputChain>,
}

pub(crate) fn start_input(config: AudioPipelineConfig) -> AudioInput {
    let stats = Arc::new(InputStats::default());
    let fatal = Arc::new(FatalSignal::new());
    let (switch_tx, swap_rx) = mpsc::channel(HANDOFF_BOUND);
    let (initial_chain, initial_info, generation) = match CpalInputDeviceRegistry::default_device()
    {
        Some(device) => {
            let reporter = FatalReporter::new(Arc::clone(&fatal), 1);
            match InputChainBuilder::build(device, &config, &stats, reporter) {
                Some(built) => (Some(built.0), built.1, 1),
                None => {
                    let (chain, info) = InputChainBuilder::build_dummy(&config);
                    (Some(chain), info, 0)
                }
            }
        }
        None => {
            log::warn!("No input device available, starting silent");
            let (chain, info) = InputChainBuilder::build_dummy(&config);
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
    let manager = InputManager::new(
        config,
        switch_tx,
        fatal,
        stats,
        info_rx,
        Arc::new(AtomicU64::new(generation)),
    );
    AudioInput { manager, source }
}

#[cfg(test)]
mod tests {
    use super::super::device_supervisor::PayloadBuilder;
    use super::InputChainBuilder;

    use super::{DeviceInfo, InputChain, SilentChunkSource, SwitchingInputSource};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::{AudioChunk, ChunkSource};
    use insanity_core::audio::config::AudioPipelineConfig;
    use std::collections::VecDeque;
    use tokio::sync::{mpsc, watch};

    use super::super::handoff::HandoffRequest;

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
        let (info_tx, _info_rx) = watch::channel(DeviceInfo {
            name: "test".into(),
            format: format.clone(),
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

    fn info(name: &str, channels: u16) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            format: AudioFormat::new(channels, 48000),
        }
    }

    fn script_source(
        initial: Option<Script>,
    ) -> (
        SwitchingInputSource<Script>,
        mpsc::Sender<HandoffRequest<Script, DeviceInfo>>,
        watch::Receiver<DeviceInfo>,
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

    fn handoff(chain: Script, name: &str, channels: u16) -> HandoffRequest<Script, DeviceInfo> {
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

    #[tokio::test]
    async fn build_silent_matches_pipeline_format() {
        let config = AudioPipelineConfig::default();
        let (chain, info) = InputChainBuilder::build_dummy(&config);
        let InputChain::Silent(mut silent) = chain else {
            panic!("expected silent chain");
        };
        let chunk = silent.next_chunk().await.unwrap();
        assert_eq!(chunk.audio_data, vec![0.0; config.block_samples()]);
        assert_eq!(info.format, config.audio_format());
        assert_eq!(info.name, insanity_core::audio::device::UNKNOWN_DEVICE_NAME);
    }
}
