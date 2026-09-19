use insanity_core::audio::denoiser::Denoiser;
use nnnoiseless::DenoiseState;

pub struct NnnoiselessDenoiser {
    inner: Box<DenoiseState<'static>>,
    scaled_in: Vec<f32>,
    scaled_out: Vec<f32>,
}

impl NnnoiselessDenoiser {
    // Account for measured 6db loss through denoiser
    const MAKEUP_GAIN: f32 = 2.0;
}

impl Denoiser for NnnoiselessDenoiser {
    const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

    fn init() -> Self {
        let model = nnnoiseless::RnnModel::default();
        let denoise_state = DenoiseState::from_model(model);
        NnnoiselessDenoiser {
            inner: denoise_state,
            scaled_in: Vec::with_capacity(DenoiseState::FRAME_SIZE),
            scaled_out: Vec::with_capacity(DenoiseState::FRAME_SIZE),
        }
    }

    fn process_frame(&mut self, output: &mut [f32], input: &[f32]) {
        let magic = 32767.0;
        self.scaled_in.clear();
        self.scaled_in.extend(input.iter().map(|s| s * magic));

        self.scaled_out.clear();
        self.scaled_out.resize(output.len(), 0.0);
        self.inner
            .process_frame(&mut self.scaled_out, &self.scaled_in);
        for (o, s) in output.iter_mut().zip(self.scaled_out.iter()) {
            *o = *s / magic * Self::MAKEUP_GAIN;
        }
    }
}
