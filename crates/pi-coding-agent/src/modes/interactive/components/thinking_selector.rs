//! Interactive thinking-level selector.

use pi_ai::types::ModelThinkingLevel;
use pi_tui::components::{SelectItem, SelectList, SelectListLayoutOptions};
use pi_tui::{Component, Line, RenderStatus};

use super::dynamic_border::DynamicBorder;
use crate::modes::interactive::theme::get_select_list_theme;

fn level_name(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "off",
        ModelThinkingLevel::Minimal => "minimal",
        ModelThinkingLevel::Low => "low",
        ModelThinkingLevel::Medium => "medium",
        ModelThinkingLevel::High => "high",
        ModelThinkingLevel::Xhigh => "xhigh",
        ModelThinkingLevel::Max => "max",
    }
}

fn level_description(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "No reasoning",
        ModelThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ModelThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ModelThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ModelThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ModelThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ModelThinkingLevel::Max => "Maximum reasoning",
    }
}

/// Selects one of the thinking levels supported by the active model.
pub struct ThinkingSelectorComponent {
    top_border: DynamicBorder,
    select_list: SelectList,
    bottom_border: DynamicBorder,
    cached: Vec<Line>,
}

impl ThinkingSelectorComponent {
    #[must_use]
    pub fn new(
        current_level: ModelThinkingLevel,
        available_levels: &[ModelThinkingLevel],
        mut on_select: Box<dyn FnMut(ModelThinkingLevel)>,
        on_cancel: Box<dyn FnMut()>,
    ) -> Self {
        let items = available_levels
            .iter()
            .map(|&level| {
                SelectItem::new(level_name(level), level_name(level))
                    .with_description(level_description(level))
            })
            .collect();
        let layout = SelectListLayoutOptions {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(32),
            ..Default::default()
        };
        let mut select_list = SelectList::new(
            items,
            available_levels.len(),
            get_select_list_theme(),
            layout,
        );
        if let Some(index) = available_levels
            .iter()
            .position(|level| *level == current_level)
        {
            select_list.set_selected_index(index);
        }
        select_list.on_select = Some(Box::new(move |item| {
            let level = match item.value.as_str() {
                "off" => ModelThinkingLevel::Off,
                "minimal" => ModelThinkingLevel::Minimal,
                "low" => ModelThinkingLevel::Low,
                "medium" => ModelThinkingLevel::Medium,
                "high" => ModelThinkingLevel::High,
                "xhigh" => ModelThinkingLevel::Xhigh,
                "max" => ModelThinkingLevel::Max,
                _ => return,
            };
            on_select(level);
        }));
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

impl Component for ThinkingSelectorComponent {
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
