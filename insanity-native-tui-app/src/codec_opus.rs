use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::codec::{AudioCodec, AudioDecoder, AudioEncoder, EncodedChunk};
use opus::{Application, Channels, Decoder, Encoder};

pub(crate) fn u16_to_channels(n: u16) -> Channels {
    match n {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        _ => Channels::Stereo,
    }
}

pub struct OpusEncoder {
    inner: Encoder,
    format: AudioFormat,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u16) -> Option<Self> {
        let Ok(inner) = Encoder::new(sample_rate, u16_to_channels(channels), Application::Audio)
        else {
            return None;
        };
        Some(OpusEncoder {
            inner,
            format: AudioFormat::new(channels, sample_rate),
        })
    }

    pub fn format(&self) -> &AudioFormat {
        &self.format
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, chunk: &AudioChunk) -> Option<EncodedChunk> {
        match self.inner.encode_vec_float(&chunk.audio_data, 65535) {
            Ok(payload) => Some(EncodedChunk {
                sequence_number: chunk.sequence_number,
                codec: AudioCodec::Opus,
                payload,
                format: chunk.format.clone(),
            }),
            Err(e) => {
                log::warn!("Opus encode failed: {e:?}");
                None
            }
        }
    }
}

pub struct OpusDecoder {
    inner: Decoder,
    format: AudioFormat,
}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u16) -> Option<Self> {
        let Ok(inner) = Decoder::new(sample_rate, u16_to_channels(channels)) else {
            return None;
        };
        Some(OpusDecoder {
            inner,
            format: AudioFormat::new(channels, sample_rate),
        })
    }

    fn set_format(&mut self, format: AudioFormat) -> bool {
        if self.format == format {
            return true;
        }
        let channels = format.channel_count;
        let sample_rate = format.sample_rate;
        match Decoder::new(sample_rate, u16_to_channels(channels)) {
            Ok(inner) => {
                self.inner = inner;
                self.format = format;
                true
            }
            Err(e) => {
                log::warn!("Opus decoder rebuild failed: {e:?}");
                false
            }
        }
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, frame: &EncodedChunk) -> Option<AudioChunk> {
        if frame.codec != AudioCodec::Opus || !self.set_format(frame.format.clone()) {
            return None;
        };
        let Ok(nb) = self.inner.get_nb_samples(&frame.payload[..]) else {
            return None;
        };
        let mut buf = vec![0f32; nb * self.format.channel_count as usize];
        if self
            .inner
            .decode_float(&frame.payload[..], &mut buf[..], false)
            .is_err()
        {
            return None;
        }
        Some(AudioChunk::new(
            frame.sequence_number,
            self.format.clone(),
            buf,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{OpusDecoder, OpusEncoder};
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::AudioChunk;
    use insanity_core::audio::codec::{AudioCodec, AudioDecoder, AudioEncoder, EncodedChunk};

    #[test]
    fn opus_roundtrip_preserves_shape() {
        let mut encoder = OpusEncoder::new(48000, 2).expect("encoder");
        let mut decoder = OpusDecoder::new(48000, 2).expect("decoder");
        let chunk = AudioChunk::new(5, AudioFormat::new(2, 48000), vec![0.4f32; 960]);
        let frame = encoder.encode(&chunk).expect("encode");
        assert_eq!(frame.sequence_number, 5);
        assert_eq!(frame.codec, AudioCodec::Opus);
        assert_eq!(frame.format, AudioFormat::new(2, 48000));
        let out = decoder.decode(&frame).expect("decode");
        assert_eq!(out.sequence_number, 5);
        assert_eq!(out.format, AudioFormat::new(2, 48000));
        assert_eq!(out.audio_data.len(), 960);
        assert!(out.audio_data.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn opus_decoder_rejects_garbage() {
        let mut decoder = OpusDecoder::new(48000, 2).expect("decoder");
        let frame = EncodedChunk {
            sequence_number: 0,
            codec: AudioCodec::Opus,
            payload: vec![0xFF; 10],
            format: AudioFormat::new(2, 48000),
        };
        assert!(decoder.decode(&frame).is_none());
    }

    #[test]
    fn opus_decoder_rebuilds_on_format_change() {
        let mut decoder = OpusDecoder::new(48000, 2).expect("decoder");
        let mut encoder = OpusEncoder::new(48000, 1).expect("encoder");
        let chunk = AudioChunk::new(0, AudioFormat::new(1, 48000), vec![0.4f32; 480]);
        let frame = encoder.encode(&chunk).expect("encode");
        let out = decoder.decode(&frame).expect("decode");
        assert_eq!(out.format, AudioFormat::new(1, 48000));
        assert_eq!(out.audio_data.len(), 480);
    }
}
