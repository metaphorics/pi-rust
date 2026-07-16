//! Interactive inline-image preference selector.

use pi_tui::components::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, Line, RenderStatus};

use super::dynamic_border::DynamicBorder;
use crate::modes::interactive::theme::get_select_list_theme;

/// Selects whether images are rendered inline or as text placeholders.
pub struct ShowImagesSelectorComponent {
    top_border: DynamicBorder,
    select_list: SelectList,
    bottom_border: DynamicBorder,
    cached: Vec<Line>,
}

impl ShowImagesSelectorComponent {
    #[must_use]
    pub fn new(
        current_value: bool,
        mut on_select: Box<dyn FnMut(bool)>,
        on_cancel: Box<dyn FnMut()>,
    ) -> Self {
        let items = vec![
            SelectItem::new("yes", "Yes").with_description("Show images inline in terminal"),
            SelectItem::new("no", "No").with_description("Show text placeholder instead"),
        ];
        let layout = SelectListLayoutOptions {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(32),
            ..Default::default()
        };
        let mut select_list = SelectList::new(items, 5, get_select_list_theme(), layout);
        select_list.set_selected_index(usize::from(!current_value));
        select_list.on_select = Some(Box::new(move |item| on_select(item.value == "yes")));
        select_list.on_cancel = Some(on_cancel);

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

impl Component for ShowImagesSelectorComponent {
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
