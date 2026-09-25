use std::sync::Arc;

use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::{AudioChunk, ChunkSource, SampleChunker};
use insanity_core::audio::config::AudioPipelineConfig;
use insanity_core::audio::converter::ResampledSource;
use insanity_core::audio::device::AudioDevice;
use rubato_audio_source::StreamResampler;
use tokio::sync::{Notify, mpsc, watch};

use super::cpal_registry::{CpalAudioDevice, default_input_device, find_input_by_id_name};
use super::cpal_stream_receiver::{CpalStreamReceiver, InputStats, make_single_input};
use super::switch::{Selection, SilenceChunkSource, SwapRequest, SwitchingChunkSource};

pub enum InputChunkSource {
    Real(Box<SampleChunker<ResampledSource<CpalStreamReceiver, StreamResampler>>>),
    Silence(SilenceChunkSource),
}

impl ChunkSource for InputChunkSource {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        match self {
            Self::Real(source) => source.next_chunk().await,
            Self::Silence(source) => source.next_chunk().await,
        }
    }
}

#[derive(Clone)]
pub struct InputInfo {
    pub name: String,
    pub stats: Arc<InputStats>,
    pub selection: Selection,
}

#[derive(Clone)]
pub struct InputManager {
    switch_tx: mpsc::UnboundedSender<SwapRequest<InputChunkSource>>,
    info: watch::Sender<InputInfo>,
    config: AudioPipelineConfig,
    fatal: Arc<Notify>,
}

impl InputManager {
    pub fn current(&self) -> InputInfo {
        self.info.borrow().clone()
    }

    pub fn switch_to(&self, id: &str, name: &str) -> anyhow::Result<String> {
        let Some(device) = find_input_by_id_name(id, name) else {
            return Err(anyhow::anyhow!(
                "Unknown input device id: {id}, name: {name}"
            ));
        };
        let (source, name, stats) = build_real(device, self.config, self.fatal.clone())?;
        self.adopt(source, name, stats, Selection::Explicit(id.to_owned()))
    }

    pub fn follow_default(&self) -> anyhow::Result<String> {
        let (source, name, stats) =
            resolve_source_or_silent(default_input_device(), self.config, self.fatal.clone());
        self.adopt(source, name, stats, Selection::FollowDefault)
    }

    fn adopt(
        &self,
        source: InputChunkSource,
        name: String,
        stats: Arc<InputStats>,
        selection: Selection,
    ) -> anyhow::Result<String> {
        let info = self.info.clone();
        let adopted_name = name.clone();
        self.switch_tx
            .send(SwapRequest {
                payload: source,
                on_adopt: Box::new(move || {
                    info.send_replace(InputInfo {
                        name: adopted_name,
                        stats,
                        selection,
                    });
                }),
            })
            .map_err(|_| anyhow::anyhow!("Input loop is gone"))?;
        Ok(name)
    }
}

pub fn start_input(
    initial: Option<CpalAudioDevice>,
    config: AudioPipelineConfig,
    fatal: Arc<Notify>,
) -> (InputManager, SwitchingChunkSource<InputChunkSource>) {
    let (source, name, stats) = resolve_source_or_silent(initial, config, fatal.clone());
    let (info, _) = watch::channel(InputInfo {
        name,
        stats,
        selection: Selection::FollowDefault,
    });
    let switching = SwitchingChunkSource::new(source);
    let manager = InputManager {
        switch_tx: switching.switcher(),
        info,
        config,
        fatal,
    };
    (manager, switching)
}

fn resolve_source_or_silent(
    opt_device: Option<CpalAudioDevice>,
    config: AudioPipelineConfig,
    fatal: Arc<Notify>,
) -> (InputChunkSource, String, Arc<InputStats>) {
    match opt_device {
        Some(device) => match build_real(device, config, fatal) {
            Ok(built) => built,
            Err(e) => {
                log::warn!("Failed to open default input, falling back to silence: {e:?}");
                build_silence(config)
            }
        },
        None => build_silence(config),
    }
}

