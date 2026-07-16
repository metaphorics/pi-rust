//! Coding-agent editor wrapper with an app-level pre-input interceptor.

use pi_tui::component::{Component, Focusable, RenderStatus};
use pi_tui::line::Line;

use crate::modes::interactive::theme::{ThemeColor, theme};

/// Wraps the pi-tui editor (or another editor-compatible component). The
/// interceptor runs before the wrapped editor and returns `true` when a key was
/// consumed by an app or extension binding.
pub struct CustomEditor<C: Component> {
    pub inner: C,
    pub bash_mode: bool,
    interceptor: Box<dyn FnMut(&str) -> bool>,
    cached: Vec<Line>,
    status: RenderStatus,
}

impl<C: Component> CustomEditor<C> {
    #[must_use]
    pub fn new(inner: C, interceptor: impl FnMut(&str) -> bool + 'static) -> Self {
        Self {
            inner,
            bash_mode: false,
            interceptor: Box::new(interceptor),
            cached: Vec::new(),
            status: RenderStatus::Changed,
        }
    }

    pub fn set_interceptor(&mut self, interceptor: impl FnMut(&str) -> bool + 'static) {
        self.interceptor = Box::new(interceptor);
    }

    pub fn set_bash_mode(&mut self, enabled: bool) {
        if self.bash_mode != enabled {
            self.bash_mode = enabled;
            self.invalidate();
        }
    }
}

impl<C: Component> Component for CustomEditor<C> {
    fn render(&mut self, width: u16) -> &[Line] {
        let inner_status;
        {
            let rendered = self.inner.render(width);
            self.cached.clear();
            self.cached.extend_from_slice(rendered);
            inner_status = self.inner.last_render_status();
        }
        if self.bash_mode && !self.cached.is_empty() {
            let last = self.cached.len() - 1;
            for index in [0, last] {
                let ansi = self.cached[index].to_ansi();
                self.cached[index] = Line::from_ansi(&theme().fg(ThemeColor::BashMode, &ansi));
            }
        }
        self.status = if self.bash_mode {
            RenderStatus::Changed
        } else {
            inner_status
        };
        &self.cached
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
        self.cached.clear();
        self.status = RenderStatus::Changed;
    }

    fn handle_input(&mut self, data: &str) {
        if !(self.interceptor)(data) {
            self.inner.handle_input(data);
        }
    }

    fn wants_key_release(&self) -> bool {
        self.inner.wants_key_release()
    }

    fn last_render_status(&self) -> RenderStatus {
        self.status
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        self.inner.as_focusable()
    }
}
