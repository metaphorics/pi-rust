//! Terminal trait + process implementation.
//!
//! Port of `packages/tui/src/terminal.ts`.

mod process;

pub use process::{
    DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS, KITTY_KEYBOARD_PROTOCOL_QUERY,
    KeyboardProtocolNegotiationSequence, ProcessTerminal, TERMINAL_BACKGROUND_COLOR_QUERY,
    is_apple_terminal_session, normalize_apple_terminal_input,
    parse_keyboard_protocol_negotiation_sequence, parse_terminal_background_color_response,
};

/// Minimal terminal interface for TUI (mirrors TS `Terminal`).
pub trait Terminal {
    /// Start the terminal with input and resize handlers.
    ///
    /// `on_input` receives raw sequence strings (not decoded KeyEvent).
    /// After `start`, the host event loop MUST call [`poll`](Self::poll)
    /// regularly so stdin segments and SIGWINCH reach the handlers
    /// (Node's `stdin.on('data')` / `stdout.on('resize')` are push-based;
    /// this Rust port is poll-based on the TUI thread).
    fn start(
        &mut self,
        on_input: Box<dyn FnMut(&str) + 'static>,
        on_resize: Box<dyn FnMut() + 'static>,
    );

    /// Drain pending stdin + resize. Call from the TUI event loop after `start`.
    fn poll(&mut self);

    /// Stop the terminal and restore state.
    fn stop(&mut self);

    /// Drain stdin before exit so Kitty key releases do not leak to the shell.
    fn drain_input(&mut self, max_ms: u64, idle_ms: u64);

    /// Write output to the terminal.
    fn write(&mut self, data: &str);

    fn columns(&self) -> u16;
    fn rows(&self) -> u16;

    /// Whether Kitty keyboard protocol is active.
    fn kitty_protocol_active(&self) -> bool;

    /// Move cursor up (negative) or down (positive) by N lines.
    fn move_by(&mut self, lines: i32);

    fn hide_cursor(&mut self);
    fn show_cursor(&mut self);

    fn clear_line(&mut self);
    fn clear_from_cursor(&mut self);
    fn clear_screen(&mut self);

    fn set_title(&mut self, title: &str);
    fn set_progress(&mut self, active: bool);
}
