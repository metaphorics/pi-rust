//! SelectList — filterable selectable list with scroll window.
//!
//! Port of `packages/tui/src/components/select-list.ts`.

use crate::component::{Component, RenderStatus};
use crate::keybindings::get_keybindings;
use crate::line::Line;
use crate::util::{truncate_to_width, visible_width};

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

fn normalize_to_single_line(text: &str) -> String {
    text.split(['\r', '\n'])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn clamp(value: usize, min: usize, max: usize) -> usize {
    value.max(min).min(max)
}

/// One selectable row.
#[derive(Debug, Clone)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl SelectItem {
    #[must_use]
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Theme callbacks for SelectList rendering.
pub struct SelectListTheme {
    pub selected_prefix: Box<dyn Fn(&str) -> String>,
    pub selected_text: Box<dyn Fn(&str) -> String>,
    pub description: Box<dyn Fn(&str) -> String>,
    pub scroll_info: Box<dyn Fn(&str) -> String>,
    pub no_match: Box<dyn Fn(&str) -> String>,
}

impl SelectListTheme {
    /// Identity theme (no styling) for tests / plain terminals.
    #[must_use]
    pub fn identity() -> Self {
        Self {
            selected_prefix: Box::new(|s| s.to_owned()),
            selected_text: Box::new(|s| s.to_owned()),
            description: Box::new(|s| s.to_owned()),
            scroll_info: Box::new(|s| s.to_owned()),
            no_match: Box::new(|s| s.to_owned()),
        }
    }
}

/// Context for custom primary-column truncation.
pub struct SelectListTruncatePrimaryContext<'a> {
    pub text: &'a str,
    pub max_width: usize,
    pub column_width: usize,
    pub item: &'a SelectItem,
    pub is_selected: bool,
}

/// Layout knobs for the primary column.
#[derive(Default)]
pub struct SelectListLayoutOptions {
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
    pub truncate_primary: Option<Box<dyn Fn(SelectListTruncatePrimaryContext<'_>) -> String>>,
}


/// Filterable, keyboard-navigable list. Always re-renders (`RenderStatus::Changed`).
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered_items: Vec<SelectItem>,
    selected_index: usize,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayoutOptions,
    pub on_select: Option<Box<dyn FnMut(&SelectItem)>>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_selection_change: Option<Box<dyn FnMut(&SelectItem)>>,
    cached: Vec<Line>,
    last_status: RenderStatus,
}

impl SelectList {
    #[must_use]
    pub fn new(
        items: Vec<SelectItem>,
        max_visible: usize,
        theme: SelectListTheme,
        layout: SelectListLayoutOptions,
    ) -> Self {
        let filtered_items = items.clone();
        Self {
            items,
            filtered_items,
            selected_index: 0,
            max_visible,
            theme,
            layout,
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
            cached: Vec::new(),
            last_status: RenderStatus::Changed,
        }
    }

    pub fn set_filter(&mut self, filter: &str) {
        let lower = filter.to_lowercase();
        self.filtered_items = self
            .items
            .iter()
            .filter(|item| item.value.to_lowercase().starts_with(&lower))
            .cloned()
            .collect();
        self.selected_index = 0;
    }

    pub fn set_selected_index(&mut self, index: usize) {
        if self.filtered_items.is_empty() {
            self.selected_index = 0;
            return;
        }
        self.selected_index = index.min(self.filtered_items.len() - 1);
    }

    #[must_use]
    pub fn get_selected_item(&self) -> Option<&SelectItem> {
        self.filtered_items.get(self.selected_index)
    }

    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn get_display_value(item: &SelectItem) -> &str {
        if item.label.is_empty() {
            &item.value
        } else {
            &item.label
        }
    }

    fn get_primary_column_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1))
    }

    fn get_primary_column_width(&self) -> usize {
        let (min, max) = self.get_primary_column_bounds();
        let widest = self.filtered_items.iter().fold(0usize, |widest, item| {
            widest.max(visible_width(Self::get_display_value(item)) + PRIMARY_COLUMN_GAP)
        });
        clamp(widest, min, max)
    }

    fn truncate_primary(
        &self,
        item: &SelectItem,
        is_selected: bool,
        max_width: usize,
        column_width: usize,
    ) -> String {
        let display = Self::get_display_value(item);
        let truncated = if let Some(custom) = &self.layout.truncate_primary {
            custom(SelectListTruncatePrimaryContext {
                text: display,
                max_width,
                column_width,
                item,
                is_selected,
            })
        } else {
            // TS calls truncateToWidth(..., "") → no ellipsis.
            truncate_to_width_no_ellipsis(display, max_width)
        };
        truncate_to_width_no_ellipsis(&truncated, max_width)
    }

    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description_single_line: Option<&str>,
        primary_column_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);

        if let Some(desc) = description_single_line
            && width > 40 {
                let effective = primary_column_width
                    .min(width.saturating_sub(prefix_width).saturating_sub(4))
                    .max(1);
                let max_primary = effective.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
                let truncated_value =
                    self.truncate_primary(item, is_selected, max_primary, effective);
                let truncated_value_width = visible_width(&truncated_value);
                let spacing_len = effective.saturating_sub(truncated_value_width).max(1);
                let spacing = " ".repeat(spacing_len);
                let description_start = prefix_width + truncated_value_width + spacing_len;
                let remaining = width.saturating_sub(description_start).saturating_sub(2);

                if remaining > MIN_DESCRIPTION_WIDTH {
                    let truncated_desc = truncate_to_width_no_ellipsis(desc, remaining);
                    if is_selected {
                        let body = format!("{prefix}{truncated_value}{spacing}{truncated_desc}");
                        return (self.theme.selected_text)(&body);
                    }
                    let desc_text = (self.theme.description)(&format!("{spacing}{truncated_desc}"));
                    return format!("{prefix}{truncated_value}{desc_text}");
                }
            }

        let max_width = width.saturating_sub(prefix_width).saturating_sub(2);
        let truncated_value = self.truncate_primary(item, is_selected, max_width, max_width);
        if is_selected {
            let body = format!("{prefix}{truncated_value}");
            (self.theme.selected_text)(&body)
        } else {
            format!("{prefix}{truncated_value}")
        }
    }

    fn notify_selection_change(&mut self) {
        if let Some(item) = self.filtered_items.get(self.selected_index).cloned()
            && let Some(cb) = &mut self.on_selection_change {
                cb(&item);
            }
    }
}

