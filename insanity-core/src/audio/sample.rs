use crate::audio::AudioFormat;

pub trait SampleSource {
    fn format(&self) -> AudioFormat;
    fn next(&mut self) -> impl Future<Output = Option<f32>> + Send;
}

pub trait SyncSampleSource: SampleSource {
    fn next_sync(&mut self) -> Option<f32>;
}

pub trait SampleSink {
    fn format(&self) -> AudioFormat;
    fn push(&mut self, sample: f32);
}
