use insanity_core::audio::device::AudioDevice;
use insanity_tui_adapter::AppEvent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::cpal_registry::enumerate_devices;
use super::input::InputManager;
use super::output::OutputManager;

pub type DeviceList = Vec<(String, String)>;

pub fn device_lists() -> (DeviceList, DeviceList) {
    let (inputs, outputs) = enumerate_devices();
    (
        inputs
            .iter()
            .filter_map(|device| Some((device.try_id()?, device.try_name()?)))
            .collect(),
        outputs
            .iter()
            .filter_map(|device| Some((device.try_id()?, device.try_name()?)))
            .collect(),
    )
}

pub fn refresh_device_events(input: &InputManager, output: &OutputManager) -> Vec<AppEvent> {
    let (inputs, outputs) = device_lists();
    vec![
        AppEvent::SetInputDevices(inputs),
        AppEvent::SetOutputDevices(outputs),
        AppEvent::SetInputDeviceName(input.current_name()),
        AppEvent::SetOutputDeviceName(output.current_name()),
    ]
}

fn send_refresh(
    app_event_tx: &Option<mpsc::UnboundedSender<AppEvent>>,
    input: &InputManager,
    output: &OutputManager,
) {
    let Some(tx) = app_event_tx else {
        return;
    };
    for event in refresh_device_events(input, output) {
        if tx.send(event).is_err() {
            log::warn!("Could not send device refresh to UI");
            break;
        }
    }
}

pub async fn run_device_supervisor(
    mut input: InputManager,
    mut output: OutputManager,
    app_event_tx: Option<mpsc::UnboundedSender<AppEvent>>,
    cancel: CancellationToken,
) {
    let input_fatal = input.fatal_signal();
    let output_fatal = output.fatal_signal();
    loop {
        tokio::select! {
            _ = input_fatal.notified() => {
                if input_fatal.generation() == input.generation() {
                    log::info!("Input stream failed, following default device");
                    input.follow_default();
                    send_refresh(&app_event_tx, &input, &output);
                } else {
                    log::debug!(
                        "Ignoring stale input failure for generation {}",
                        input_fatal.generation()
                    );
                }
            }
            _ = output_fatal.notified() => {
                if output_fatal.generation() == output.generation() {
                    log::info!("Output stream failed, following default device");
                    output.follow_default();
                    send_refresh(&app_event_tx, &input, &output);
                } else {
                    log::debug!(
                        "Ignoring stale output failure for generation {}",
                        output_fatal.generation()
                    );
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{refresh_device_events, run_device_supervisor};
    use insanity_core::audio::config::AudioPipelineConfig;
    use insanity_tui_adapter::AppEvent;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::super::input::{InputManager, start_input};
    use super::super::output::{OutputManager, start_output};

    fn managers() -> (InputManager, OutputManager) {
        let config = AudioPipelineConfig::default();
        (start_input(config).manager, start_output(config).manager)
    }

    #[tokio::test]
    async fn refresh_lists_devices_then_names() {
        let (input, output) = managers();
        let events = refresh_device_events(&input, &output);
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], AppEvent::SetInputDevices(_)));
        assert!(matches!(events[1], AppEvent::SetOutputDevices(_)));
        let AppEvent::SetInputDeviceName(input_name) = &events[2] else {
            panic!("expected input name third, got {:?}", events[2]);
        };
        let AppEvent::SetOutputDeviceName(output_name) = &events[3] else {
            panic!("expected output name fourth, got {:?}", events[3]);
        };
        assert_eq!(*input_name, input.current_name());
        assert_eq!(*output_name, output.current_name());
    }

    #[tokio::test]
    async fn fresh_fatal_rebuilds_and_refreshes_ui() {
        let (input, output) = managers();
        input.fatal_signal().signal(input.generation());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_device_supervisor(
            input,
            output,
            Some(tx),
            cancel.clone(),
        ));
        let mut events = Vec::new();
        for _ in 0..4 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("supervisor refreshes UI after fresh fatal")
                .expect("sender alive");
            events.push(event);
        }
        cancel.cancel();
        task.await.expect("supervisor shuts down");
        assert!(matches!(events[0], AppEvent::SetInputDevices(_)));
        assert!(matches!(events[1], AppEvent::SetOutputDevices(_)));
        let AppEvent::SetInputDeviceName(input_name) = &events[2] else {
            panic!("expected input name third, got {:?}", events[2]);
        };
        let AppEvent::SetOutputDeviceName(output_name) = &events[3] else {
            panic!("expected output name fourth, got {:?}", events[3]);
        };
        assert!(!input_name.is_empty());
        assert!(!output_name.is_empty());
    }

    #[tokio::test]
    async fn stale_fatal_sends_no_events() {
        let (input, output) = managers();
        input
            .fatal_signal()
            .signal(input.generation().wrapping_add(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_device_supervisor(
            input,
            output,
            Some(tx),
            cancel.clone(),
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
        cancel.cancel();
        task.await.expect("supervisor shuts down");
    }
}
