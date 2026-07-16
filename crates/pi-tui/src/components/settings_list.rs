//! SettingsList — searchable settings with value cycle / submenu slot.
//!
//! Port of `packages/tui/src/components/settings-list.ts`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::component::{Component, ComponentBox, RenderStatus};
use crate::fuzzy::fuzzy_match;
use crate::keybindings::get_keybindings;
use crate::line::Line;
use crate::util::{truncate_to_width, visible_width, wrap_text_with_ansi};

use super::input::Input;

/// One settings row.
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    pub values: Option<Vec<String>>,
    /// Opens a submenu: `(current_value, done_callback) -> Component`.
    /// `done` receives `Some(selected)` on confirm or `None` on cancel.
    pub submenu: Option<Box<dyn FnMut(&str, Box<dyn FnMut(Option<String>)>) -> ComponentBox>>,
}

impl SettingItem {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        current_value: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            current_value: current_value.into(),
            values: None,
            submenu: None,
        }
    }
}

/// Theme for SettingsList.
pub struct SettingsListTheme {
    pub label: Box<dyn Fn(&str, bool) -> String>,
    pub value: Box<dyn Fn(&str, bool) -> String>,
    pub description: Box<dyn Fn(&str) -> String>,
    pub cursor: String,
    pub hint: Box<dyn Fn(&str) -> String>,
}

impl SettingsListTheme {
    #[must_use]
    pub fn identity() -> Self {
        Self {
            label: Box::new(|s, _| s.to_owned()),
            value: Box::new(|s, _| s.to_owned()),
            description: Box::new(|s| s.to_owned()),
            cursor: "→ ".to_owned(),
            hint: Box::new(|s| s.to_owned()),
        }
    }
}

/// Construction options.
#[derive(Debug, Clone, Default)]
pub struct SettingsListOptions {
    pub enable_search: bool,
}

/// Shared slot written by submenu `done` callback.
/// Outer `Option` = callback was invoked; inner = selected value or cancel.
type SubmenuPending = Rc<RefCell<Option<Option<String>>>>;

/// Settings list with optional fuzzy search and submenu.
pub struct SettingsList {
    items: Vec<SettingItem>,
    /// Indices into `items` for the current filter (when search enabled).
    filtered_indices: Vec<usize>,
    theme: SettingsListTheme,
    selected_index: usize,
    max_visible: usize,
    on_change: Box<dyn FnMut(&str, &str)>,
    on_cancel: Box<dyn FnMut()>,
    search_input: Option<Input>,
    search_enabled: bool,
    submenu_component: Option<ComponentBox>,
    /// Display-list selection index to restore when submenu closes.
    submenu_display_index: Option<usize>,
    /// Index into `items` for the row that opened the submenu.
    submenu_item_idx: Option<usize>,
    /// Written by `done` (sync or async); drained after input/render.
    submenu_pending: SubmenuPending,
    cached: Vec<Line>,
}

