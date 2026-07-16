//! Searchable authentication-provider selector ported from `oauth-selector.ts`.

use pi_tui::component::{Component, Focusable, RenderStatus};
use pi_tui::components::{Input, Text};
use pi_tui::fuzzy::fuzzy_match;
use pi_tui::line::Line;

use crate::modes::interactive::theme::{ThemeColor, theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthType {
    OAuth,
    ApiKey,
}

impl AuthType {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OAuth => "subscription",
            Self::ApiKey => "API key",
        }
    }

    const fn search_text(self) -> &'static str {
        match self {
            Self::OAuth => "oauth subscription",
            Self::ApiKey => "api_key API key",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthStatusSource {
    Environment(Option<String>),
    Runtime,
    Fallback,
    ModelsJsonKey,
    ModelsJsonCommand,
}

#[derive(Clone, Debug)]
pub struct OAuthProvider {
    pub id: String,
    pub name: String,
    pub auth_type: AuthType,
    pub configured_credential: Option<AuthType>,
    pub auth_status: Option<AuthStatusSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OAuthSelectorMode {
    Login,
    Logout,
}

pub struct OAuthSelector {
    mode: OAuthSelectorMode,
    pub providers: Vec<OAuthProvider>,
    filtered: Vec<usize>,
    pub selected: usize,
    search_input: Input,
    focused: bool,
    show_auth_type_labels: bool,
    on_select: Box<dyn FnMut(String, AuthType)>,
    on_cancel: Box<dyn FnMut()>,
    cached: Vec<Line>,
}

impl OAuthSelector {
    #[must_use]
    pub fn new(
        mode: OAuthSelectorMode,
        providers: Vec<OAuthProvider>,
        on_select: impl FnMut(String, AuthType) + 'static,
        on_cancel: impl FnMut() + 'static,
    ) -> Self {
        let show_auth_type_labels = providers
            .first()
            .is_some_and(|first| providers.iter().any(|p| p.auth_type != first.auth_type));
        let filtered = (0..providers.len()).collect();
        Self {
            mode,
            providers,
            filtered,
            selected: 0,
            search_input: Input::default(),
            focused: false,
            show_auth_type_labels,
            on_select: Box::new(on_select),
            on_cancel: Box::new(on_cancel),
            cached: Vec::new(),
        }
    }

    pub fn set_initial_search(&mut self, query: &str) {
        self.search_input.set_value(query);
        self.filter_providers();
    }

    fn filter_providers(&mut self) {
        let query = self.search_input.get_value();
        if query.is_empty() {
            self.filtered = (0..self.providers.len()).collect();
        } else {
            let mut matches = self
                .providers
                .iter()
                .enumerate()
                .filter_map(|(index, provider)| {
                    let searchable = format!(
                        "{} {} {}",
                        provider.name,
                        provider.id,
                        provider.auth_type.search_text()
                    );
                    let matched = fuzzy_match(query, &searchable);
                    matched.matches.then_some((index, matched.score))
                })
                .collect::<Vec<_>>();
            matches.sort_by(|a, b| a.1.total_cmp(&b.1));
            self.filtered = matches.into_iter().map(|(index, _)| index).collect();
        }
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn selected_provider(&self) -> Option<&OAuthProvider> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.providers.get(*index))
    }

    fn status_indicator(&self, provider: &OAuthProvider) -> String {
        if provider.configured_credential == Some(provider.auth_type) {
            return theme().fg(ThemeColor::Success, " ✓ configured");
        }
        if let Some(credential) = provider.configured_credential {
            return format!(
                "{}{}",
                theme().fg(ThemeColor::Muted, " • "),
                theme().fg(
                    ThemeColor::Warning,
                    if credential == AuthType::OAuth {
                        "subscription configured"
                    } else {
                        "API key configured"
                    }
                )
            );
        }
        if provider.auth_type != AuthType::ApiKey {
            return theme().fg(ThemeColor::Muted, " • unconfigured");
        }
        match provider.auth_status.as_ref() {
            Some(AuthStatusSource::Environment(label)) => theme().fg(
                ThemeColor::Success,
                &format!(" ✓ env: {}", label.as_deref().unwrap_or("API key")),
            ),
            Some(AuthStatusSource::Runtime) => {
                theme().fg(ThemeColor::Success, " ✓ runtime API key")
            }
            Some(AuthStatusSource::Fallback) => {
                theme().fg(ThemeColor::Success, " ✓ custom API key")
            }
            Some(AuthStatusSource::ModelsJsonKey) => {
                theme().fg(ThemeColor::Success, " ✓ key in models.json")
            }
            Some(AuthStatusSource::ModelsJsonCommand) => {
                theme().fg(ThemeColor::Success, " ✓ command in models.json")
            }
            None => theme().fg(ThemeColor::Muted, " • unconfigured"),
        }
    }
}

fn append_text(lines: &mut Vec<Line>, text: &str, width: u16) {
    if text.is_empty() {
        lines.push(Line::plain(" ".repeat(usize::from(width))));
    } else {
        let mut component = Text::new(text, 1, 0, None);
        lines.extend_from_slice(component.render(width));
    }
}

impl Component for OAuthSelector {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        let title = if self.mode == OAuthSelectorMode::Login {
            "Select provider to configure:"
        } else {
            "Select provider to logout:"
        };
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Accent, &theme().bold(title)),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached
            .extend_from_slice(self.search_input.render(width));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));

        let max_visible = 8;
        let start = self
            .selected
            .saturating_sub(max_visible / 2)
            .min(self.filtered.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(self.filtered.len());
        for visible_index in start..end {
            let provider = &self.providers[self.filtered[visible_index]];
            let selected = visible_index == self.selected;
            let prefix = if selected {
                theme().fg(ThemeColor::Accent, "→ ")
            } else {
                "  ".to_owned()
            };
            let name = if selected {
                theme().fg(ThemeColor::Accent, &provider.name)
            } else {
                theme().fg(ThemeColor::Text, &provider.name)
            };
            let type_label = if self.show_auth_type_labels {
                theme().fg(
                    ThemeColor::Muted,
                    &format!(" [{}]", provider.auth_type.label()),
                )
            } else {
                String::new()
            };
            let status = self.status_indicator(provider);
            append_text(
                &mut self.cached,
                &format!("{prefix}{name}{type_label}{status}"),
                width,
            );
        }
        if start > 0 || end < self.filtered.len() {
            append_text(
                &mut self.cached,
                &theme().fg(
                    ThemeColor::Muted,
                    &format!("  ({}/{})", self.selected + 1, self.filtered.len()),
                ),
                width,
            );
        }
        if self.filtered.is_empty() {
            let message = if self.providers.is_empty() {
                if self.mode == OAuthSelectorMode::Login {
                    "No providers available"
                } else {
                    "No providers logged in. Use /login first."
                }
            } else {
                "No matching providers"
            };
            append_text(
                &mut self.cached,
                &theme().fg(ThemeColor::Muted, &format!("  {message}")),
                width,
            );
        }
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.search_input.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        let (up, down, confirm, cancel) = {
            let keybindings = pi_tui::keybindings::get_keybindings();
            (
                keybindings.matches(data, "tui.select.up"),
                keybindings.matches(data, "tui.select.down"),
                keybindings.matches(data, "tui.select.confirm"),
                keybindings.matches(data, "tui.select.cancel"),
            )
        };
        if up {
            self.selected = self.selected.saturating_sub(1);
        } else if down {
            self.selected = (self.selected + 1).min(self.filtered.len().saturating_sub(1));
        } else if confirm {
            if let Some(provider) = self.selected_provider() {
                let id = provider.id.clone();
                let auth_type = provider.auth_type;
                (self.on_select)(id, auth_type);
            }
        } else if cancel {
            (self.on_cancel)();
        } else {
            self.search_input.handle_input(data);
            self.filter_providers();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for OAuthSelector {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}
