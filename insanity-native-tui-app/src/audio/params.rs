pub const SAMPLE_RATE: u32 = 48000;
pub const CHUNK_SIZE: usize = 480;
pub const CHANNELS: u16 = 2;
pub const MAX_VOLUME: usize = 500;

pub(crate) const CHUNK_PERIOD: std::time::Duration =
    std::time::Duration::from_millis(CHUNK_SIZE as u64 * 1000 / SAMPLE_RATE as u64);
