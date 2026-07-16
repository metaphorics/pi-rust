//! Searchable interactive model selector.

use std::sync::Arc;

use parking_lot::Mutex;
use pi_ai::models::models_are_equal;
use pi_ai::types::Model;
use pi_tui::components::Input;
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::keybindings::get_keybindings;
use pi_tui::{Component, Focusable, Line, RenderStatus};
use tokio::sync::RwLock;

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_hint;
use crate::model_registry::ModelRegistry;
use crate::modes::interactive::theme::{ThemeColor, theme};
use crate::session::ScopedModel;
use crate::settings_manager::SettingsManager;

#[derive(Clone)]
struct ModelItem {
    provider: String,
    id: String,
    model: Model,
    search_text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelScope {
    All,
    Scoped,
}

fn selector_search_text(model: &Model) -> String {
    let name = if model.name.is_empty() {
        String::new()
    } else {
        format!(" {}", model.name)
    };
    format!(
        "{} {}/{} {} {}{}",
        model.provider, model.provider, model.id, model.provider, model.id, name
    )
}

fn model_item(model: Model) -> ModelItem {
    ModelItem {
        provider: model.provider.clone(),
        id: model.id.clone(),
        search_text: selector_search_text(&model),
        model,
    }
}

/// Searches configured-provider models and persists the selected default.
pub struct ModelSelectorComponent {
    all_models: Vec<ModelItem>,
    scoped_model_items: Vec<ModelItem>,
    active_models: Vec<ModelItem>,
    filtered_models: Vec<ModelItem>,
    selected_index: usize,
    current_model: Option<Model>,
    settings_manager: Arc<Mutex<SettingsManager>>,
    on_select: Box<dyn FnMut(Model)>,
    on_cancel: Box<dyn FnMut()>,
    error_message: Option<String>,
    scope: ModelScope,
    search_input: Input,
    focused: bool,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
    cached: Vec<Line>,
}

impl ModelSelectorComponent {
    /// Loads the registry before returning so the component can be attached to a TUI atomically.
    pub async fn new(
        current_model: Option<Model>,
        settings_manager: Arc<Mutex<SettingsManager>>,
        model_registry: Arc<RwLock<ModelRegistry>>,
        mut scoped_models: Vec<ScopedModel>,
        on_select: Box<dyn FnMut(Model)>,
        on_cancel: Box<dyn FnMut()>,
        initial_search_input: Option<String>,
    ) -> Self {
        {
            let mut registry = model_registry.write().await;
            registry.refresh();
        }
        let (error_message, available_models) = {
            let registry = model_registry.read().await;
            (registry.get_error(), registry.get_available().await)
        };
        {
            let registry = model_registry.read().await;
            for scoped in &mut scoped_models {
                if let Some(refreshed) = registry.find(&scoped.model.provider, &scoped.model.id) {
                    scoped.model = refreshed.clone();
                }
            }
        }

        let scope = if scoped_models.is_empty() {
            ModelScope::All
        } else {
            ModelScope::Scoped
        };
        let mut all_models = available_models
            .into_iter()
            .map(model_item)
            .collect::<Vec<_>>();
        all_models.sort_by(|a, b| {
            let a_current = models_are_equal(current_model.as_ref(), Some(&a.model));
            let b_current = models_are_equal(current_model.as_ref(), Some(&b.model));
            b_current
                .cmp(&a_current)
                .then_with(|| a.provider.cmp(&b.provider))
        });
        let scoped_model_items = scoped_models
            .into_iter()
            .map(|scoped| model_item(scoped.model))
            .collect::<Vec<_>>();
        let active_models = match scope {
            ModelScope::All => all_models.clone(),
            ModelScope::Scoped => scoped_model_items.clone(),
        };
        let current_index = active_models
            .iter()
            .position(|item| models_are_equal(current_model.as_ref(), Some(&item.model)));
        let selected_index = current_index.unwrap_or(0);
        let mut search_input = Input::new();
        if let Some(value) = initial_search_input
            && !value.is_empty()
        {
            search_input.set_value(value);
        }

        let mut component = Self {
            all_models,
            scoped_model_items,
            active_models,
            filtered_models: Vec::new(),
            selected_index,
            current_model,
            settings_manager,
            on_select,
            on_cancel,
            error_message,
            scope,
            search_input,
            focused: false,
            top_border: DynamicBorder::default(),
            bottom_border: DynamicBorder::default(),
            cached: Vec::new(),
        };
        component.filter_models();
        component
    }

    fn scope_text(&self) -> String {
        let all = if self.scope == ModelScope::All {
            theme().fg(ThemeColor::Accent, "all")
        } else {
            theme().fg(ThemeColor::Muted, "all")
        };
        let scoped = if self.scope == ModelScope::Scoped {
            theme().fg(ThemeColor::Accent, "scoped")
        } else {
            theme().fg(ThemeColor::Muted, "scoped")
        };
        format!(
            "{}{}{}{}",
            theme().fg(ThemeColor::Muted, "Scope: "),
            all,
            theme().fg(ThemeColor::Muted, " | "),
            scoped
        )
    }

    fn scope_hint_text() -> String {
        format!(
            "{}{}",
            key_hint("tui.input.tab", "scope"),
            theme().fg(ThemeColor::Muted, " (all/scoped)")
        )
    }

