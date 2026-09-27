use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// Safe on a real-time audio thread
pub fn note_input_error(kind: ErrorKind, fatal: &Arc<Notify>) {
    INPUT.note(kind);
    if is_fatal(kind) {
        fatal.notify_one();
    }
}

/// Safe on a real-time audio thread
pub fn note_output_error(kind: ErrorKind, fatal: &Arc<Notify>) {
    OUTPUT.note(kind);
    if is_fatal(kind) {
        fatal.notify_one();
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
        StreamErrorCounts, input_errors, is_fatal, note_input_error, note_output_error,
        output_errors,
    };
    use cpal::ErrorKind;
    use tokio::sync::Notify;

    fn test_fatal() -> Arc<Notify> {
        Arc::new(Notify::new())
    }

    #[test]
    fn xruns_and_other_errors_are_counted_separately() {
        let input_before = input_errors();
        let output_before = output_errors();
        note_input_error(ErrorKind::Xrun, &test_fatal());
        note_input_error(ErrorKind::Xrun, &test_fatal());
        note_input_error(ErrorKind::RealtimeDenied, &test_fatal());
        note_output_error(ErrorKind::DeviceNotAvailable, &test_fatal());
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
    async fn fatal_errors_wake_the_owner() {
        let fatal = test_fatal();
        note_input_error(ErrorKind::DeviceNotAvailable, &fatal);
        fatal.notified().await;
        let quiet = test_fatal();
        note_output_error(ErrorKind::Xrun, &quiet);
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