/// Truncate without ellipsis (TS `truncateToWidth(text, max, "")`).
fn truncate_to_width_no_ellipsis(text: &str, max_width: usize) -> String {
    // Our util always appends reset and uses no ellipsis by design for the
    // simple path — matching empty-ellipsis TS when max_width clips content.
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_owned();
    }
    // Reuse util truncate (no ellipsis in Rust port) then strip trailing reset if present.
    let mut s = truncate_to_width(text, max_width);
    if s.ends_with("\x1b[0m") {
        s.truncate(s.len() - 4);
    }
    s
}

impl Component for SelectList {
    fn render(&mut self, width: u16) -> &[Line] {
        let w = width as usize;
        let mut lines: Vec<Line> = Vec::new();

        if self.filtered_items.is_empty() {
            let msg = (self.theme.no_match)("  No matching commands");
            lines.push(Line::from_ansi(&msg));
            self.cached = lines;
            self.last_status = RenderStatus::Changed;
            return &self.cached;
        }

        let primary_column_width = self.get_primary_column_width();
        let start_index = {
            let half = self.max_visible / 2;
            let max_start = self.filtered_items.len().saturating_sub(self.max_visible);
            self.selected_index.saturating_sub(half).min(max_start)
        };
        let end_index = (start_index + self.max_visible).min(self.filtered_items.len());

        for i in start_index..end_index {
            let item = &self.filtered_items[i];
            let is_selected = i == self.selected_index;
            let desc = item
                .description
                .as_ref()
                .map(|d| normalize_to_single_line(d));
            let line =
                self.render_item(item, is_selected, w, desc.as_deref(), primary_column_width);
            lines.push(Line::from_ansi(&line));
        }

        if start_index > 0 || end_index < self.filtered_items.len() {
            let scroll_text = format!(
                "  ({}/{})",
                self.selected_index + 1,
                self.filtered_items.len()
            );
            let truncated = truncate_to_width_no_ellipsis(&scroll_text, w.saturating_sub(2));
            let styled = (self.theme.scroll_info)(&truncated);
            lines.push(Line::from_ansi(&styled));
        }

        self.cached = lines;
        self.last_status = RenderStatus::Changed;
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn handle_input(&mut self, key_data: &str) {
        let kb = get_keybindings();
        if kb.matches(key_data, "tui.select.up") {
            if self.filtered_items.is_empty() {
                return;
            }
            self.selected_index = if self.selected_index == 0 {
                self.filtered_items.len() - 1
            } else {
                self.selected_index - 1
            };
            self.notify_selection_change();
        } else if kb.matches(key_data, "tui.select.down") {
            if self.filtered_items.is_empty() {
                return;
            }
            self.selected_index = if self.selected_index == self.filtered_items.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
            self.notify_selection_change();
        } else if kb.matches(key_data, "tui.select.confirm") {
            if let Some(item) = self.filtered_items.get(self.selected_index).cloned()
                && let Some(cb) = &mut self.on_select {
                    cb(&item);
                }
        } else if kb.matches(key_data, "tui.select.cancel")
            && let Some(cb) = &mut self.on_cancel {
                cb();
            }
    }

    fn last_render_status(&self) -> RenderStatus {
        // Always Changed (TS has no cache).
        RenderStatus::Changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("alpha", "Alpha"),
            SelectItem::new("beta", "Beta").with_description("second"),
            SelectItem::new("gamma", "Gamma"),
        ]
    }

    #[test]
    fn filter_prefix_and_no_match_message() {
        let mut list = SelectList::new(items(), 5, SelectListTheme::identity(), Default::default());
        list.set_filter("be");
        assert_eq!(
            list.get_selected_item().map(|i| i.value.as_str()),
            Some("beta")
        );
        {
            let lines = list.render(60);
            assert!(lines[0].to_ansi().contains("→ ") || lines[0].to_ansi().contains("Beta"));
        }
        assert_eq!(list.last_render_status(), RenderStatus::Changed);

        list.set_filter("zzz");
        let lines = list.render(60);
        assert!(lines[0].to_ansi().contains("No matching commands"));
    }

    #[test]
    fn selection_wraps_on_up_down() {
        let mut list = SelectList::new(items(), 5, SelectListTheme::identity(), Default::default());
        assert_eq!(list.selected_index(), 0);
        list.handle_input("\x1b[A"); // up from 0 → last
        assert_eq!(list.selected_index(), 2);
        list.handle_input("\x1b[B"); // down from last → 0
        assert_eq!(list.selected_index(), 0);
    }

    #[test]
    fn prefixes_selected_and_unselected() {
        let mut list = SelectList::new(items(), 5, SelectListTheme::identity(), Default::default());
        let lines: Vec<String> = list.render(80).iter().map(Line::to_ansi).collect();
        assert!(lines[0].contains("→ "));
        assert!(
            lines[1].starts_with("  ") || lines[1].contains("  Beta") || lines[1].contains("Beta")
        );
    }
}
