macro_rules! sample_format_dispatch {
    ($format:expr, $run:ident, $($arg:expr),* $(,)?) => {
        match $format {
            ::cpal::SampleFormat::I8 => $run::<i8>($($arg),*),
            ::cpal::SampleFormat::I16 => $run::<i16>($($arg),*),
            ::cpal::SampleFormat::I32 => $run::<i32>($($arg),*),
            ::cpal::SampleFormat::I64 => $run::<i64>($($arg),*),
            ::cpal::SampleFormat::U8 => $run::<u8>($($arg),*),
            ::cpal::SampleFormat::U16 => $run::<u16>($($arg),*),
            ::cpal::SampleFormat::U32 => $run::<u32>($($arg),*),
            ::cpal::SampleFormat::U64 => $run::<u64>($($arg),*),
            ::cpal::SampleFormat::F32 => $run::<f32>($($arg),*),
            ::cpal::SampleFormat::F64 => $run::<f64>($($arg),*),
            other => Err(::anyhow::anyhow!("unsupported sample format {other:?}")),
        }
    };
}

pub(crate) use sample_format_dispatch;
