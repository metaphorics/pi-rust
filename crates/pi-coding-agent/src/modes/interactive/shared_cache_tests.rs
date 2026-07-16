//! Cache-behavior tests for shared interactive component adapters.

use std::cell::RefCell;
use std::rc::Rc;

use pi_tui::component::{Component, RenderStatus};
use pi_tui::components::Text;
use pi_tui::line::Line;

use super::shared::{Shared, SlotHandle, SwapSlot};

struct Counted {
    renders: usize,
    status: RenderStatus,
    cache: Vec<Line>,
}

impl Counted {
    fn new() -> Self {
        Self {
            renders: 0,
            status: RenderStatus::Changed,
            cache: Vec::new(),
        }
    }
}

impl Component for Counted {
    fn render(&mut self, _width: u16) -> &[Line] {
        self.renders += 1;
        self.cache = vec![Line::plain("cached")];
        self.status = RenderStatus::Unchanged;
        &self.cache
    }

    fn invalidate(&mut self) {
        self.status = RenderStatus::Changed;
    }

    fn last_render_status(&self) -> RenderStatus {
        self.status
    }
}

#[test]
fn shared_adapter_skips_unchanged_inner_render_and_reacts_to_invalidation() {
    let inner = Rc::new(RefCell::new(Counted::new()));
    let mut shared = Shared::new(inner.clone());

    assert_eq!(shared.render(80)[0].plain_text(), "cached");
    assert_eq!(inner.borrow().renders, 1);
    assert_eq!(shared.last_render_status(), RenderStatus::Unchanged);

    assert_eq!(shared.render(80)[0].plain_text(), "cached");
    assert_eq!(
        inner.borrow().renders,
        1,
        "unchanged inner was rendered again"
    );
    assert_eq!(shared.last_render_status(), RenderStatus::Unchanged);

    inner.borrow_mut().invalidate();
    assert_eq!(shared.render(80)[0].plain_text(), "cached");
    assert_eq!(inner.borrow().renders, 2);
}

#[test]
fn swap_slot_renders_a_replaced_occupant_at_the_same_width() {
    let handle = SlotHandle::new(Box::new(Text::with_text("editor")));
    let mut slot = SwapSlot::new(handle.clone());
    assert!(
        slot.render(80)
            .iter()
            .any(|line| line.plain_text().contains("editor"))
    );

    handle.replace(Box::new(Text::with_text("selector")));
    assert!(
        slot.render(80)
            .iter()
            .any(|line| line.plain_text().contains("selector"))
    );
}
