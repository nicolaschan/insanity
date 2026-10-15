use anyhow::Context;
use opus::{Application, Channels, Decoder, Encoder};

use insanity_core::audio::AudioFormat;
use insanity_core::audio::chunk::AudioChunk;
use insanity_core::audio::codec::{AudioCodec, AudioDecoder, EncodedChunk};
use insanity_core::audio::transform::ChunkTransform;

pub(crate) fn u16_to_channels(n: u16) -> Channels {
    match n {
        1 => Channels::Mono,
        2 => Channels::Stereo,
        _ => Channels::Stereo,
    }
}

pub struct OpusEncoder {
    inner: Encoder,
}

impl OpusEncoder {
    pub fn new(format: &AudioFormat) -> Option<Self> {
        Encoder::new(
            format.sample_rate,
            u16_to_channels(format.channel_count),
            Application::Audio,
        )
        .ok()
        .map(|inner| OpusEncoder { inner })
    }
}

impl ChunkTransform for OpusEncoder {
    type OutputT = Option<EncodedChunk>;

    fn transform(&mut self, chunk: AudioChunk) -> Option<EncodedChunk> {
        match self.inner.encode_vec_float(&chunk.audio_data, 65535) {
            Ok(payload) => Some(EncodedChunk {
                sequence_number: chunk.sequence_number,
                codec: AudioCodec::Opus,
                payload,
                format: chunk.format,
            }),
            Err(e) => {
                log::warn!("Opus encode failed: {e:?}");
                None
            }
        }
    }
}

pub fn rebuild_opus_encoder(format: &AudioFormat) -> Option<OpusEncoder> {
    OpusEncoder::new(format)
}

pub struct OpusDecoder {
    inner: Decoder,
    format: AudioFormat,
}

impl OpusDecoder {
    pub fn new(format: AudioFormat) -> Option<Self> {
        Decoder::new(format.sample_rate, u16_to_channels(format.channel_count))
            .ok()
            .map(|inner| OpusDecoder { inner, format })
    }

    fn set_format(&mut self, format: AudioFormat) -> anyhow::Result<()> {
        if self.format == format {
            Ok(())
        } else {
            let decoder = Decoder::new(format.sample_rate, u16_to_channels(format.channel_count))
                .context("Opus decoder rebuild failed")?;
            self.inner = decoder;
            self.format = format;
            Ok(())
        }
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, frame: &EncodedChunk) -> Option<AudioChunk> {
        if frame.codec != AudioCodec::Opus {
            return None;
        };
        if let Err(e) = self.set_format(frame.format.clone()) {
            log::debug!("{:#}", e);
            return None;
        }
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

pub fn rebuild_opus_decoder(format: &AudioFormat) -> Option<OpusDecoder> {
    OpusDecoder::new(format.clone())
}

#[cfg(test)]
mod tests {
    use insanity_core::audio::AudioFormat;
    use insanity_core::audio::chunk::AudioChunk;
    use insanity_core::audio::codec::{AudioCodec, AudioDecoder, EncodedChunk};
    use insanity_core::audio::transform::ChunkTransform;

    use super::{OpusDecoder, OpusEncoder};

    #[test]
    fn opus_roundtrip_preserves_shape() {
        let format = AudioFormat::new(2, 48000);
        let mut encoder = OpusEncoder::new(&format).expect("encoder");
        let mut decoder = OpusDecoder::new(format).expect("decoder");
        let chunk = AudioChunk::new(5, AudioFormat::new(2, 48000), vec![0.4f32; 960]);
        let frame = encoder.transform(chunk).expect("encode");
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
        let format = AudioFormat::new(2, 48000);
        let mut decoder = OpusDecoder::new(format).expect("decoder");
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
        let decoder_format = AudioFormat::new(2, 48000);
        let encoder_format = AudioFormat::new(1, 48000);
        let mut decoder = OpusDecoder::new(decoder_format).expect("decoder");
        let mut encoder = OpusEncoder::new(&encoder_format).expect("encoder");
        let chunk = AudioChunk::new(0, AudioFormat::new(1, 48000), vec![0.4f32; 480]);
        let frame = encoder.transform(chunk).expect("encode");
        let out = decoder.decode(&frame).expect("decode");
        assert_eq!(out.format, AudioFormat::new(1, 48000));
        assert_eq!(out.audio_data.len(), 480);
    }
}