impl SettingsList {
    #[must_use]
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: Box<dyn FnMut(&str, &str)>,
        on_cancel: Box<dyn FnMut()>,
        options: SettingsListOptions,
    ) -> Self {
        let filtered_indices: Vec<usize> = (0..items.len()).collect();
        let search_enabled = options.enable_search;
        let search_input = if search_enabled {
            Some(Input::new())
        } else {
            None
        };
        Self {
            items,
            filtered_indices,
            theme,
            selected_index: 0,
            max_visible,
            on_change,
            on_cancel,
            search_input,
            search_enabled,
            submenu_component: None,
            submenu_display_index: None,
            submenu_item_idx: None,
            submenu_pending: Rc::new(RefCell::new(None)),
            cached: Vec::new(),
        }
    }

    /// Update an item's `current_value` by id.
    pub fn update_value(&mut self, id: &str, new_value: impl Into<String>) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.current_value = new_value.into();
        }
    }

    /// Whether a submenu is currently open.
    #[must_use]
    pub fn is_submenu_open(&self) -> bool {
        self.submenu_component.is_some()
    }

    fn apply_filter(&mut self, query: &str) {
        if query.trim().is_empty() {
            self.filtered_indices = (0..self.items.len()).collect();
            self.selected_index = 0;
            return;
        }

        let tokens: Vec<&str> = query
            .trim()
            .split(|c: char| c.is_whitespace() || c == '/')
            .filter(|t| !t.is_empty())
            .collect();

        if tokens.is_empty() {
            self.filtered_indices = (0..self.items.len()).collect();
            self.selected_index = 0;
            return;
        }

        let mut scored: Vec<(usize, f64)> = Vec::new();
        for (idx, item) in self.items.iter().enumerate() {
            let mut total = 0.0_f64;
            let mut all = true;
            for token in &tokens {
                let m = fuzzy_match(token, &item.label);
                if m.matches {
                    total += m.score;
                } else {
                    all = false;
                    break;
                }
            }
            if all {
                scored.push((idx, total));
            }
        }
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        self.filtered_indices = scored.into_iter().map(|(i, _)| i).collect();
        self.selected_index = 0;
    }

    fn close_submenu(&mut self) {
        self.submenu_component = None;
        self.submenu_item_idx = None;
        *self.submenu_pending.borrow_mut() = None;
        if let Some(idx) = self.submenu_display_index.take() {
            self.selected_index = idx;
        }
    }

    /// Drain `done` results written by the submenu (sync or async).
    fn drain_submenu_pending(&mut self) {
        let Some(selected) = self.submenu_pending.borrow_mut().take() else {
            return;
        };
        if let Some(val) = selected
            && let Some(item_idx) = self.submenu_item_idx
            && let Some(item) = self.items.get_mut(item_idx)
        {
            item.current_value = val.clone();
            let id = item.id.clone();
            (self.on_change)(&id, &val);
        }
        self.close_submenu();
    }

    fn activate_item(&mut self) {
        let indices = if self.search_enabled {
            self.filtered_indices.clone()
        } else {
            (0..self.items.len()).collect()
        };
        let Some(&item_idx) = indices.get(self.selected_index) else {
            return;
        };
        let has_submenu = self.items[item_idx].submenu.is_some();
        if has_submenu {
            self.submenu_display_index = Some(self.selected_index);
            self.submenu_item_idx = Some(item_idx);
            *self.submenu_pending.borrow_mut() = None;

            let current_value = self.items[item_idx].current_value.clone();
            let mut factory = self.items[item_idx].submenu.take().expect("has_submenu");
            let pending = Rc::clone(&self.submenu_pending);
            let done: Box<dyn FnMut(Option<String>)> = Box::new(move |v| {
                *pending.borrow_mut() = Some(v);
            });
            let component = factory(&current_value, done);
            self.items[item_idx].submenu = Some(factory);
            self.submenu_component = Some(component);

            // Sync done during construction.
            self.drain_submenu_pending();
            return;
        }

        if let Some(values) = self.items[item_idx].values.clone() {
            if values.is_empty() {
                return;
            }
            let current = &self.items[item_idx].current_value;
            let current_index = values.iter().position(|v| v == current).unwrap_or(0);
            let next_index = (current_index + 1) % values.len();
            let new_value = values[next_index].clone();
            let id = self.items[item_idx].id.clone();
            self.items[item_idx].current_value = new_value.clone();
            (self.on_change)(&id, &new_value);
        }
    }

    fn add_hint_line(&self, lines: &mut Vec<Line>, width: usize) {
        lines.push(Line::empty());
        let hint = if self.search_enabled {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        let styled = (self.theme.hint)(hint);
        let truncated = truncate_no_ellipsis(&styled, width);
        lines.push(Line::from_ansi(&truncated));
    }

    fn render_main_list(&mut self, width: u16) -> Vec<Line> {
        let w = width as usize;
        let mut lines: Vec<Line> = Vec::new();

        if self.search_enabled
            && let Some(input) = &mut self.search_input
        {
            lines.extend_from_slice(input.render(width));
            lines.push(Line::empty());
        }

        if self.items.is_empty() {
            let msg = (self.theme.hint)("  No settings available");
            lines.push(Line::from_ansi(&msg));
            if self.search_enabled {
                self.add_hint_line(&mut lines, w);
            }
            return lines;
        }

        let display: Vec<usize> = if self.search_enabled {
            self.filtered_indices.clone()
        } else {
            (0..self.items.len()).collect()
        };

        if display.is_empty() {
            let msg = (self.theme.hint)("  No matching settings");
            lines.push(Line::from_ansi(&truncate_no_ellipsis(&msg, w)));
            self.add_hint_line(&mut lines, w);
            return lines;
        }

        let start_index = {
            let half = self.max_visible / 2;
            let max_start = display.len().saturating_sub(self.max_visible);
            self.selected_index.saturating_sub(half).min(max_start)
        };
        let end_index = (start_index + self.max_visible).min(display.len());

        let max_label_width = self
            .items
            .iter()
            .map(|item| visible_width(&item.label))
            .max()
            .unwrap_or(0)
            .min(30);

        for (i, &item_idx) in display.iter().enumerate().take(end_index).skip(start_index) {
            let item = &self.items[item_idx];
            let is_selected = i == self.selected_index;
            let prefix = if is_selected {
                self.theme.cursor.as_str()
            } else {
                "  "
            };
            let prefix_width = visible_width(prefix);

            let label_pad = max_label_width.saturating_sub(visible_width(&item.label));
            let label_padded = format!("{}{}", item.label, " ".repeat(label_pad));
            let label_text = (self.theme.label)(&label_padded, is_selected);

            let separator = "  ";
            let used = prefix_width + max_label_width + visible_width(separator);
            let value_max = w.saturating_sub(used).saturating_sub(2);
            let value_trunc = truncate_no_ellipsis(&item.current_value, value_max);
            let value_text = (self.theme.value)(&value_trunc, is_selected);

            let full = format!("{prefix}{label_text}{separator}{value_text}");
            lines.push(Line::from_ansi(&truncate_no_ellipsis(&full, w)));
        }

        if start_index > 0 || end_index < display.len() {
            let scroll = format!("  ({}/{})", self.selected_index + 1, display.len());
            let trunc = truncate_no_ellipsis(&scroll, w.saturating_sub(2));
            let styled = (self.theme.hint)(&trunc);
            lines.push(Line::from_ansi(&styled));
        }

        if let Some(&item_idx) = display.get(self.selected_index)
            && let Some(desc) = &self.items[item_idx].description
        {
            lines.push(Line::empty());
            let wrapped = wrap_text_with_ansi(desc, w.saturating_sub(4));
            for line in wrapped {
                let styled = (self.theme.description)(&format!("  {line}"));
                lines.push(Line::from_ansi(&styled));
            }
        }

        self.add_hint_line(&mut lines, w);
        lines
    }
}

