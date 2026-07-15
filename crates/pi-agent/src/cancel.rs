//! Cancellation token — AbortSignal analog for the agent loop.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Shared abort flag for a single agent run.
///
/// Mirrors the observable surface of DOM `AbortSignal` used by pi's agent loop:
/// tools and hooks consult [`is_cancelled`](Self::is_cancelled); the host calls
/// [`cancel`](Self::cancel).
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}
