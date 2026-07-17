//! Working, retry, compaction, and branch-summary status indicators.

use std::time::{Duration, Instant};

use pi_tui::component::{Component, RenderStatus};
use pi_tui::line::Line;

use super::keybinding_hints::key_text;
use crate::modes::interactive::theme::{ThemeColor, theme};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusIndicatorKind {
    Working,
    Retry,
    Compaction,
    BranchSummary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionStatusReason {
    Manual,
    Threshold,
    Overflow,
}

#[derive(Clone, Copy, Debug)]
struct RetryState {
    attempt: u32,
    max_attempts: u32,
    deadline: Instant,
}

pub struct StatusIndicator {
    pub kind: StatusIndicatorKind,
    message: String,
    started: Instant,
    frame: usize,
    retry: Option<RetryState>,
    /// Extension override for the spinner (frames, interval ms; oracle
    /// `ctx.ui.setWorkingIndicator`). `None` = built-in braille spinner.
    custom_frames: Option<(Vec<String>, u64)>,
    last_tick: Option<Instant>,
    cached: Vec<Line>,
}

impl StatusIndicator {
    #[must_use]
    pub fn working(message: impl Into<String>) -> Self {
        Self::new(StatusIndicatorKind::Working, message)
    }

    #[must_use]
    pub fn retry(attempt: u32, max_attempts: u32, seconds: u64) -> Self {
        let mut status = Self::new(StatusIndicatorKind::Retry, String::new());
        status.retry = Some(RetryState {
            attempt,
            max_attempts,
            deadline: Instant::now() + Duration::from_secs(seconds),
        });
        status.refresh_retry_message();
        status
    }

    #[must_use]
    pub fn compaction(reason: CompactionStatusReason) -> Self {
        let cancel_hint = format!("({} to cancel)", key_text("app.interrupt"));
        let message = match reason {
            CompactionStatusReason::Manual => format!("Compacting context... {cancel_hint}"),
            CompactionStatusReason::Threshold => format!("Auto-compacting... {cancel_hint}"),
            CompactionStatusReason::Overflow => {
                format!("Context overflow detected, Auto-compacting... {cancel_hint}")
            }
        };
        Self::new(StatusIndicatorKind::Compaction, message)
    }

    #[must_use]
    pub fn branch_summary() -> Self {
        Self::new(
            StatusIndicatorKind::BranchSummary,
            format!(
                "Summarizing branch... ({} to cancel)",
                key_text("app.interrupt")
            ),
        )
    }

    #[must_use]
    pub fn new(kind: StatusIndicatorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            started: Instant::now(),
            frame: 0,
            cached: Vec::new(),
            retry: None,
            custom_frames: None,
            last_tick: None,
        }
    }

    /// Extension spinner override (`None` restores the built-in frames).
    pub fn set_custom_frames(&mut self, frames: Option<(Vec<String>, u64)>) {
        self.custom_frames = frames.filter(|(frames, _)| !frames.is_empty());
        self.frame = 0;
        self.last_tick = None;
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.retry = None;
    }

    fn refresh_retry_message(&mut self) {
        let Some(retry) = self.retry else {
            return;
        };
        let remaining = retry.deadline.saturating_duration_since(Instant::now());
        let seconds = remaining.as_millis().div_ceil(1_000);
        self.message = format!(
            "Retrying ({}/{}) in {seconds}s... ({} to cancel)",
            retry.attempt,
            retry.max_attempts,
            key_text("app.interrupt")
        );
    }

    /// Explicit animation step for callers that drive their own clock.
    /// Custom frames honor their interval; built-in frames advance per call
    /// (the pump tick is the built-in cadence).
    pub fn tick(&mut self) {
        match &self.custom_frames {
            Some((_, interval_ms)) => {
                let now = Instant::now();
                let due = self
                    .last_tick
                    .is_none_or(|last| now.duration_since(last).as_millis() as u64 >= *interval_ms);
                if due {
                    self.frame = self.frame.wrapping_add(1);
                    self.last_tick = Some(now);
                }
            }
            None => self.frame = self.frame.wrapping_add(1),
        }
    }

    #[must_use]
    pub fn elapsed_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Component for StatusIndicator {
    fn render(&mut self, _width: u16) -> &[Line] {
        self.refresh_retry_message();
        let color = if self.kind == StatusIndicatorKind::Retry {
            ThemeColor::Warning
        } else {
            ThemeColor::Accent
        };
        let spinner: &str = match &self.custom_frames {
            Some((frames, _)) => &frames[self.frame % frames.len()],
            None => SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()],
        };
        let output = format!(
            "{} {}",
            theme().fg(color, spinner),
            theme().fg(ThemeColor::Muted, &self.message)
        );
        self.cached = vec![Line::empty(), Line::from_ansi(&output)];
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

pub struct IdleStatus {
    cached: Vec<Line>,
    width: Option<u16>,
    status: RenderStatus,
}

impl IdleStatus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cached: Vec::new(),
            width: None,
            status: RenderStatus::Changed,
        }
    }
}

impl Default for IdleStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for IdleStatus {
    fn render(&mut self, width: u16) -> &[Line] {
        if self.width == Some(width) {
            self.status = RenderStatus::Unchanged;
            return &self.cached;
        }
        self.width = Some(width);
        let empty = " ".repeat(usize::from(width));
        self.cached = vec![Line::plain(empty.clone()), Line::plain(empty)];
        self.status = RenderStatus::Changed;
        &self.cached
    }

    fn invalidate(&mut self) {
        self.width = None;
        self.status = RenderStatus::Changed;
    }

    fn last_render_status(&self) -> RenderStatus {
        self.status
    }
}
