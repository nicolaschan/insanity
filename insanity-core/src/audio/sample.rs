pub trait SampleSource {
    fn next(&mut self) -> impl Future<Output = Option<f32>> + Send;
}

pub trait SyncSampleSource: SampleSource {
    fn next_sync(&mut self) -> Option<f32>;
}

pub trait Resampler: Send {
    fn push_sample(&mut self, sample: f32);
    fn pop_sample(&mut self) -> Option<f32>;
}

pub trait SampleSink {
    fn push(&mut self, sample: f32);
}
