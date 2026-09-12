#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPipelineConfig {
    sample_rate: u32,
    channels: u16,
    frames: usize,
    jitter_chunks: usize,
}

impl Default for AudioPipelineConfig {
    fn default() -> Self {
        AudioPipelineConfig {
            sample_rate: 48000,
            channels: 2,
            frames: 480,
            jitter_chunks: 10,
        }
    }
}

impl AudioPipelineConfig {
    pub fn try_new(
        sample_rate: u32,
        channels: u16,
        frames: usize,
        jitter_chunks: usize,
    ) -> Result<Self, &'static str> {
        if sample_rate == 0 {
            return Err("sample_rate must be > 0");
        }
        if channels == 0 {
            return Err("channels must be > 0");
        }
        if frames == 0 {
            return Err("frames must be > 0");
        }
        if jitter_chunks == 0 {
            return Err("jitter_chunks must be > 0");
        }
        let nanos = frames as u128 * 1_000_000_000 / sample_rate as u128;
        if nanos == 0 {
            return Err("frames/sample_rate yields zero period");
        }
        if nanos > u64::MAX as u128 {
            return Err("period overflows");
        }
        Ok(AudioPipelineConfig {
            sample_rate,
            channels,
            frames,
            jitter_chunks,
        })
    }

    pub fn with_jitter_chunks(self, jitter_chunks: usize) -> Result<Self, &'static str> {
        Self::try_new(self.sample_rate, self.channels, self.frames, jitter_chunks)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn jitter_chunks(&self) -> usize {
        self.jitter_chunks
    }

    pub fn chunk_period(&self) -> std::time::Duration {
        debug_assert!(self.sample_rate > 0);
        debug_assert!(self.frames > 0);
        std::time::Duration::from_nanos(
            self.frames as u64 * 1_000_000_000 / self.sample_rate as u64,
        )
    }

    pub fn block_samples(&self) -> usize {
        debug_assert!(self.channels > 0);
        debug_assert!(self.frames > 0);
        self.channels as usize * self.frames
    }
}

#[cfg(test)]
mod tests {
    use super::AudioPipelineConfig;

    #[test]
    fn default_pins_current_behavior() {
        let audio_config = AudioPipelineConfig::default();
        assert_eq!(audio_config.sample_rate(), 48000);
        assert_eq!(audio_config.channels(), 2);
        assert_eq!(audio_config.frames(), 480);
        assert_eq!(audio_config.jitter_chunks(), 10);
        assert_eq!(
            audio_config.chunk_period(),
            std::time::Duration::from_millis(10)
        );
        assert_eq!(audio_config.block_samples(), 960);
    }

    #[test]
    fn zero_sample_rate_rejected() {
        assert_eq!(
            AudioPipelineConfig::try_new(0, 2, 480, 10),
            Err("sample_rate must be > 0")
        );
    }

    #[test]
    fn zero_channels_rejected() {
        assert_eq!(
            AudioPipelineConfig::try_new(48000, 0, 480, 10),
            Err("channels must be > 0")
        );
    }

    #[test]
    fn zero_frames_rejected() {
        assert_eq!(
            AudioPipelineConfig::try_new(48000, 2, 0, 10),
            Err("frames must be > 0")
        );
    }

    #[test]
    fn zero_jitter_chunks_rejected() {
        assert_eq!(
            AudioPipelineConfig::try_new(48000, 2, 480, 0),
            Err("jitter_chunks must be > 0")
        );
    }

    #[test]
    fn nanos_precision_preserved() {
        let audio_config = AudioPipelineConfig::try_new(44100, 2, 480, 10).expect("valid config");
        assert_eq!(
            audio_config.chunk_period(),
            std::time::Duration::from_nanos(10_884_353)
        );
    }

    #[test]
    fn zero_period_rejected() {
        assert_eq!(
            AudioPipelineConfig::try_new(u32::MAX, 2, 1, 10),
            Err("frames/sample_rate yields zero period")
        );
    }
}
