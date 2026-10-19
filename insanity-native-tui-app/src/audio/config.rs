use std::cmp::Reverse;
use std::time::Duration;

use cpal::traits::DeviceTrait;
use cpal::{BufferSize, Device, SampleFormat, StreamConfig};

use insanity_core::audio::config::AudioPipelineConfig;

pub(crate) const AUDIO_CALLBACK_FRAMES: u32 = 480;

pub(crate) const STREAM_BUILD_TIMEOUT: Duration = Duration::from_secs(5);

const PULSEAUDIO_HOST: &str = "PulseAudio";

fn callback_buffer_size(host: &str, supported: &cpal::SupportedBufferSize) -> BufferSize {
    match (host, supported) {
        (PULSEAUDIO_HOST, cpal::SupportedBufferSize::Range { min, max }) => {
            let clamped = AUDIO_CALLBACK_FRAMES.clamp(*min, *max);
            log::trace!("requesting fixed stream buffer of {clamped} frames");
            BufferSize::Fixed(clamped)
        }
        _ => BufferSize::Default,
    }
}

fn host_name() -> &'static str {
    super::cpal_registry::shared_host().id().name()
}

pub(crate) fn sample_format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(100),
        SampleFormat::F64 => Some(90),
        SampleFormat::I32 => Some(80),
        SampleFormat::U32 => Some(79),
        SampleFormat::I16 => Some(60),
        SampleFormat::U16 => Some(59),
        SampleFormat::I8 => Some(50),
        SampleFormat::U8 => Some(49),
        _ => None,
    }
}

/// Rate this range will run at: the target if covered, else the nearest edge.
fn stream_sample_rate(range: &cpal::SupportedStreamConfigRange, target: u32) -> u32 {
    target.clamp(range.min_sample_rate(), range.max_sample_rate())
}

pub(crate) fn ranked_configs(
    ranges: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    channels: u16,
    sample_rate: u32,
) -> Vec<cpal::SupportedStreamConfigRange> {
    let mut keyed = ranges
        .filter_map(|cfg| {
            let distance = sample_rate.abs_diff(stream_sample_rate(&cfg, sample_rate));
            let rank = sample_format_rank(cfg.sample_format())?;
            Some((
                ((cfg.channels() != channels) as u8, distance, Reverse(rank)),
                cfg,
            ))
        })
        .collect::<Vec<((u8, u32, Reverse<u8>), cpal::SupportedStreamConfigRange)>>();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, cfg)| cfg).collect()
}

fn stream_config(
    direction: &str,
    cfg_range: cpal::SupportedStreamConfigRange,
    audio_config: &AudioPipelineConfig,
) -> (SampleFormat, StreamConfig) {
    let sample_rate = stream_sample_rate(&cfg_range, audio_config.sample_rate());
    let cfg = StreamConfig {
        channels: cfg_range.channels(),
        sample_rate,
        buffer_size: callback_buffer_size(host_name(), cfg_range.buffer_size()),
    };
    log::trace!(
        "Candidate {direction} stream: {}ch {}Hz {:?} from device range {}..{}Hz",
        cfg.channels,
        cfg.sample_rate,
        cfg_range.sample_format(),
        cfg_range.min_sample_rate(),
        cfg_range.max_sample_rate(),
    );
    (cfg_range.sample_format(), cfg)
}

pub(crate) fn get_input_config(
    device: &Device,
    audio_config: &AudioPipelineConfig,
) -> anyhow::Result<Vec<(SampleFormat, StreamConfig)>> {
    let range = device
        .supported_input_configs()
        .map_err(|e| anyhow::anyhow!(e))?;
    let ranked = ranked_configs(range, audio_config.channels(), audio_config.sample_rate());
    if ranked.is_empty() {
        anyhow::bail!(
            "No supported input config for {}ch @ {}Hz",
            audio_config.channels(),
            audio_config.sample_rate()
        );
    }
    let configs: Vec<(SampleFormat, StreamConfig)> = ranked
        .into_iter()
        .map(|cfg_range| stream_config("input", cfg_range, audio_config))
        .collect();
    Ok(configs)
}

