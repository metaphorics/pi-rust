//! Session-scoped model configuration selector.

use std::collections::{HashMap, HashSet};

use pi_ai::types::Model;
use pi_tui::components::Input;
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::keybindings::get_keybindings;
use pi_tui::keys::matches_key;
use pi_tui::{Component, Focusable, Line, RenderStatus};

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::key_text;
use crate::modes::interactive::theme::{ThemeColor, theme};

/// `None` means every available model is enabled; `Some` is an explicit order.
pub type EnabledModelIds = Option<Vec<String>>;

pub struct ModelsConfig {
    pub all_models: Vec<Model>,
    pub enabled_model_ids: EnabledModelIds,
}

pub struct ModelsCallbacks {
    pub on_change: Box<dyn FnMut(EnabledModelIds)>,
    pub on_persist: Box<dyn FnMut(EnabledModelIds)>,
    pub on_cancel: Box<dyn FnMut()>,
}

#[derive(Clone)]
struct ModelItem {
    full_id: String,
    model: Model,
    enabled: bool,
    search_text: String,
}

fn is_enabled(enabled_ids: &EnabledModelIds, id: &str) -> bool {
    enabled_ids
        .as_ref()
        .is_none_or(|enabled| enabled.iter().any(|candidate| candidate == id))
}

fn toggle(enabled_ids: &EnabledModelIds, id: &str) -> EnabledModelIds {
    let Some(enabled) = enabled_ids else {
        return Some(vec![id.to_owned()]);
    };
    let mut result = enabled.clone();
    if let Some(index) = result.iter().position(|candidate| candidate == id) {
        result.remove(index);
    } else {
        result.push(id.to_owned());
    }
    Some(result)
}

