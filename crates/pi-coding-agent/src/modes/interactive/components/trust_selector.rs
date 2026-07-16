//! Project trust chooser ported from `trust-selector.ts`.

use std::path::{Path, PathBuf};

use pi_tui::component::{Component, RenderStatus};
use pi_tui::components::Text;
use pi_tui::line::Line;

use super::keybinding_hints::{key_hint, raw_key_hint};
use crate::modes::interactive::theme::{ThemeColor, theme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustUpdate {
    pub path: String,
    pub decision: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectTrustStoreEntry {
    pub path: String,
    pub decision: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustSelection {
    pub trusted: bool,
    pub updates: Vec<ProjectTrustUpdate>,
}

#[derive(Clone, Debug)]
struct ProjectTrustOption {
    label: String,
    trusted: bool,
    updates: Vec<ProjectTrustUpdate>,
    saved_path: Option<String>,
}

pub struct TrustSelectorOptions {
    pub cwd: String,
    pub saved_decision: Option<ProjectTrustStoreEntry>,
    pub project_trusted: bool,
    pub on_select: Box<dyn FnMut(TrustSelection)>,
    pub on_cancel: Box<dyn FnMut()>,
}

pub struct TrustSelectorComponent {
    cwd: String,
    saved_decision: Option<ProjectTrustStoreEntry>,
    project_trusted: bool,
    trust_options: Vec<ProjectTrustOption>,
    selected: usize,
    on_select: Box<dyn FnMut(TrustSelection)>,
    on_cancel: Box<dyn FnMut()>,
    cached: Vec<Line>,
}

fn normalize_cwd(cwd: &str) -> String {
    let path = Path::new(cwd);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    std::fs::canonicalize(&absolute)
        .unwrap_or(absolute)
        .display()
        .to_string()
}

fn project_trust_options(cwd: &str) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd);
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_owned(),
        trusted: true,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(true),
        }],
        saved_path: Some(trust_path.clone()),
    }];
    if let Some(parent) = Path::new(&trust_path).parent()
        && parent != Path::new(&trust_path)
    {
        let parent = parent.display().to_string();
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({parent})"),
            trusted: true,
            updates: vec![
                ProjectTrustUpdate {
                    path: parent.clone(),
                    decision: Some(true),
                },
                ProjectTrustUpdate {
                    path: trust_path.clone(),
                    decision: None,
                },
            ],
            saved_path: Some(parent),
        });
    }
    options.push(ProjectTrustOption {
        label: "Do not trust".to_owned(),
        trusted: false,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(false),
        }],
        saved_path: Some(trust_path),
    });
    options
}

fn format_decision(trust_path: Option<&str>, decision: Option<&ProjectTrustStoreEntry>) -> String {
    let Some(decision) = decision else {
        return "none".to_owned();
    };
    let label = if decision.decision {
        "trusted"
    } else {
        "untrusted"
    };
    if trust_path.is_some_and(|path| path != decision.path) {
        format!("{label} (inherited from {})", decision.path)
    } else {
        format!("{label} ({})", decision.path)
    }
}

impl TrustSelectorComponent {
    #[must_use]
    pub fn new(options: TrustSelectorOptions) -> Self {
        let trust_options = project_trust_options(&options.cwd);
        let selected = trust_options
            .iter()
            .position(|option| {
                option.saved_path.as_deref().is_some_and(|path| {
                    options.saved_decision.as_ref().is_some_and(|decision| {
                        decision.decision == option.trusted && decision.path == path
                    })
                })
            })
            .unwrap_or(0);
        Self {
            cwd: options.cwd,
            saved_decision: options.saved_decision,
            project_trusted: options.project_trusted,
            trust_options,
            selected,
            on_select: options.on_select,
            on_cancel: options.on_cancel,
            cached: Vec::new(),
        }
    }

    fn is_saved_option(&self, option: &ProjectTrustOption) -> bool {
        option.saved_path.as_deref().is_some_and(|path| {
            self.saved_decision.as_ref().is_some_and(|decision| {
                decision.decision == option.trusted && decision.path == path
            })
        })
    }
}

fn append_text(lines: &mut Vec<Line>, text: &str, width: u16) {
    if text.is_empty() {
        lines.push(Line::plain(" ".repeat(usize::from(width))));
    } else {
        let mut text = Text::new(text, 1, 0, None);
        lines.extend_from_slice(text.render(width));
    }
}

impl Component for TrustSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Accent, &theme().bold("Project trust")),
            width,
        );
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Muted, &self.cwd),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        let saved = format_decision(
            self.trust_options
                .first()
                .and_then(|option| option.saved_path.as_deref()),
            self.saved_decision.as_ref(),
        );
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Muted, &format!("Saved decision: {saved}")),
            width,
        );
        append_text(
            &mut self.cached,
            &theme().fg(
                ThemeColor::Muted,
                &format!(
                    "Current session: {}",
                    if self.project_trusted {
                        "trusted"
                    } else {
                        "untrusted"
                    }
                ),
            ),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        for (index, option) in self.trust_options.iter().enumerate() {
            let selected = index == self.selected;
            let prefix = if selected {
                theme().fg(ThemeColor::Accent, "→ ")
            } else {
                "  ".to_owned()
            };
            let label = if selected {
                theme().fg(ThemeColor::Accent, &option.label)
            } else {
                theme().fg(ThemeColor::Text, &option.label)
            };
            let checkmark = if self.is_saved_option(option) {
                theme().fg(ThemeColor::Success, " ✓")
            } else {
                String::new()
            };
            append_text(
                &mut self.cached,
                &format!("{prefix}{label}{checkmark}"),
                width,
            );
        }
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &format!(
                "{}  {}  {}",
                raw_key_hint("↑↓", "navigate"),
                key_hint("tui.select.confirm", "save"),
                key_hint("tui.select.cancel", "cancel")
            ),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn handle_input(&mut self, data: &str) {
        let keybindings = pi_tui::keybindings::get_keybindings();
        if keybindings.matches(data, "tui.select.up") || data == "k" {
            self.selected = self.selected.saturating_sub(1);
        } else if keybindings.matches(data, "tui.select.down") || data == "j" {
            self.selected = (self.selected + 1).min(self.trust_options.len().saturating_sub(1));
        } else if keybindings.matches(data, "tui.select.confirm") || data == "\n" {
            if let Some(selected) = self.trust_options.get(self.selected) {
                (self.on_select)(TrustSelection {
                    trusted: selected.trusted,
                    updates: selected.updates.clone(),
                });
            }
        } else if keybindings.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}
