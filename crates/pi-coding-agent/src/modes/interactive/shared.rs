//! Shared-ownership component adapters for the interactive mode.
//!
//! pi's InteractiveMode keeps live references to mounted components (chat
//! container, footer, editor, status area) and mutates them from event
//! handlers. pi-tui's `Tui` owns its component tree by value, so the loop
//! shares components through `Rc<RefCell<_>>` and mounts these adapters.

use std::cell::{Cell, RefCell, RefMut};
use std::rc::Rc;

use pi_tui::component::{Component, Focusable, RenderStatus};
use pi_tui::line::Line;

/// Mountable view of an `Rc<RefCell<T>>` component.
///
/// `render` copies the inner component's lines into a local cache (the same
/// per-frame line copy `Container` performs for its children) and forwards
/// `last_render_status`, preserving DirtySpans correctness: cache hits on the
/// inner component still report `Unchanged`.
pub struct Shared<T: Component> {
    inner: Rc<RefCell<T>>,
    cache: Vec<Line>,
    status: RenderStatus,
    last_width: Option<u16>,
}

impl<T: Component> Shared<T> {
    pub fn new(inner: Rc<RefCell<T>>) -> Self {
        Self {
            inner,
            cache: Vec::new(),
            status: RenderStatus::Changed,
            last_width: None,
        }
    }
}

impl<T: Component + 'static> Component for Shared<T> {
    fn render(&mut self, width: u16) -> &[Line] {
        let needs_render = self.last_width != Some(width)
            || self.inner.borrow().last_render_status() != RenderStatus::Unchanged;
        if needs_render {
            let mut inner = self.inner.borrow_mut();
            self.cache.clear();
            self.cache.extend_from_slice(inner.render(width));
            self.status = inner.last_render_status();
            self.last_width = Some(width);
        } else {
            self.status = RenderStatus::Unchanged;
        }
        &self.cache
    }

    fn invalidate(&mut self) {
        self.inner.borrow_mut().invalidate();
        self.cache.clear();
        self.last_width = None;
        self.status = RenderStatus::Changed;
    }

    fn handle_input(&mut self, data: &str) {
        self.inner.borrow_mut().handle_input(data);
    }

    fn wants_key_release(&self) -> bool {
        self.inner.borrow().wants_key_release()
    }

    fn last_render_status(&self) -> RenderStatus {
        self.status
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        if self.inner.borrow_mut().as_focusable().is_some() {
            Some(self)
        } else {
            None
        }
    }
}

impl<T: Component + 'static> Focusable for Shared<T> {
    fn focused(&self) -> bool {
        self.inner
            .borrow_mut()
            .as_focusable()
            .is_some_and(|f| f.focused())
    }

    fn set_focused(&mut self, focused: bool) {
        if let Some(f) = self.inner.borrow_mut().as_focusable() {
            f.set_focused(focused);
        }
    }
}

/// Swappable mount point: the editor area shows either the editor or an
/// in-place selector (oracle `showSelector` swaps `editorContainer` children).
///
/// The occupant lives behind an `Rc` so the mode can swap it while the slot
/// stays mounted at a stable root index (focus routing is index-based).
#[derive(Clone)]
pub struct SlotHandle {
    occupant: Rc<RefCell<Box<dyn Component>>>,
    changed: Rc<Cell<bool>>,
}

impl SlotHandle {
    pub fn new(occupant: Box<dyn Component>) -> Self {
        Self {
            occupant: Rc::new(RefCell::new(occupant)),
            changed: Rc::new(Cell::new(true)),
        }
    }

    pub fn replace(&self, occupant: Box<dyn Component>) {
        *self.occupant.borrow_mut() = occupant;
        self.changed.set(true);
    }

    pub fn borrow_mut(&self) -> RefMut<'_, Box<dyn Component>> {
        self.occupant.borrow_mut()
    }
}

pub struct SwapSlot {
    occupant: SlotHandle,
    cache: Vec<Line>,
    status: RenderStatus,
    last_width: Option<u16>,
}

impl SwapSlot {
    pub fn new(occupant: SlotHandle) -> Self {
        Self {
            occupant,
            cache: Vec::new(),
            status: RenderStatus::Changed,
            last_width: None,
        }
    }
}

impl Component for SwapSlot {
    fn render(&mut self, width: u16) -> &[Line] {
        let replaced = self.occupant.changed.replace(false);
        let needs_render = replaced
            || self.last_width != Some(width)
            || self.occupant.occupant.borrow().last_render_status() != RenderStatus::Unchanged;
        if needs_render {
            let mut occupant = self.occupant.occupant.borrow_mut();
            self.cache.clear();
            self.cache.extend_from_slice(occupant.render(width));
            self.status = occupant.last_render_status();
            self.last_width = Some(width);
        } else {
            self.status = RenderStatus::Unchanged;
        }
        &self.cache
    }

    fn invalidate(&mut self) {
        self.occupant.occupant.borrow_mut().invalidate();
        self.cache.clear();
        self.last_width = None;
        self.status = RenderStatus::Changed;
    }

    fn handle_input(&mut self, data: &str) {
        self.occupant.occupant.borrow_mut().handle_input(data);
    }

    fn wants_key_release(&self) -> bool {
        self.occupant.occupant.borrow().wants_key_release()
    }

    fn last_render_status(&self) -> RenderStatus {
        self.status
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        if self.occupant.occupant.borrow_mut().as_focusable().is_some() {
            Some(self)
        } else {
            None
        }
    }
}

impl Focusable for SwapSlot {
    fn focused(&self) -> bool {
        self.occupant
            .occupant
            .borrow_mut()
            .as_focusable()
            .is_some_and(|f| f.focused())
    }

    fn set_focused(&mut self, focused: bool) {
        if let Some(f) = self.occupant.occupant.borrow_mut().as_focusable() {
            f.set_focused(focused);
        }
    }
}