fn truncate_no_ellipsis(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_owned();
    }
    let mut s = truncate_to_width(text, max_width);
    if s.ends_with("\x1b[0m") {
        s.truncate(s.len() - 4);
    }
    s
}

impl Component for SettingsList {
    fn render(&mut self, width: u16) -> &[Line] {
        // Submenu may call done during its own render.
        if self.submenu_component.is_some() {
            let lines = {
                let sub = self.submenu_component.as_mut().expect("checked");
                sub.render(width).to_vec()
            };
            self.drain_submenu_pending();
            if self.submenu_component.is_none() {
                // Closed during render — show main list.
                self.cached = self.render_main_list(width);
                return &self.cached;
            }
            self.cached = lines;
            return &self.cached;
        }
        self.cached = self.render_main_list(width);
        &self.cached
    }

    fn invalidate(&mut self) {
        if let Some(sub) = &mut self.submenu_component {
            sub.invalidate();
        }
    }

    fn handle_input(&mut self, data: &str) {
        if self.submenu_component.is_some() {
            if let Some(sub) = &mut self.submenu_component {
                sub.handle_input(data);
            }
            self.drain_submenu_pending();
            return;
        }

        let kb = get_keybindings();
        let display_len = if self.search_enabled {
            self.filtered_indices.len()
        } else {
            self.items.len()
        };

        if kb.matches(data, "tui.select.up") {
            if display_len == 0 {
                return;
            }
            self.selected_index = if self.selected_index == 0 {
                display_len - 1
            } else {
                self.selected_index - 1
            };
        } else if kb.matches(data, "tui.select.down") {
            if display_len == 0 {
                return;
            }
            self.selected_index = if self.selected_index == display_len - 1 {
                0
            } else {
                self.selected_index + 1
            };
        } else if kb.matches(data, "tui.select.confirm") || data == " " {
            self.activate_item();
        } else if kb.matches(data, "tui.select.cancel") {
            (self.on_cancel)();
        } else if self.search_enabled {
            let sanitized: String = data.chars().filter(|c| *c != ' ').collect();
            if sanitized.is_empty() {
                return;
            }
            if let Some(input) = &mut self.search_input {
                input.handle_input(&sanitized);
                let q = input.get_value().to_owned();
                self.apply_filter(&q);
            }
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

#[cfg(test)]
mod tests {
    /// Regression: typing into the search box re-enters the global
    /// keybindings registry through `Input::handle_input`; a held guard
    /// (instead of a snapshot) deadlocks here.
    #[test]
    fn search_typing_filters_without_deadlock() {
        use super::*;
        let items = vec![
            SettingItem {
                id: "a".into(),
                label: "Alpha".into(),
                description: Some("First".into()),
                current_value: "true".into(),
                values: Some(vec!["true".into(), "false".into()]),
                submenu: None,
            },
            SettingItem {
                id: "w".into(),
                label: "Warnings".into(),
                description: None,
                current_value: "configure".into(),
                values: None,
                submenu: None,
            },
        ];
        let mut list = SettingsList::new(
            items,
            10,
            SettingsListTheme::identity(),
            Box::new(|_, _| {}),
            Box::new(|| {}),
            SettingsListOptions {
                enable_search: true,
            },
        );
        let _ = list.render(80);
        list.handle_input("w");
        let lines: Vec<String> = list.render(80).iter().map(Line::to_ansi).collect();
        let joined = lines.join("\n");
        assert!(joined.contains("Warnings"), "{joined}");
        assert!(!joined.contains("Alpha"), "{joined}");
    }

    use super::*;
    use crate::component::{Component, RenderStatus};
    use crate::line::Line;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Dummy submenu that stores `done` and fires it on next input.
    struct DeferredSubmenu {
        done: Option<Box<dyn FnMut(Option<String>)>>,
        value: String,
        cached: Vec<Line>,
    }

    impl Component for DeferredSubmenu {
        fn render(&mut self, _width: u16) -> &[Line] {
            self.cached = vec![Line::plain(format!("submenu:{}", self.value))];
            &self.cached
        }

        fn invalidate(&mut self) {}

        fn handle_input(&mut self, data: &str) {
            if data == "confirm" {
                if let Some(mut done) = self.done.take() {
                    done(Some("picked".to_owned()));
                }
            } else if data == "cancel"
                && let Some(mut done) = self.done.take()
            {
                done(None);
            }
        }

        fn last_render_status(&self) -> RenderStatus {
            RenderStatus::Changed
        }
    }

    #[test]
    fn submenu_async_done_applies_value_and_closes() {
        let changes: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let changes_cb = Rc::clone(&changes);

        let mut item = SettingItem::new("theme", "Theme", "dark");
        item.submenu = Some(Box::new(|current, done| {
            Box::new(DeferredSubmenu {
                done: Some(done),
                value: current.to_owned(),
                cached: Vec::new(),
            })
        }));

        let mut list = SettingsList::new(
            vec![item],
            5,
            SettingsListTheme::identity(),
            Box::new(move |id, val| {
                changes_cb
                    .borrow_mut()
                    .push((id.to_owned(), val.to_owned()));
            }),
            Box::new(|| {}),
            SettingsListOptions::default(),
        );

        // Open submenu via Enter.
        list.handle_input("\r");
        assert!(list.is_submenu_open());
        {
            let lines = list.render(40);
            assert!(lines[0].plain_text().contains("submenu:dark"));
        }
        assert!(list.is_submenu_open());

        // Async done on confirm.
        list.handle_input("confirm");
        assert!(!list.is_submenu_open());
        assert_eq!(list.items[0].current_value, "picked");
        assert_eq!(
            changes.borrow().as_slice(),
            &[("theme".to_owned(), "picked".to_owned())]
        );
    }

    #[test]
    fn submenu_async_cancel_closes_without_change() {
        let changes: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let changes_cb = Rc::clone(&changes);

        let mut item = SettingItem::new("theme", "Theme", "dark");
        item.submenu = Some(Box::new(|current, done| {
            Box::new(DeferredSubmenu {
                done: Some(done),
                value: current.to_owned(),
                cached: Vec::new(),
            })
        }));

        let mut list = SettingsList::new(
            vec![item],
            5,
            SettingsListTheme::identity(),
            Box::new(move |id, val| {
                changes_cb
                    .borrow_mut()
                    .push((id.to_owned(), val.to_owned()));
            }),
            Box::new(|| {}),
            SettingsListOptions::default(),
        );

        list.handle_input("\r");
        assert!(list.is_submenu_open());
        list.handle_input("cancel");
        assert!(!list.is_submenu_open());
        assert_eq!(list.items[0].current_value, "dark");
        assert!(changes.borrow().is_empty());
    }

    #[test]
    fn value_cycle_on_enter() {
        let changes: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let changes_cb = Rc::clone(&changes);

        let mut item = SettingItem::new("mode", "Mode", "a");
        item.values = Some(vec!["a".into(), "b".into(), "c".into()]);

        let mut list = SettingsList::new(
            vec![item],
            5,
            SettingsListTheme::identity(),
            Box::new(move |id, val| {
                changes_cb
                    .borrow_mut()
                    .push((id.to_owned(), val.to_owned()));
            }),
            Box::new(|| {}),
            SettingsListOptions::default(),
        );

        list.handle_input("\r");
        assert_eq!(list.items[0].current_value, "b");
        list.handle_input(" ");
        assert_eq!(list.items[0].current_value, "c");
        assert_eq!(changes.borrow().len(), 2);
    }
}