pub(crate) fn get_output_config(
    device: &Device,
    audio_config: &AudioPipelineConfig,
) -> anyhow::Result<Vec<(SampleFormat, StreamConfig)>> {
    let range = device
        .supported_output_configs()
        .map_err(|e| anyhow::anyhow!(e))?;
    let ranked = ranked_configs(range, audio_config.channels(), audio_config.sample_rate());
    if ranked.is_empty() {
        anyhow::bail!(
            "No supported output config for {}ch @ {}Hz",
            audio_config.channels(),
            audio_config.sample_rate()
        );
    }
    let configs: Vec<(SampleFormat, StreamConfig)> = ranked
        .into_iter()
        .map(|cfg_range| stream_config("output", cfg_range, audio_config))
        .collect();
    Ok(configs)
}

#[cfg(test)]
mod buffer_size_tests {
    use cpal::{BufferSize, SupportedBufferSize};

    use super::{AUDIO_CALLBACK_FRAMES, callback_buffer_size};

    #[test]
    fn pulseaudio_requests_callback_frames_within_range() {
        let wide = SupportedBufferSize::Range {
            min: 1,
            max: 1 << 20,
        };
        assert_eq!(
            callback_buffer_size("PulseAudio", &wide),
            BufferSize::Fixed(AUDIO_CALLBACK_FRAMES)
        );
        let narrow = SupportedBufferSize::Range {
            min: 1024,
            max: 4096,
        };
        assert_eq!(
            callback_buffer_size("PulseAudio", &narrow),
            BufferSize::Fixed(1024)
        );
    }

    #[test]
    fn pulseaudio_without_range_uses_default() {
        assert_eq!(
            callback_buffer_size("PulseAudio", &SupportedBufferSize::Unknown),
            BufferSize::Default
        );
    }

    #[test]
    fn other_hosts_use_default_even_with_range() {
        let range = SupportedBufferSize::Range { min: 480, max: 480 };
        for host in ["WASAPI", "CoreAudio", "ALSA", "PipeWire", "JACK"] {
            assert_eq!(
                callback_buffer_size(host, &range),
                BufferSize::Default,
                "{host}"
            );
        }
    }
}

#[cfg(test)]
mod format_selection_tests {
    use cpal::SampleFormat;

    use super::{ranked_configs, sample_format_rank, stream_sample_rate};

