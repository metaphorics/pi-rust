use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use super::types::{DeviceCodePoll, OAuthError};

pub const CANCEL_MESSAGE: &str = "Login cancelled";
pub const TIMEOUT_MESSAGE: &str = "Device flow timed out";
pub const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
pub const MINIMUM_INTERVAL: Duration = Duration::from_secs(1);
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const SLOW_DOWN_INTERVAL_INCREMENT: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub struct CancellationFlag(Arc<AtomicBool>);

impl CancellationFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Default)]
pub struct DeviceCodePollOptions {
    pub interval: Option<Duration>,
    pub expires_in: Option<Duration>,
    pub wait_before_first_poll: bool,
    pub cancellation: Option<CancellationFlag>,
}

pub async fn poll_oauth_device_code_flow<T, F, Fut>(
    options: DeviceCodePollOptions,
    mut poll: F,
) -> Result<T, OAuthError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<DeviceCodePoll<T>, OAuthError>>,
{
    let started = Instant::now();
    let expires_in = options.expires_in.unwrap_or(Duration::MAX);
    let mut interval = options
        .interval
        .unwrap_or(DEFAULT_POLL_INTERVAL)
        .max(MINIMUM_INTERVAL);
    let mut slow_down_responses = 0_u32;

    if options.wait_before_first_poll {
        sleep_until_next(interval, started, expires_in, options.cancellation.as_ref()).await?;
    }

    while started.elapsed() < expires_in {
        if options
            .cancellation
            .as_ref()
            .is_some_and(CancellationFlag::is_cancelled)
        {
            return Err(OAuthError::Other(CANCEL_MESSAGE.into()));
        }
        match poll().await? {
            DeviceCodePoll::Complete(value) => return Ok(value),
            DeviceCodePoll::Failed(message) => return Err(OAuthError::Other(message)),
            DeviceCodePoll::Pending => {}
            DeviceCodePoll::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                interval = interval_seconds
                    .map(Duration::from_secs)
                    .unwrap_or(interval + SLOW_DOWN_INTERVAL_INCREMENT)
                    .max(MINIMUM_INTERVAL);
            }
        }
        sleep_until_next(interval, started, expires_in, options.cancellation.as_ref()).await?;
    }

    Err(OAuthError::Other(
        if slow_down_responses > 0 {
            SLOW_DOWN_TIMEOUT_MESSAGE
        } else {
            TIMEOUT_MESSAGE
        }
        .into(),
    ))
}

async fn sleep_until_next(
    interval: Duration,
    started: Instant,
    expires_in: Duration,
    cancellation: Option<&CancellationFlag>,
) -> Result<(), OAuthError> {
    let remaining = expires_in.saturating_sub(started.elapsed());
    let sleep = tokio::time::sleep(interval.min(remaining));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(25)) => {
                if cancellation.is_some_and(CancellationFlag::is_cancelled) {
                    return Err(OAuthError::Other(CANCEL_MESSAGE.into()));
                }
            }
        }
    }
}
