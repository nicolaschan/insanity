use std::sync::{Mutex, MutexGuard};

pub mod codec;
pub mod config;
pub mod cpal_registry;
pub mod cpal_stream_receiver;
pub mod denoise;
pub mod hub;
pub mod mixer;
pub mod output;
pub mod params;

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
