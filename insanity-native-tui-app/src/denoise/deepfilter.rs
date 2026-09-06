use df::tract::{DfParams, DfTract, RuntimeParams};
use insanity_core::audio::denoiser::Denoiser;
use ndarray::{ArrayView2, ArrayViewMut2};
use send_safe::SendWrapperThread;

use crate::processor::AUDIO_CHUNK_SIZE;

pub struct DeepfilterDenoiser(SendWrapperThread<DfTract>);

impl Denoiser for DeepfilterDenoiser {
    const FRAME_SIZE: usize = AUDIO_CHUNK_SIZE;

    fn init() -> Self {
        let wrapper = SendWrapperThread::new(|| {
            let params = DfParams::default();
            let rt = RuntimeParams::default_with_ch(1);
            DfTract::new(params, &rt).unwrap()
        });
        DeepfilterDenoiser(wrapper)
    }

    fn process_frame(&mut self, output: &mut [f32], input: &[f32]) {
        let input = input.to_vec();
        let denoised = self
            .0
            .execute(move |df_tract| {
                let input = ArrayView2::from_shape((1, input.len()), &input).unwrap();
                let mut output_buf = vec![0.0; input.len()];
                let output_array =
                    ArrayViewMut2::from_shape((1, input.len()), &mut output_buf).unwrap();
                df_tract.process(input, output_array).unwrap();
                output_buf
            })
            .unwrap();
        output.copy_from_slice(&denoised);
    }
}
