use std::sync::{Mutex, MutexGuard};

pub mod codec;
pub(crate) mod config;
pub(crate) mod cpal_registry;
pub(crate) mod cpal_stream;
pub(crate) mod cpal_stream_receiver;
pub mod denoise;
pub(crate) mod device_supervisor;
pub(crate) mod handoff;
pub mod hub;
pub(crate) mod input;
pub mod mixer;
pub(crate) mod output;
pub(crate) mod stream_errors;

/// Lock a mutex, recovering from poisoning with an error log instead of
/// panicking. Audio must stay alive: a poisoned lock means a previous holder
/// panicked, so we reclaim the guard and keep going.
pub(crate) fn lock<'a, T>(m: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log::error!("{what} mutex poisoned, recovering");
            poisoned.into_inner()
        }
    }
}
