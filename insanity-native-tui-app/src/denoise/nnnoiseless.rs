use insanity_core::audio::denoiser::Denoiser;
use nnnoiseless::DenoiseState;

pub struct NnnoiselessDenoiser<'a>(DenoiseState<'a>);

impl<'a> Denoiser for NnnoiselessDenoiser<'a> {
    const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

    fn init() -> Self {
        let model = nnnoiseless::RnnModel::default();
        let denoise_state = DenoiseState::from_model(model);
        NnnoiselessDenoiser(*denoise_state)
    }

    fn process_frame(&mut self, output: &mut [f32], input: &[f32]) {
        let magic = 32767.0;
        let input: Vec<f32> = input.iter().map(|s| s * magic).collect();

        let mut scaled_output = vec![0.0; output.len()];
        self.0.process_frame(&mut scaled_output, &input);
        for (i, scaled_value) in scaled_output.iter().enumerate() {
            output[i] = *scaled_value / magic;
        }
    }
}
