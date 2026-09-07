pub trait Resampler: Send {
    fn input_block_frames(&self) -> usize;
    fn resample(&mut self, input: &[f32]) -> Vec<f32>;
}
