//! Interactive theme selector.

use pi_tui::components::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, Line, RenderStatus};

use super::dynamic_border::DynamicBorder;
use crate::modes::interactive::theme::{get_available_themes, get_select_list_theme};

const MAX_VISIBLE: usize = 10;

/// Selects a theme, previewing each highlighted item.
pub struct ThemeSelectorComponent {
    top_border: DynamicBorder,
    select_list: SelectList,
    bottom_border: DynamicBorder,
    cached: Vec<Line>,
}

impl ThemeSelectorComponent {
    #[must_use]
    pub fn new(
        current_theme: &str,
        mut on_select: Box<dyn FnMut(String)>,
        on_cancel: Box<dyn FnMut()>,
        mut on_preview: Box<dyn FnMut(String)>,
    ) -> Self {
        let themes = get_available_themes();
        let items = themes
            .iter()
            .map(|name| {
                let item = SelectItem::new(name, name);
                if name == current_theme {
                    item.with_description("(current)")
                } else {
                    item
                }
            })
            .collect();
        let layout = SelectListLayoutOptions {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(32),
            ..Default::default()
        };
        let mut select_list = SelectList::new(items, MAX_VISIBLE, get_select_list_theme(), layout);
        if let Some(index) = themes.iter().position(|name| name == current_theme) {
            select_list.set_selected_index(index);
        }
        select_list.on_select = Some(Box::new(move |item| on_select(item.value.clone())));
        select_list.on_cancel = Some(on_cancel);
        select_list.on_selection_change =
            Some(Box::new(move |item| on_preview(item.value.clone())));

        Self {
            top_border: DynamicBorder::default(),
            select_list,
            bottom_border: DynamicBorder::default(),
            cached: Vec::new(),
        }
    }

    pub fn get_select_list(&mut self) -> &mut SelectList {
        &mut self.select_list
    }
}

impl Component for ThemeSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.extend_from_slice(self.top_border.render(width));
        self.cached
            .extend_from_slice(self.select_list.render(width));
        self.cached
            .extend_from_slice(self.bottom_border.render(width));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.top_border.invalidate();
        self.select_list.invalidate();
        self.bottom_border.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        self.select_list.handle_input(data);
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}
