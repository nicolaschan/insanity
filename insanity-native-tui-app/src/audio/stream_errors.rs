use std::sync::atomic::{AtomicUsize, Ordering};

use cpal::ErrorKind;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamErrorCounts {
    pub(crate) xruns: usize,
    pub(crate) other: usize,
}

impl StreamErrorCounts {
    pub(crate) fn since(self, earlier: StreamErrorCounts) -> StreamErrorCounts {
        StreamErrorCounts {
            xruns: self.xruns.saturating_sub(earlier.xruns),
            other: self.other.saturating_sub(earlier.other),
        }
    }

    pub(crate) fn is_zero(self) -> bool {
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

/// Safe on a real-time audio thread: one relaxed atomic add, no I/O.
pub(crate) fn note_input_error(kind: ErrorKind) {
    INPUT.note(kind);
}

/// Safe on a real-time audio thread: one relaxed atomic add, no I/O.
pub(crate) fn note_output_error(kind: ErrorKind) {
    OUTPUT.note(kind);
}

pub(crate) fn input_errors() -> StreamErrorCounts {
    INPUT.snapshot()
}

pub(crate) fn output_errors() -> StreamErrorCounts {
    OUTPUT.snapshot()
}

#[cfg(test)]
mod tests {
    use super::{
        StreamErrorCounts, input_errors, note_input_error, note_output_error, output_errors,
    };
    use cpal::ErrorKind;

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn xruns_and_other_errors_are_counted_separately() {
        let _guard = SERIAL.lock().unwrap();
        let input_before = input_errors();
        let output_before = output_errors();
        note_input_error(ErrorKind::Xrun);
        note_input_error(ErrorKind::Xrun);
        note_input_error(ErrorKind::RealtimeDenied);
        note_output_error(ErrorKind::DeviceNotAvailable);
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
}
