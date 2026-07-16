//! `VtTerminal` — pi-tui [`Terminal`] backed by the pi-vt cell-grid screen.
//!
//! The test-side replacement for pi's @xterm/headless harness: every byte the
//! Tui writes is interpreted by a real VT state machine, and assertions read
//! the FINAL SCREEN GRID (rows/cells/styles/sync-frame accounting) — never a
//! raw write log or the Tui's retained buffer.
//!
//! ```ignore
//! let (term, handle) = VtTerminal::new(100, 30);
//! let mut tui = Tui::new(term);
//! // ... mount components, tui.start_render_loop_hooks() / render ...
//! handle.send_input("\r");
//! tui.poll_terminal();
//! assert!(handle.screen(|s| s.contains("expected text")));
//! assert_eq!(handle.screen(|s| s.cells_mutated_outside_sync()), 0);
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use pi_tui::terminal::Terminal;
use pi_vt::VtScreen;

struct Shared {
    screen: RefCell<VtScreen>,
    input_queue: RefCell<VecDeque<String>>,
    resize_pending: RefCell<bool>,
    size: RefCell<(u16, u16)>,
    kitty: RefCell<bool>,
    stopped: RefCell<bool>,
}

/// Test-side handle: inject input, resize, inspect the screen.
#[derive(Clone)]
pub struct VtHandle {
    shared: Rc<Shared>,
}

impl VtHandle {
    /// Queue raw input (delivered to the Tui on the next `poll`).
    pub fn send_input(&self, data: &str) {
        self.shared
            .input_queue
            .borrow_mut()
            .push_back(data.to_owned());
    }

    /// Resize the emulated terminal; the resize event fires on the next `poll`.
    pub fn resize(&self, cols: u16, rows: u16) {
        *self.shared.size.borrow_mut() = (cols, rows);
        self.shared.screen.borrow_mut().resize(cols, rows);
        *self.shared.resize_pending.borrow_mut() = true;
    }

    /// Read the screen grid.
    pub fn screen<R>(&self, f: impl FnOnce(&VtScreen) -> R) -> R {
        f(&self.shared.screen.borrow())
    }

    /// Toggle emulated Kitty keyboard protocol support.
    pub fn set_kitty(&self, active: bool) {
        *self.shared.kitty.borrow_mut() = active;
    }

    pub fn stopped(&self) -> bool {
        *self.shared.stopped.borrow()
    }
}

/// pi-tui `Terminal` writing into a [`VtScreen`].
type InputHandler = Box<dyn FnMut(&str) + 'static>;
type ResizeHandler = Box<dyn FnMut() + 'static>;

pub struct VtTerminal {
    shared: Rc<Shared>,
    on_input: Option<InputHandler>,
    on_resize: Option<ResizeHandler>,
}

impl VtTerminal {
    pub fn new(cols: u16, rows: u16) -> (VtTerminal, VtHandle) {
        let shared = Rc::new(Shared {
            screen: RefCell::new(VtScreen::new(cols, rows)),
            input_queue: RefCell::new(VecDeque::new()),
            resize_pending: RefCell::new(false),
            size: RefCell::new((cols, rows)),
            kitty: RefCell::new(false),
            stopped: RefCell::new(false),
        });
        (
            VtTerminal {
                shared: Rc::clone(&shared),
                on_input: None,
                on_resize: None,
            },
            VtHandle { shared },
        )
    }

    fn feed(&self, data: &str) {
        self.shared.screen.borrow_mut().feed_str(data);
    }
}

impl Terminal for VtTerminal {
    fn start(
        &mut self,
        on_input: Box<dyn FnMut(&str) + 'static>,
        on_resize: Box<dyn FnMut() + 'static>,
    ) {
        self.on_input = Some(on_input);
        self.on_resize = Some(on_resize);
    }

    fn poll(&mut self) {
        if std::mem::take(&mut *self.shared.resize_pending.borrow_mut())
            && let Some(on_resize) = self.on_resize.as_mut()
        {
            on_resize();
        }
        loop {
            let next = self.shared.input_queue.borrow_mut().pop_front();
            let Some(data) = next else { break };
            if let Some(on_input) = self.on_input.as_mut() {
                on_input(&data);
            }
        }
    }

    fn stop(&mut self) {
        *self.shared.stopped.borrow_mut() = true;
    }

    fn drain_input(&mut self, _max_ms: u64, _idle_ms: u64) {
        self.shared.input_queue.borrow_mut().clear();
    }

    fn write(&mut self, data: &str) {
        self.feed(data);
    }

    fn columns(&self) -> u16 {
        self.shared.size.borrow().0
    }

    fn rows(&self) -> u16 {
        self.shared.size.borrow().1
    }

    fn kitty_protocol_active(&self) -> bool {
        *self.shared.kitty.borrow()
    }

    fn move_by(&mut self, lines: i32) {
        if lines > 0 {
            self.feed(&format!("\x1b[{lines}B"));
        } else if lines < 0 {
            let n = -lines;
            self.feed(&format!("\x1b[{n}A"));
        }
    }

    fn hide_cursor(&mut self) {
        self.feed("\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.feed("\x1b[?25h");
    }

    fn clear_line(&mut self) {
        self.feed("\x1b[K");
    }

    fn clear_from_cursor(&mut self) {
        self.feed("\x1b[J");
    }

    fn clear_screen(&mut self) {
        self.feed("\x1b[2J\x1b[H");
    }

    fn set_title(&mut self, title: &str) {
        self.feed(&format!("\x1b]0;{title}\x07"));
    }

    fn set_progress(&mut self, _active: bool) {}
}
