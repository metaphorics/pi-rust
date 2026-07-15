//! Loader — spinning indicator over [`Text`].
//!
//! Port of `packages/tui/src/components/loader.ts`.

use std::time::{Duration, Instant};

use crate::component::{Component, RenderStatus};
use crate::line::Line;

use super::text::Text;

/// Animation frames / interval for the spinner indicator.
#[derive(Debug, Clone)]
pub struct LoaderIndicatorOptions {
    /// Animation frames. Empty array hides the indicator.
    pub frames: Option<Vec<String>>,
    /// Frame interval in milliseconds.
    pub interval_ms: Option<u64>,
}

const DEFAULT_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DEFAULT_INTERVAL_MS: u64 = 80;

type ColorFn = Box<dyn Fn(&str) -> String>;
type RequestRender = Box<dyn FnMut()>;

/// Loader component that updates with an optional spinning animation.
///
/// Extends the Text pattern (padding_x=1, padding_y=0). Animation advances on
/// `render` while running (no OS timer); optional `request_render` callback
/// mirrors TS `ui.requestRender()`.
pub struct Loader {
    inner: Text,
    frames: Vec<String>,
    interval_ms: u64,
    current_frame: usize,
    running: bool,
    last_tick: Option<Instant>,
    request_render: Option<RequestRender>,
    render_indicator_verbatim: bool,
    spinner_color_fn: ColorFn,
    message_color_fn: ColorFn,
    message: String,
    /// Leading blank line prepended in `render` (TS: `["", ...super.render]`).
    outer_cache: Vec<Line>,
    last_status: RenderStatus,
}

impl Loader {
    #[must_use]
    pub fn new(
        spinner_color_fn: ColorFn,
        message_color_fn: ColorFn,
        message: impl Into<String>,
        indicator: Option<LoaderIndicatorOptions>,
        request_render: Option<RequestRender>,
    ) -> Self {
        let mut loader = Self {
            inner: Text::new(String::new(), 1, 0, None),
            frames: DEFAULT_FRAMES.iter().map(|s| (*s).to_owned()).collect(),
            interval_ms: DEFAULT_INTERVAL_MS,
            current_frame: 0,
            running: false,
            last_tick: None,
            request_render,
            render_indicator_verbatim: false,
            spinner_color_fn,
            message_color_fn,
            message: message.into(),
            outer_cache: Vec::new(),
            last_status: RenderStatus::Changed,
        };
        loader.set_indicator(indicator);
        loader
    }

    /// Default message `"Loading..."`.
    #[must_use]
    pub fn with_colors(spinner_color_fn: ColorFn, message_color_fn: ColorFn) -> Self {
        Self::new(spinner_color_fn, message_color_fn, "Loading...", None, None)
    }

    pub fn start(&mut self) {
        self.update_display();
        self.restart_animation();
    }

    pub fn stop(&mut self) {
        self.running = false;
        self.last_tick = None;
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.update_display();
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicatorOptions>) {
        self.render_indicator_verbatim = indicator.is_some();
        if let Some(ind) = indicator {
            self.frames = ind
                .frames
                .unwrap_or_else(|| DEFAULT_FRAMES.iter().map(|s| (*s).to_owned()).collect());
            self.interval_ms = ind
                .interval_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_INTERVAL_MS);
        } else {
            self.frames = DEFAULT_FRAMES.iter().map(|s| (*s).to_owned()).collect();
            self.interval_ms = DEFAULT_INTERVAL_MS;
        }
        self.current_frame = 0;
        self.start();
    }

    pub fn set_request_render(&mut self, request_render: Option<RequestRender>) {
        self.request_render = request_render;
    }

    fn restart_animation(&mut self) {
        self.stop();
        if self.frames.len() <= 1 {
            return;
        }
        self.running = true;
        self.last_tick = Some(Instant::now());
    }

    fn maybe_advance_frame(&mut self) {
        if !self.running || self.frames.len() <= 1 {
            return;
        }
        let Some(last) = self.last_tick else {
            self.last_tick = Some(Instant::now());
            return;
        };
        let interval = Duration::from_millis(self.interval_ms);
        if last.elapsed() >= interval {
            self.current_frame = (self.current_frame + 1) % self.frames.len();
            self.last_tick = Some(Instant::now());
            self.update_display();
        }
    }

    fn update_display(&mut self) {
        let frame = self
            .frames
            .get(self.current_frame)
            .map(String::as_str)
            .unwrap_or("");
        let rendered_frame = if self.render_indicator_verbatim {
            frame.to_owned()
        } else {
            (self.spinner_color_fn)(frame)
        };
        let indicator = if frame.is_empty() {
            String::new()
        } else {
            format!("{rendered_frame} ")
        };
        let colored_msg = (self.message_color_fn)(&self.message);
        self.inner.set_text(format!("{indicator}{colored_msg}"));
        if let Some(cb) = &mut self.request_render {
            cb();
        }
    }
}

impl Component for Loader {
    fn render(&mut self, width: u16) -> &[Line] {
        self.maybe_advance_frame();
        let inner = self.inner.render(width);
        self.outer_cache.clear();
        self.outer_cache.push(Line::empty());
        self.outer_cache.extend_from_slice(inner);
        // Loader always changes when animating; Text cache may still report Unchanged.
        self.last_status = if self.running && self.frames.len() > 1 {
            RenderStatus::Changed
        } else {
            self.inner.last_render_status()
        };
        &self.outer_cache
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }

    fn last_render_status(&self) -> RenderStatus {
        self.last_status
    }
}
