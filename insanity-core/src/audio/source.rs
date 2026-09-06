use std::future::Future;

pub trait AudioSource {
    fn next(&mut self) -> impl Future<Output = Option<f32>> + Send;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
}

pub trait SyncAudioSource: AudioSource {
    fn next_sync(&mut self) -> Option<f32>;
}