fn enable_all(
    enabled_ids: &EnabledModelIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledModelIds {
    let Some(enabled) = enabled_ids else {
        return None;
    };
    let mut result = enabled.clone();
    for id in target_ids.unwrap_or(all_ids) {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    if result.len() == all_ids.len() {
        None
    } else {
        Some(result)
    }
}

fn clear_all(
    enabled_ids: &EnabledModelIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledModelIds {
    if enabled_ids.is_none() {
        return Some(match target_ids {
            Some(targets) => all_ids
                .iter()
                .filter(|id| !targets.contains(id))
                .cloned()
                .collect(),
            None => Vec::new(),
        });
    }
    let enabled = enabled_ids.as_ref().expect("checked above");
    let targets: HashSet<&str> = target_ids
        .unwrap_or(enabled)
        .iter()
        .map(String::as_str)
        .collect();
    Some(
        enabled
            .iter()
            .filter(|id| !targets.contains(id.as_str()))
            .cloned()
            .collect(),
    )
}

fn move_enabled(enabled_ids: &EnabledModelIds, id: &str, delta: isize) -> EnabledModelIds {
    let Some(enabled) = enabled_ids else {
        return None;
    };
    let mut result = enabled.clone();
    let Some(index) = result.iter().position(|candidate| candidate == id) else {
        return Some(result);
    };
    let new_index = index as isize + delta;
    if !(0..result.len() as isize).contains(&new_index) {
        return Some(result);
    }
    result.swap(index, new_index as usize);
    Some(result)
}

fn sorted_ids(enabled_ids: &EnabledModelIds, all_ids: &[String]) -> Vec<String> {
    let Some(enabled) = enabled_ids else {
        return all_ids.to_vec();
    };
    let enabled_set: HashSet<&str> = enabled.iter().map(String::as_str).collect();
    enabled
        .iter()
        .cloned()
        .chain(
            all_ids
                .iter()
                .filter(|id| !enabled_set.contains(id.as_str()))
                .cloned(),
        )
        .collect()
}

fn model_search_text(model: &Model) -> String {
    let name = if model.name.is_empty() {
        String::new()
    } else {
        format!(" {}", model.name)
    };
    format!(
        "{} {} {}/{} {} {}{}",
        model.id, model.provider, model.provider, model.id, model.provider, model.id, name
    )
}

/// Enables, disables, reorders, and optionally persists the models used by cycling.
pub struct ScopedModelsSelectorComponent {
    models_by_id: HashMap<String, Model>,
    all_ids: Vec<String>,
    enabled_ids: EnabledModelIds,
    filtered_items: Vec<ModelItem>,
    selected_index: usize,
    search_input: Input,
    callbacks: ModelsCallbacks,
    max_visible: usize,
    is_dirty: bool,
    focused: bool,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
    cached: Vec<Line>,
}

impl ScopedModelsSelectorComponent {
    #[must_use]
    pub fn new(config: ModelsConfig, callbacks: ModelsCallbacks) -> Self {
        let mut models_by_id = HashMap::new();
        let mut all_ids = Vec::with_capacity(config.all_models.len());
        for model in config.all_models {
            let full_id = format!("{}/{}", model.provider, model.id);
            models_by_id.insert(full_id.clone(), model);
            all_ids.push(full_id);
        }
        let mut component = Self {
            models_by_id,
            all_ids,
            enabled_ids: config.enabled_model_ids,
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            callbacks,
            max_visible: 8,
            is_dirty: false,
            focused: false,
            top_border: DynamicBorder::default(),
            bottom_border: DynamicBorder::default(),
            cached: Vec::new(),
        };
        component.refresh();
        component
    }

    fn build_items(&self) -> Vec<ModelItem> {
        sorted_ids(&self.enabled_ids, &self.all_ids)
            .into_iter()
            .filter_map(|full_id| {
                let model = self.models_by_id.get(&full_id)?.clone();
                Some(ModelItem {
                    enabled: is_enabled(&self.enabled_ids, &full_id),
                    search_text: model_search_text(&model),
                    full_id,
                    model,
                })
            })
            .collect()
    }

    fn refresh(&mut self) {
        let items = self.build_items();
        self.filtered_items = if self.search_input.get_value().is_empty() {
            items
        } else {
            fuzzy_filter(&items, self.search_input.get_value(), |item| {
                item.search_text.as_str()
            })
            .into_iter()
            .cloned()
            .collect()
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_items.len().saturating_sub(1));
    }

    fn enabled_ids_copy(&self) -> EnabledModelIds {
        self.enabled_ids.clone()
    }

    fn notify_change(&mut self) {
        let enabled = self.enabled_ids_copy();
        (self.callbacks.on_change)(enabled);
    }

    fn footer_text(&self) -> String {
        let enabled_count = self
            .enabled_ids
            .as_ref()
            .map_or(self.all_ids.len(), Vec::len);
        let count_text = if self.enabled_ids.is_none() {
            "all enabled".to_owned()
        } else {
            format!("{enabled_count}/{} enabled", self.all_ids.len())
        };
        let parts = [
            format!("{} toggle", key_text("tui.select.confirm")),
            format!("{} all", key_text("app.models.enableAll")),
            format!("{} clear", key_text("app.models.clearAll")),
            format!("{} provider", key_text("app.models.toggleProvider")),
            format!(
                "{}/{} reorder",
                key_text("app.models.reorderUp"),
                key_text("app.models.reorderDown")
            ),
            format!("{} save", key_text("app.models.save")),
            count_text,
        ];
        let body = format!("  {}", parts.join(" · "));
        if self.is_dirty {
            format!(
                "{}{}",
                theme().fg(ThemeColor::Dim, &format!("{body} ")),
                theme().fg(ThemeColor::Warning, "(unsaved)")
            )
        } else {
            theme().fg(ThemeColor::Dim, &body)
        }
    }

    fn render_list(&self) -> Vec<Line> {
        if self.filtered_items.is_empty() {
            return vec![Line::from_ansi(
                &theme().fg(ThemeColor::Muted, "  No matching models"),
            )];
        }
        let start = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.filtered_items.len().saturating_sub(self.max_visible));
        let end = (start + self.max_visible).min(self.filtered_items.len());
        let all_enabled = self.enabled_ids.is_none();
        let mut lines = Vec::with_capacity(end - start + 2);
        for (index, item) in self.filtered_items[start..end].iter().enumerate() {
            let selected = start + index == self.selected_index;
            let prefix = if selected {
                theme().fg(ThemeColor::Accent, "→ ")
            } else {
                "  ".to_owned()
            };
            let model = if selected {
                theme().fg(ThemeColor::Accent, &item.model.id)
            } else {
                item.model.id.clone()
            };
            let provider = theme().fg(ThemeColor::Muted, &format!(" [{}]", item.model.provider));
            let status = if all_enabled {
                String::new()
            } else if item.enabled {
                theme().fg(ThemeColor::Success, " ✓")
            } else {
                theme().fg(ThemeColor::Dim, " ✗")
            };
            lines.push(Line::from_ansi(&format!(
                "{prefix}{model}{provider}{status}"
            )));
        }
        if start > 0 || end < self.filtered_items.len() {
            lines.push(Line::from_ansi(&theme().fg(
                ThemeColor::Muted,
                &format!(
                    "  ({}/{})",
                    self.selected_index + 1,
                    self.filtered_items.len()
                ),
            )));
        }
        let selected = &self.filtered_items[self.selected_index];
        lines.push(Line::plain(""));
        lines.push(Line::from_ansi(&theme().fg(
            ThemeColor::Muted,
            &format!("  Model Name: {}", selected.model.name),
        )));
        lines
    }

    pub fn get_search_input(&mut self) -> &mut Input {
        &mut self.search_input
    }

    #[must_use]
    pub fn enabled_model_ids(&self) -> &EnabledModelIds {
        &self.enabled_ids
    }
}

impl Component for ScopedModelsSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.extend_from_slice(self.top_border.render(width));
        self.cached.push(Line::plain(""));
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Accent, &theme().bold("Model Configuration")),
        ));
        self.cached.push(Line::from_ansi(&theme().fg(
            ThemeColor::Muted,
            &format!(
                "Session-only. {} to save to settings.",
                key_text("app.models.save")
            ),
        )));
        self.cached.push(Line::plain(""));
        self.cached
            .extend_from_slice(self.search_input.render(width));
        self.cached.push(Line::plain(""));
        self.cached.extend(self.render_list());
        self.cached.push(Line::plain(""));
        self.cached.push(Line::from_ansi(&self.footer_text()));
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
        if kb.matches(data, "tui.select.up") {
            if !self.filtered_items.is_empty() {
                self.selected_index = if self.selected_index == 0 {
                    self.filtered_items.len() - 1
                } else {
                    self.selected_index - 1
                };
            }
            return;
        }
        if kb.matches(data, "tui.select.down") {
            if !self.filtered_items.is_empty() {
                self.selected_index = if self.selected_index + 1 == self.filtered_items.len() {
                    0
                } else {
                    self.selected_index + 1
                };
            }
            return;
        }
        let reorder_up = kb.matches(data, "app.models.reorderUp");
        let reorder_down = kb.matches(data, "app.models.reorderDown");
        if reorder_up || reorder_down {
            if self.enabled_ids.is_some()
                && let Some(item) = self.filtered_items.get(self.selected_index).cloned()
                && is_enabled(&self.enabled_ids, &item.full_id)
            {
                let delta = if reorder_up { -1 } else { 1 };
                let current_index = self
                    .enabled_ids
                    .as_ref()
                    .and_then(|ids| ids.iter().position(|id| id == &item.full_id));
                if let Some(current_index) = current_index {
                    let new_index = current_index as isize + delta;
                    let enabled_len = self.enabled_ids.as_ref().map_or(0, Vec::len);
                    if (0..enabled_len as isize).contains(&new_index) {
                        self.enabled_ids = move_enabled(&self.enabled_ids, &item.full_id, delta);
                        self.is_dirty = true;
                        self.selected_index = (self.selected_index as isize + delta) as usize;
                        self.refresh();
                        self.notify_change();
                    }
                }
            }
            return;
        }
        if kb.matches(data, "tui.select.confirm") {
            if let Some(item) = self.filtered_items.get(self.selected_index) {
                self.enabled_ids = toggle(&self.enabled_ids, &item.full_id);
                self.is_dirty = true;
                self.refresh();
                self.notify_change();
            }
            return;
        }
        if kb.matches(data, "app.models.enableAll") {
            let targets = (!self.search_input.get_value().is_empty()).then(|| {
                self.filtered_items
                    .iter()
                    .map(|item| item.full_id.clone())
                    .collect::<Vec<_>>()
            });
            self.enabled_ids = enable_all(&self.enabled_ids, &self.all_ids, targets.as_deref());
            self.is_dirty = true;
            self.refresh();
            self.notify_change();
            return;
        }
        if kb.matches(data, "app.models.clearAll") {
            let targets = (!self.search_input.get_value().is_empty()).then(|| {
                self.filtered_items
                    .iter()
                    .map(|item| item.full_id.clone())
                    .collect::<Vec<_>>()
            });
            self.enabled_ids = clear_all(&self.enabled_ids, &self.all_ids, targets.as_deref());
            self.is_dirty = true;
            self.refresh();
            self.notify_change();
            return;
        }
        if kb.matches(data, "app.models.toggleProvider") {
            if let Some(item) = self.filtered_items.get(self.selected_index) {
                let provider = item.model.provider.clone();
                let provider_ids = self
                    .all_ids
                    .iter()
                    .filter(|id| {
                        self.models_by_id
                            .get(*id)
                            .is_some_and(|model| model.provider == provider)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let all_enabled = provider_ids
                    .iter()
                    .all(|id| is_enabled(&self.enabled_ids, id));
                self.enabled_ids = if all_enabled {
                    clear_all(&self.enabled_ids, &self.all_ids, Some(&provider_ids))
                } else {
                    enable_all(&self.enabled_ids, &self.all_ids, Some(&provider_ids))
                };
                self.is_dirty = true;
                self.refresh();
                self.notify_change();
            }
            return;
        }
        if kb.matches(data, "app.models.save") {
            let enabled = self.enabled_ids_copy();
            (self.callbacks.on_persist)(enabled);
            self.is_dirty = false;
            return;
        }
        if matches_key(data, "ctrl+c") {
            if self.search_input.get_value().is_empty() {
                (self.callbacks.on_cancel)();
            } else {
                self.search_input.set_value("");
                self.refresh();
            }
            return;
        }
        if matches_key(data, "escape") {
            (self.callbacks.on_cancel)();
            return;
        }
        self.search_input.handle_input(data);
        self.refresh();
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ScopedModelsSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}
