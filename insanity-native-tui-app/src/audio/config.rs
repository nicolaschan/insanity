use std::time::Duration;

use cpal::{
    Device, SampleFormat, SupportedStreamConfig, SupportedStreamConfigRange, traits::DeviceTrait,
};

use insanity_core::audio::config::AudioPipelineConfig;

pub(crate) const STREAM_BUILD_TIMEOUT: Duration = Duration::from_secs(5);

/// Return a config that matches audio_config channels, sample rate,
/// and f32 sample format if one exists.
fn find_supported_config<'a>(
    mut ranges: impl Iterator<Item = &'a cpal::SupportedStreamConfigRange>,
    audio_config: &AudioPipelineConfig,
) -> Option<SupportedStreamConfig> {
    ranges.find_map(|r| {
        if r.channels() == audio_config.channels()
            && r.contains_rate(audio_config.sample_rate())
            && r.sample_format() == SampleFormat::F32
        {
            (*r).try_with_sample_rate(audio_config.sample_rate())
        } else {
            None
        }
    })
}

// see cpal_stream.rs
fn is_streamable_format(format: SampleFormat) -> bool {
    !matches!(
        format,
        SampleFormat::DsdU8 | SampleFormat::DsdU16 | SampleFormat::DsdU32
    )
}

fn cpal_config_prioritization(
    mut ranges: Vec<SupportedStreamConfigRange>,
) -> impl Iterator<Item = SupportedStreamConfig> {
    // `cmp_default_heuristics` ranks the best config greatest
    ranges.sort_by(|lhs, rhs| rhs.cmp_default_heuristics(lhs));
    ranges
        .into_iter()
        .filter(|r| is_streamable_format(r.sample_format()))
        .filter_map(|r| r.try_with_standard_sample_rate())
}

fn app_config_prioritization(
    ranges: Vec<SupportedStreamConfigRange>,
    default_config: Option<SupportedStreamConfig>,
    audio_config: &AudioPipelineConfig,
) -> Vec<SupportedStreamConfig> {
    find_supported_config(ranges.iter(), audio_config)
        .into_iter()
        .chain(default_config)
        .chain(cpal_config_prioritization(ranges))
        .collect()
}

/// Return a list of input configs to try in order of preference.
/// 1. Pipeline config if supported
/// 2. OS/device preferred config
/// 3. Cpal ranked supported configs
pub(crate) fn ordered_input_configs(
    device: &Device,
    audio_config: &AudioPipelineConfig,
) -> Vec<SupportedStreamConfig> {
    let default_config = device
        .default_input_config()
        .inspect_err(|e| log::debug!("Failed to get default input config: {e:#}"))
        .ok();
    device
        .supported_input_configs()
        .inspect_err(|e| log::debug!(
            "Failed to get supported input configs: {e:#}. Falling back to default input device if available."
        ))
        .map(|iter| iter.collect::<Vec<_>>())
        .map(|ranges| app_config_prioritization(ranges, default_config, audio_config))
        .unwrap_or(default_config.into_iter().collect())
}

/// Return a list of output configs to try in order of preference.
/// 1. Pipeline config if supported
/// 2. OS/device preferred config
/// 3. Cpal ranked supported configs
pub(crate) fn ordered_output_configs(
    device: &Device,
    audio_config: &AudioPipelineConfig,
) -> Vec<SupportedStreamConfig> {
    let default_config = device
        .default_output_config()
        .inspect_err(|e| log::debug!("Failed to get default output config: {e:#}"))
        .ok();
    device
        .supported_output_configs()
        .inspect_err(|e| log::debug!(
            "Failed to get supported output configs: {e:#}. Falling back to default output device if available."
        ))
        .map(|iter| iter.collect::<Vec<_>>())
        .map(|ranges| app_config_prioritization(ranges, default_config, audio_config))
        .unwrap_or(default_config.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfig};
    use insanity_core::audio::config::AudioPipelineConfig;

    use super::app_config_prioritization;

    fn single_rate_ch(
        channels: u16,
        rate: u32,
        format: SampleFormat,
    ) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            rate,
            rate,
            SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn app_ordering_is_preferred_then_default_then_fallback() {
        let ranges = vec![
            single_rate_ch(2, 48000, SampleFormat::F32),
            single_rate_ch(2, 48000, SampleFormat::U8),
        ];
        let default =
            SupportedStreamConfig::new(1, 44100, SupportedBufferSize::Unknown, SampleFormat::I16);
        let ordered =
            app_config_prioritization(ranges, Some(default), &AudioPipelineConfig::default());
        assert!(ordered.len() >= 3);
        // 1. exact pipeline match.
        assert_eq!(ordered[0].sample_format(), SampleFormat::F32);
        assert_eq!(ordered[0].sample_rate(), 48000);
        assert_eq!(ordered[0].channels(), 2);
        // 2. device default.
        assert_eq!(ordered[1].sample_format(), SampleFormat::I16);
        assert_eq!(ordered[1].sample_rate(), 44100);
        assert_eq!(ordered[1].channels(), 1);
        // 3. fallback tail still offers the remaining U8 candidate.
        assert!(
            ordered[2..]
                .iter()
                .any(|c| c.sample_format() == SampleFormat::U8)
        );
    }
}
