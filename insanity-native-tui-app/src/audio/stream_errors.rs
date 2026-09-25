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

fn is_fatal(kind: ErrorKind) -> bool {
    !matches!(
        kind,
        ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
    )
}

/// Safe on a real-time audio thread
pub fn note_input_error(kind: ErrorKind, fatal: &Notify) {
    INPUT.note(kind);
    if is_fatal(kind) {
        fatal.notify_one();
    }
}

/// Safe on a real-time audio thread
pub fn note_output_error(kind: ErrorKind, fatal: &Notify) {
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
    use super::{
        StreamErrorCounts, input_errors, note_input_error, note_output_error, output_errors,
    };
    use cpal::ErrorKind;
    use std::time::Duration;
    use tokio::sync::Notify;

    #[test]
    fn xruns_and_other_errors_are_counted_separately() {
        let input_before = input_errors();
        let output_before = output_errors();
        let silent = Notify::new();
        note_input_error(ErrorKind::Xrun, &silent);
        note_input_error(ErrorKind::Xrun, &silent);
        note_input_error(ErrorKind::RealtimeDenied, &silent);
        note_output_error(ErrorKind::DeviceNotAvailable, &silent);
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
    fn since_saturates_and_reports_zero() {
        let later = StreamErrorCounts { xruns: 1, other: 1 };
        let earlier = StreamErrorCounts { xruns: 3, other: 5 };
        assert!(later.since(earlier).is_zero());
        assert!(!earlier.since(later).is_zero());
    }

    #[tokio::test]
    async fn fatal_input_wakes_but_live_kinds_stay_silent() {
        let fatal = Notify::new();
        note_input_error(ErrorKind::DeviceNotAvailable, &fatal);
        tokio::time::timeout(Duration::from_secs(1), fatal.notified())
            .await
            .expect("fatal input error must wake");
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::DeviceChanged,
            ErrorKind::RealtimeDenied,
        ] {
            note_input_error(kind, &fatal);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), fatal.notified())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fatal_output_wakes_but_live_kinds_stay_silent() {
        let fatal = Notify::new();
        note_output_error(ErrorKind::StreamInvalidated, &fatal);
        tokio::time::timeout(Duration::from_secs(1), fatal.notified())
            .await
            .expect("fatal output error must wake");
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::DeviceChanged,
            ErrorKind::RealtimeDenied,
        ] {
            note_output_error(kind, &fatal);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), fatal.notified())
                .await
                .is_err()
        );
    }
}