    fn set_scope(&mut self, scope: ModelScope) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.active_models = match scope {
            ModelScope::All => self.all_models.clone(),
            ModelScope::Scoped => self.scoped_model_items.clone(),
        };
        self.selected_index = self
            .active_models
            .iter()
            .position(|item| models_are_equal(self.current_model.as_ref(), Some(&item.model)))
            .unwrap_or(0);
        self.filter_models();
    }

    fn filter_models(&mut self) {
        self.filtered_models = if self.search_input.get_value().is_empty() {
            self.active_models.clone()
        } else {
            fuzzy_filter(&self.active_models, self.search_input.get_value(), |item| {
                item.search_text.as_str()
            })
            .into_iter()
            .cloned()
            .collect()
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_models.len().saturating_sub(1));
    }

    fn select(&mut self, model: Model) {
        {
            let mut settings = self.settings_manager.lock();
            settings.set_default_provider(model.provider.clone());
            settings.set_default_model(model.id.clone());
        }
        (self.on_select)(model);
    }

    fn render_list(&self) -> Vec<Line> {
        let max_visible = 10;
        let start = self
            .selected_index
            .saturating_sub(max_visible / 2)
            .min(self.filtered_models.len().saturating_sub(max_visible));
        let end = (start + max_visible).min(self.filtered_models.len());
        let mut lines = Vec::with_capacity(end - start + 2);
        for (offset, item) in self.filtered_models[start..end].iter().enumerate() {
            let index = start + offset;
            let selected = index == self.selected_index;
            let current = models_are_equal(self.current_model.as_ref(), Some(&item.model));
            let provider = theme().fg(ThemeColor::Muted, &format!("[{}]", item.provider));
            let checkmark = if current {
                theme().fg(ThemeColor::Success, " ✓")
            } else {
                String::new()
            };
            let text = if selected {
                format!(
                    "{}{} {provider}{checkmark}",
                    theme().fg(ThemeColor::Accent, "→ "),
                    theme().fg(ThemeColor::Accent, &item.id)
                )
            } else {
                format!("  {} {provider}{checkmark}", item.id)
            };
            lines.push(Line::from_ansi(&text));
        }
        if start > 0 || end < self.filtered_models.len() {
            lines.push(Line::from_ansi(&theme().fg(
                ThemeColor::Muted,
                &format!(
                    "  ({}/{})",
                    self.selected_index + 1,
                    self.filtered_models.len()
                ),
            )));
        }
        if let Some(error) = &self.error_message {
            lines.extend(
                error
                    .split('\n')
                    .map(|line| Line::from_ansi(&theme().fg(ThemeColor::Error, line))),
            );
        } else if self.filtered_models.is_empty() {
            lines.push(Line::from_ansi(
                &theme().fg(ThemeColor::Muted, "  No matching models"),
            ));
        } else {
            lines.push(Line::plain(""));
            lines.push(Line::from_ansi(&theme().fg(
                ThemeColor::Muted,
                &format!(
                    "  Model Name: {}",
                    self.filtered_models[self.selected_index].model.name
                ),
            )));
        }
        lines
    }

    pub fn get_search_input(&mut self) -> &mut Input {
        &mut self.search_input
    }
}

impl Component for ModelSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        let list = self.render_list();
        self.cached.clear();
        self.cached.extend_from_slice(self.top_border.render(width));
        self.cached.push(Line::plain(""));
        if self.scoped_model_items.is_empty() {
            self.cached.push(Line::from_ansi(&theme().fg(
                ThemeColor::Warning,
                "Only showing models from configured providers. Use /login to add providers.",
            )));
        } else {
            self.cached.push(Line::from_ansi(&self.scope_text()));
            self.cached.push(Line::from_ansi(&Self::scope_hint_text()));
        }
        self.cached.push(Line::plain(""));
        self.cached
            .extend_from_slice(self.search_input.render(width));
        self.cached.push(Line::plain(""));
        self.cached.extend(list);
        self.cached.push(Line::plain(""));
        self.cached
            .extend_from_slice(self.bottom_border.render(width));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.top_border.invalidate();
        self.search_input.invalidate();
        self.bottom_border.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        if kb.matches(data, "tui.input.tab") {
            if !self.scoped_model_items.is_empty() {
                self.set_scope(if self.scope == ModelScope::All {
                    ModelScope::Scoped
                } else {
                    ModelScope::All
                });
            }
            return;
        }
        if kb.matches(data, "tui.select.up") {
            if !self.filtered_models.is_empty() {
                self.selected_index = if self.selected_index == 0 {
                    self.filtered_models.len() - 1
                } else {
                    self.selected_index - 1
                };
            }
        } else if kb.matches(data, "tui.select.down") {
            if !self.filtered_models.is_empty() {
                self.selected_index = if self.selected_index + 1 == self.filtered_models.len() {
                    0
                } else {
                    self.selected_index + 1
                };
            }
        } else if kb.matches(data, "tui.select.confirm") {
            if let Some(item) = self.filtered_models.get(self.selected_index) {
                self.select(item.model.clone());
            }
        } else if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        } else {
            self.search_input.handle_input(data);
            self.filter_models();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ModelSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}