fn build_real(
    device: CpalAudioDevice,
    config: AudioPipelineConfig,
    fatal: Arc<Notify>,
) -> anyhow::Result<(InputChunkSource, String, Arc<InputStats>)> {
    let name = device.name();
    let receiver = make_single_input(device.0, config, fatal)?;
    let stats = receiver.stats();
    let resampled = ResampledSource::new(receiver, config.sample_rate(), config.frames());
    let chunked = SampleChunker::new(resampled, config.frames());
    Ok((InputChunkSource::Real(Box::new(chunked)), name, stats))
}

fn build_silence(config: AudioPipelineConfig) -> (InputChunkSource, String, Arc<InputStats>) {
    let format = AudioFormat::new(config.channels(), config.sample_rate());
    let source = SilenceChunkSource::new(format, config.frames());
    (
        InputChunkSource::Silence(source),
        "No input device".to_owned(),
        Arc::new(InputStats::default()),
    )
}

#[cfg(test)]
mod tests {
    use super::super::cpal_stream_receiver::InputStats;
    use super::{InputInfo, build_silence, start_input};
    use crate::audio::switch::{Selection, SwapRequest};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::ChunkSource;
    use insanity_core::audio::config::AudioPipelineConfig;
    use std::sync::Arc;
    use tokio::sync::Notify;

    fn test_fatal() -> Arc<Notify> {
        Arc::new(Notify::new())
    }

    #[tokio::test]
    async fn silence_initial_matches_pipeline_format() {
        let config = AudioPipelineConfig::default();
        let (_, mut switching) = start_input(None, config, test_fatal());
        let chunk = switching.next_chunk().await.unwrap();
        assert_eq!(
            chunk.audio_data,
            vec![0.0; config.frames() * usize::from(config.channels())]
        );
        assert_eq!(
            chunk.format,
            AudioFormat::new(config.channels(), config.sample_rate())
        );
    }

    #[tokio::test]
    async fn follow_default_converges_to_follow_default_selection() {
        let config = AudioPipelineConfig::default();
        let (manager, mut switching) = start_input(None, config, test_fatal());
        let name = manager.follow_default().expect("follow_default sends");
        assert_eq!(manager.current().selection, Selection::FollowDefault);
        switching.next_chunk().await.unwrap();
        assert_eq!(manager.current().name, name);
        assert_eq!(manager.current().selection, Selection::FollowDefault);
    }

    #[tokio::test]
    async fn unknown_id_switch_errors_and_source_undisturbed() {
        let config = AudioPipelineConfig::default();
        let (manager, mut switching) = start_input(None, config, test_fatal());
        assert!(
            manager
                .switch_to("no-such-device", "No Such Device")
                .is_err()
        );
        assert_eq!(manager.current().selection, Selection::FollowDefault);
        let chunk = switching.next_chunk().await.unwrap();
        assert!(chunk.audio_data.iter().all(|s| *s == 0.0));
    }

    #[tokio::test]
    async fn info_updates_at_adoption_not_at_send() {
        let config = AudioPipelineConfig::default();
        let (manager, mut switching) = start_input(None, config, test_fatal());
        assert_eq!(manager.current().selection, Selection::FollowDefault);
        let (source, _, _) = build_silence(config);
        let info = manager.info.clone();
        manager
            .switch_tx
            .send(SwapRequest {
                payload: source,
                on_adopt: Box::new(move || {
                    info.send_replace(InputInfo {
                        name: "adopted".to_owned(),
                        stats: Arc::new(InputStats::default()),
                        selection: Selection::Explicit("adopted-id".to_owned()),
                    });
                }),
            })
            .unwrap();
        assert_eq!(manager.current().selection, Selection::FollowDefault);
        switching.next_chunk().await.unwrap();
        assert_eq!(manager.current().name, "adopted");
        assert_eq!(
            manager.current().selection,
            Selection::Explicit("adopted-id".to_owned())
        );
    }
}