    fn range(channels: u16, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            44100,
            48000,
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    fn single_rate(rate: u32, format: SampleFormat) -> cpal::SupportedStreamConfigRange {
        single_rate_ch(2, rate, format)
    }

    fn single_rate_ch(
        channels: u16,
        rate: u32,
        format: SampleFormat,
    ) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            rate,
            rate,
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn wasapi_style_per_rate_list_picks_target_rate() {
        let rates = [
            8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000, 88200, 96000, 192000,
        ];
        let configs: Vec<_> = rates
            .iter()
            .map(|&r| single_rate(r, SampleFormat::F32))
            .collect();
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        let picked = ranked[0];
        assert_eq!(picked.min_sample_rate(), 48000);
        assert_eq!(picked.max_sample_rate(), 48000);
        assert_eq!(stream_sample_rate(&picked, 48000), 48000);
    }

    #[test]
    fn missing_target_rate_picks_closest() {
        let configs = vec![
            single_rate(8000, SampleFormat::F32),
            single_rate(16000, SampleFormat::F32),
            single_rate(44100, SampleFormat::F32),
            single_rate(96000, SampleFormat::F32),
        ];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        let picked = ranked[0];
        assert_eq!(picked.max_sample_rate(), 44100);
        assert_eq!(stream_sample_rate(&picked, 48000), 44100);
    }

    #[test]
    fn exact_rate_beats_sample_format() {
        let configs = vec![
            single_rate(44100, SampleFormat::F32),
            single_rate(48000, SampleFormat::I16),
        ];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        let picked = ranked[0];
        assert_eq!(picked.sample_format(), SampleFormat::I16);
        assert_eq!(picked.max_sample_rate(), 48000);
    }

    #[test]
    fn stream_rate_clamps_into_range() {
        let high = cpal::SupportedStreamConfigRange::new(
            2,
            96000,
            192000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        assert_eq!(stream_sample_rate(&high, 48000), 96000);
        assert_eq!(
            stream_sample_rate(&single_rate(8000, SampleFormat::F32), 48000),
            8000
        );
        assert_eq!(
            stream_sample_rate(&range(2, SampleFormat::F32), 48000),
            48000
        );
    }

    #[test]
    fn fidelity_ordering_prefers_float_then_width_then_signed() {
        let ranked = [
            SampleFormat::F32,
            SampleFormat::F64,
            SampleFormat::I32,
            SampleFormat::U32,
            SampleFormat::I16,
            SampleFormat::U16,
            SampleFormat::I8,
            SampleFormat::U8,
        ];
        let scores: Vec<u8> = ranked
            .iter()
            .map(|f| sample_format_rank(*f).expect("usable"))
            .collect();
        let mut ordered = scores.clone();
        ordered.sort();
        ordered.reverse();
        assert_eq!(scores, ordered);
    }

    #[test]
    fn packed_and_dsd_formats_are_unusable() {
        for format in [
            SampleFormat::I24,
            SampleFormat::U24,
            SampleFormat::DsdU8,
            SampleFormat::DsdU16,
            SampleFormat::DsdU32,
        ] {
            assert_eq!(sample_format_rank(format), None);
        }
    }

    #[test]
    fn u8_first_list_selects_f32() {
        let configs = vec![
            range(2, SampleFormat::U8),
            range(2, SampleFormat::I16),
            range(2, SampleFormat::F32),
        ];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        assert_eq!(ranked[0].sample_format(), SampleFormat::F32);
        assert_eq!(ranked[0].channels(), 2);
        let configs = vec![
            range(2, SampleFormat::U8),
            range(2, SampleFormat::I16),
            range(2, SampleFormat::F32),
        ];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        assert_eq!(ranked[0].sample_format(), SampleFormat::F32);
    }

    #[test]
    fn mono_configs_never_win_over_stereo() {
        let configs = vec![range(1, SampleFormat::F32), range(2, SampleFormat::U8)];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        assert_eq!(ranked[0].channels(), 2);
        assert_eq!(ranked[0].sample_format(), SampleFormat::U8);
    }

    #[test]
    fn dsd_stereo_is_skipped_for_fallback() {
        let configs = vec![range(2, SampleFormat::DsdU8), range(1, SampleFormat::F32)];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        assert_eq!(ranked.len(), 1);
        assert_ne!(ranked[0].sample_format(), SampleFormat::DsdU8);
    }

    #[test]
    fn empty_list_selects_nothing() {
        assert!(ranked_configs(Vec::new().into_iter(), 2, 48000).is_empty());
        assert!(ranked_configs(Vec::new().into_iter(), 2, 48000).is_empty());
    }

    #[test]
    fn fidelity_tie_keeps_first_range() {
        let first = cpal::SupportedStreamConfigRange::new(
            2,
            44100,
            48000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        let second = cpal::SupportedStreamConfigRange::new(
            2,
            8000,
            96000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        let ranked = ranked_configs(vec![first, second].into_iter(), 2, 48000);
        assert_eq!(ranked[0].max_sample_rate(), 48000);
    }

    #[test]
    fn mono_only_device_falls_back_to_mono() {
        let configs = vec![
            single_rate_ch(1, 48000, SampleFormat::F64),
            single_rate_ch(1, 48000, SampleFormat::F32),
            single_rate_ch(1, 48000, SampleFormat::I16),
        ];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        assert_eq!(ranked.len(), 3);
        assert!(ranked.iter().all(|r| r.channels() == 1));
        assert_eq!(ranked[0].sample_format(), SampleFormat::F32);
        assert_eq!(ranked[1].sample_format(), SampleFormat::F64);
        assert_eq!(ranked[2].sample_format(), SampleFormat::I16);
    }

    #[test]
    fn tyler_case_f64_first_still_offers_f32_fallback() {
        let configs = vec![
            single_rate_ch(1, 48000, SampleFormat::F64),
            single_rate_ch(1, 48000, SampleFormat::F32),
        ];
        let ranked = ranked_configs(configs.into_iter(), 2, 48000);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].sample_format(), SampleFormat::F32);
        assert_eq!(ranked[1].sample_format(), SampleFormat::F64);
    }
}
