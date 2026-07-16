//! Explicitly-driven countdown timer ported from `countdown-timer.ts`.

use std::time::{Duration, Instant};

pub struct CountdownTimer {
    deadline: Instant,
    last_seconds: u64,
    on_tick: Box<dyn FnMut(u64)>,
    on_expire: Option<Box<dyn FnOnce()>>,
}

impl CountdownTimer {
    pub fn new(
        timeout: Duration,
        mut on_tick: impl FnMut(u64) + 'static,
        on_expire: impl FnOnce() + 'static,
    ) -> Self {
        let remaining_seconds = timeout.as_millis().div_ceil(1_000) as u64;
        on_tick(remaining_seconds);
        Self {
            deadline: Instant::now() + timeout,
            last_seconds: remaining_seconds,
            on_tick: Box::new(on_tick),
            on_expire: Some(Box::new(on_expire)),
        }
    }

    #[must_use]
    pub fn remaining_seconds(&self) -> u64 {
        self.last_seconds
    }

    /// Advance the countdown. Returns `true` once it has expired.
    pub fn tick(&mut self) -> bool {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let seconds = remaining.as_millis().div_ceil(1_000) as u64;
        if seconds != self.last_seconds {
            self.last_seconds = seconds;
            (self.on_tick)(seconds);
        }
        if remaining.is_zero() {
            if let Some(on_expire) = self.on_expire.take() {
                on_expire();
            }
            return true;
        }
        false
    }
}
