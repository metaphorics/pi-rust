//! Earendil announcement component.

use pi_tui::component::{Component, RenderStatus};
use pi_tui::components::Text;
use pi_tui::line::Line;

use crate::modes::interactive::theme::{ThemeColor, theme};

pub const BLOG_URL: &str = "https://mariozechner.at/posts/2026-04-08-ive-sold-out/";

pub struct EarendilAnnouncement {
    cached: Vec<Line>,
    width: Option<u16>,
    status: RenderStatus,
}

impl EarendilAnnouncement {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cached: Vec::new(),
            width: None,
            status: RenderStatus::Changed,
        }
    }
}

impl Default for EarendilAnnouncement {
    fn default() -> Self {
        Self::new()
    }
}

fn append_text(lines: &mut Vec<Line>, text: &str, width: u16) {
    let mut component = Text::new(text, 1, 0, None);
    lines.extend_from_slice(component.render(width));
}

impl Component for EarendilAnnouncement {
    fn render(&mut self, width: u16) -> &[Line] {
        if self.width == Some(width) {
            self.status = RenderStatus::Unchanged;
            return &self.cached;
        }
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Accent, &"─".repeat(usize::from(width))),
        ));
        append_text(
            &mut self.cached,
            &theme().bold(&theme().fg(ThemeColor::Accent, "pi has joined Earendil")),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Muted, "Read the blog post:"),
            width,
        );
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::MdLink, BLOG_URL),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Accent, &"─".repeat(usize::from(width))),
        ));
        self.width = Some(width);
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
