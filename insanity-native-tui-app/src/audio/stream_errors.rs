use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use cpal::ErrorKind;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamErrorCounts {
    pub xruns: usize,
    pub other: usize,
}

impl StreamErrorCounts {
    pub fn since(self, earlier: StreamErrorCounts) -> StreamErrorCounts {
        StreamErrorCounts {
            xruns: self.xruns.saturating_sub(earlier.xruns),
            other: self.other.saturating_sub(earlier.other),
        }
    }

    pub fn is_zero(self) -> bool {
        self.xruns == 0 && self.other == 0
    }
}

struct Counters {
    xruns: AtomicUsize,
    other: AtomicUsize,
}

impl Counters {
    const fn new() -> Self {
        Counters {
            xruns: AtomicUsize::new(0),
            other: AtomicUsize::new(0),
        }
    }

    fn note(&self, kind: ErrorKind) {
        match kind {
            ErrorKind::Xrun => self.xruns.fetch_add(1, Ordering::Relaxed),
            _ => self.other.fetch_add(1, Ordering::Relaxed),
        };
    }

    fn snapshot(&self) -> StreamErrorCounts {
        StreamErrorCounts {
            xruns: self.xruns.load(Ordering::Relaxed),
            other: self.other.load(Ordering::Relaxed),
        }
    }
}

static INPUT: Counters = Counters::new();
static OUTPUT: Counters = Counters::new();

pub fn is_fatal(kind: ErrorKind) -> bool {
    !matches!(
        kind,
        ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
    )
}

pub struct FatalSignal {
    generation: AtomicU64,
    notify: Notify,
}

impl FatalSignal {
    pub fn new() -> Self {
        FatalSignal {
            generation: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    pub fn signal(&self, generation: u64) {
        self.generation.store(generation, Ordering::Relaxed);
        self.notify.notify_one();
    }

    pub fn notified(&self) -> impl Future<Output = ()> + '_ {
        self.notify.notified()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

impl Default for FatalSignal {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct FatalReporter {
    fatal: Arc<FatalSignal>,
    generation: u64,
}

impl FatalReporter {
    pub fn new(fatal: Arc<FatalSignal>, generation: u64) -> Self {
        FatalReporter { fatal, generation }
    }

    pub fn report_input(&self, kind: ErrorKind) {
        note_input_error(kind, &self.fatal, self.generation);
    }

    pub fn report_output(&self, kind: ErrorKind) {
        note_output_error(kind, &self.fatal, self.generation);
    }
}

/// Safe on a real-time audio thread
pub fn note_input_error(kind: ErrorKind, fatal: &Arc<FatalSignal>, generation: u64) {
    INPUT.note(kind);
    if is_fatal(kind) {
        fatal.signal(generation);
    }
}

/// Safe on a real-time audio thread
pub fn note_output_error(kind: ErrorKind, fatal: &Arc<FatalSignal>, generation: u64) {
    OUTPUT.note(kind);
    if is_fatal(kind) {
        fatal.signal(generation);
    }
}

pub fn input_errors() -> StreamErrorCounts {
    INPUT.snapshot()
}

pub fn output_errors() -> StreamErrorCounts {
    OUTPUT.snapshot()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        FatalSignal, StreamErrorCounts, input_errors, is_fatal, note_input_error,
        note_output_error, output_errors,
    };
    use cpal::ErrorKind;

    fn test_fatal() -> Arc<FatalSignal> {
        Arc::new(FatalSignal::new())
    }

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn xruns_and_other_errors_are_counted_separately() {
        let _guard = SERIAL.lock().unwrap();
        let input_before = input_errors();
        let output_before = output_errors();
        note_input_error(ErrorKind::Xrun, &test_fatal(), 1);
        note_input_error(ErrorKind::Xrun, &test_fatal(), 1);
        note_input_error(ErrorKind::RealtimeDenied, &test_fatal(), 1);
        note_output_error(ErrorKind::DeviceNotAvailable, &test_fatal(), 1);
        assert_eq!(
            input_errors().since(input_before),
            StreamErrorCounts { xruns: 2, other: 1 }
        );
        assert_eq!(
            output_errors().since(output_before),
            StreamErrorCounts { xruns: 0, other: 1 }
        );
    }

    #[test]
    fn only_recoverable_errors_are_non_fatal() {
        assert!(!is_fatal(ErrorKind::Xrun));
        assert!(!is_fatal(ErrorKind::DeviceChanged));
        assert!(!is_fatal(ErrorKind::RealtimeDenied));
        assert!(is_fatal(ErrorKind::DeviceNotAvailable));
        assert!(is_fatal(ErrorKind::DeviceBusy));
    }

    #[tokio::test]
    async fn fatal_errors_wake_the_owner_with_generation() {
        let fatal = test_fatal();
        {
            let _guard = SERIAL.lock().unwrap();
            note_input_error(ErrorKind::DeviceNotAvailable, &fatal, 3);
        }
        assert_eq!(fatal.generation(), 3);
        fatal.notified().await;
        let quiet = test_fatal();
        {
            let _guard = SERIAL.lock().unwrap();
            note_output_error(ErrorKind::Xrun, &quiet, 7);
        }
        assert_eq!(quiet.generation(), 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), quiet.notified())
                .await
                .is_err()
        );
    }
    #[test]
    fn since_saturates_and_reports_zero() {
        let later = StreamErrorCounts { xruns: 1, other: 1 };
        let earlier = StreamErrorCounts { xruns: 3, other: 5 };
        assert!(later.since(earlier).is_zero());
        assert!(!earlier.since(later).is_zero());
    }
}
