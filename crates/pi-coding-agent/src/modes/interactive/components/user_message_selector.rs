//! User message selector for branching — port of
//! `modes/interactive/components/user-message-selector.ts`.
//!
//! Deviation: the oracle auto-cancels via `setTimeout(onCancel, 100)` when
//! constructed with no messages; Rust has no timer here, so the component
//! exposes [`UserMessageSelectorComponent::poll_auto_cancel`] which the mode
//! loop calls after construction to fire the deferred cancel.

use pi_tui::component::{Component, Focusable};
use pi_tui::components::{Spacer, Text};
use pi_tui::keybindings::get_keybindings;
use pi_tui::line::Line;

use super::dynamic_border::DynamicBorder;
use super::session_selector::truncate_with_ellipsis;
use crate::modes::interactive::theme::{ThemeColor, theme};

/// Selection callback: receives the selected entry ID.
pub type OnSelectFn = Box<dyn FnMut(&str)>;
/// Cancel callback.
pub type OnCancelFn = Box<dyn FnMut()>;

/// One selectable user message.
#[derive(Debug, Clone)]
pub struct UserMessageItem {
    /// Entry ID in the session.
    pub id: String,
    /// The message text.
    pub text: String,
    /// Optional timestamp if available.
    pub timestamp: Option<String>,
}

/// Custom user message list component with selection.
///
/// Oracle `UserMessageList`.
pub struct UserMessageList {
    messages: Vec<UserMessageItem>,
    selected_index: usize,
    pub on_select: Option<OnSelectFn>,
    pub on_cancel: Option<OnCancelFn>,
    /// Max messages visible.
    max_visible: usize,
    lines: Vec<Line>,
}

impl UserMessageList {
    #[must_use]
    pub fn new(messages: Vec<UserMessageItem>, initial_selected_id: Option<&str>) -> Self {
        // Store messages in chronological order (oldest to newest)
        let initial_index =
            initial_selected_id.and_then(|id| messages.iter().position(|message| message.id == id));
        // Start with selected message if provided, else default to the most recent
        let selected_index = initial_index.unwrap_or(messages.len().saturating_sub(1));
        Self {
            messages,
            selected_index,
            on_select: None,
            on_cancel: None,
            max_visible: 10,
            lines: Vec::new(),
        }
    }
}

impl Component for UserMessageList {
    fn render(&mut self, width: u16) -> &[Line] {
        let t = theme();
        let mut lines: Vec<Line> = Vec::new();

        if self.messages.is_empty() {
            lines.push(Line::from_ansi(
                &t.fg(ThemeColor::Muted, "  No user messages found"),
            ));
            self.lines = lines;
            return &self.lines;
        }

        // Calculate visible range with scrolling
        let len = self.messages.len() as isize;
        let max_visible = self.max_visible as isize;
        let start_index = (self.selected_index as isize - max_visible / 2)
            .min(len - max_visible)
            .max(0);
        let end_index = (start_index + max_visible).min(len);

        // Render visible messages (2 lines per message + blank line)
        for i in start_index..end_index {
            let message = &self.messages[i as usize];
            let is_selected = i as usize == self.selected_index;

            // Normalize message to single line
            let normalized_message = message.text.replace('\n', " ").trim().to_owned();

            // First line: cursor + message
            let cursor = if is_selected {
                t.fg(ThemeColor::Accent, "› ")
            } else {
                "  ".to_owned()
            };
            let max_msg_width = usize::from(width).saturating_sub(2); // Account for cursor (2 chars)
            let truncated_msg = truncate_with_ellipsis(&normalized_message, max_msg_width, "...");
            let message_line = format!(
                "{cursor}{}",
                if is_selected {
                    t.bold(&truncated_msg)
                } else {
                    truncated_msg
                }
            );

            lines.push(Line::from_ansi(&message_line));

            // Second line: metadata (position in history)
            let position = i + 1;
            let metadata = format!("  Message {position} of {}", self.messages.len());
            let metadata_line = t.fg(ThemeColor::Muted, &metadata);
            lines.push(Line::from_ansi(&metadata_line));
            lines.push(Line::empty()); // Blank line between messages
        }

        // Add scroll indicator if needed
        if start_index > 0 || end_index < len {
            let scroll_info = t.fg(
                ThemeColor::Muted,
                &format!("  ({}/{})", self.selected_index + 1, self.messages.len()),
            );
            lines.push(Line::from_ansi(&scroll_info));
        }

        self.lines = lines;
        &self.lines
    }

    fn invalidate(&mut self) {
        // No cached state to invalidate currently
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        // Up arrow - go to previous (older) message, wrap to bottom when at top
        if kb.matches(data, "tui.select.up") {
            if !self.messages.is_empty() {
                self.selected_index = if self.selected_index == 0 {
                    self.messages.len() - 1
                } else {
                    self.selected_index - 1
                };
            }
        }
        // Down arrow - go to next (newer) message, wrap to top when at bottom
        else if kb.matches(data, "tui.select.down") {
            if !self.messages.is_empty() {
                self.selected_index = if self.selected_index == self.messages.len() - 1 {
                    0
                } else {
                    self.selected_index + 1
                };
            }
        }
        // Enter - select message and branch
        else if kb.matches(data, "tui.select.confirm") {
            drop(kb);
            if let Some(selected) = self.messages.get(self.selected_index) {
                let id = selected.id.clone();
                if let Some(on_select) = &mut self.on_select {
                    on_select(&id);
                }
            }
        }
        // Escape - cancel
        else if kb.matches(data, "tui.select.cancel") {
            drop(kb);
            if let Some(on_cancel) = &mut self.on_cancel {
                on_cancel();
            }
        }
    }
}

/// Component that renders a user message selector for branching.
///
/// Oracle `UserMessageSelectorComponent`.
pub struct UserMessageSelectorComponent {
    spacer_top: Spacer,
    title: Text,
    subtitle: Text,
    spacer_after_subtitle: Spacer,
    top_border: DynamicBorder,
    spacer_before_list: Spacer,
    message_list: UserMessageList,
    spacer_after_list: Spacer,
    bottom_border: DynamicBorder,
    auto_cancel_pending: bool,
    lines: Vec<Line>,
    focused: bool,
}

impl UserMessageSelectorComponent {
    #[must_use]
    pub fn new(
        messages: Vec<UserMessageItem>,
        on_select: OnSelectFn,
        on_cancel: OnCancelFn,
        initial_selected_id: Option<&str>,
    ) -> Self {
        // Auto-cancel if no messages (oracle defers this with setTimeout).
        let auto_cancel_pending = messages.is_empty();

        // Create message list
        let mut message_list = UserMessageList::new(messages, initial_selected_id);
        message_list.on_select = Some(on_select);
        message_list.on_cancel = Some(on_cancel);

        Self {
            spacer_top: Spacer::new(1),
            // Content is (re)styled from the live theme at render time.
            title: Text::new(String::new(), 1, 0, None),
            subtitle: Text::new(String::new(), 1, 0, None),
            spacer_after_subtitle: Spacer::new(1),
            top_border: DynamicBorder::default(),
            spacer_before_list: Spacer::new(1),
            message_list,
            spacer_after_list: Spacer::new(1),
            bottom_border: DynamicBorder::default(),
            auto_cancel_pending,
            lines: Vec::new(),
            focused: false,
        }
    }

    /// Fire the deferred auto-cancel (oracle `setTimeout(onCancel, 100)` for an
    /// empty message list). Returns true if the cancel callback ran.
    pub fn poll_auto_cancel(&mut self) -> bool {
        if !self.auto_cancel_pending {
            return false;
        }
        self.auto_cancel_pending = false;
        if let Some(on_cancel) = &mut self.message_list.on_cancel {
            on_cancel();
            return true;
        }
        false
    }

    /// Oracle `getMessageList`.
    pub fn message_list(&mut self) -> &mut UserMessageList {
        &mut self.message_list
    }
}

impl Component for UserMessageSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        let t = theme();
        self.title.set_text(t.bold("Fork from Message"));
        self.subtitle.set_text(t.fg(
            ThemeColor::Muted,
            "Select a user message to copy the active path up to that point into a new session",
        ));
        let mut lines: Vec<Line> = Vec::new();
        lines.extend_from_slice(self.spacer_top.render(width));
        lines.extend_from_slice(self.title.render(width));
        lines.extend_from_slice(self.subtitle.render(width));
        lines.extend_from_slice(self.spacer_after_subtitle.render(width));
        lines.extend_from_slice(self.top_border.render(width));
        lines.extend_from_slice(self.spacer_before_list.render(width));
        lines.extend_from_slice(self.message_list.render(width));
        lines.extend_from_slice(self.spacer_after_list.render(width));
        lines.extend_from_slice(self.bottom_border.render(width));
        self.lines = lines;
        &self.lines
    }

    fn invalidate(&mut self) {
        self.title.invalidate();
        self.subtitle.invalidate();
        self.top_border.invalidate();
        self.message_list.invalidate();
        self.bottom_border.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        self.message_list.handle_input(data);
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for UserMessageSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    fn msg(id: &str, text: &str) -> UserMessageItem {
        UserMessageItem {
            id: id.to_owned(),
            text: text.to_owned(),
            timestamp: None,
        }
    }

    #[test]
    fn initial_selection_defaults_to_most_recent() {
        let list = UserMessageList::new(vec![msg("a", "1"), msg("b", "2"), msg("c", "3")], None);
        assert_eq!(list.selected_index, 2);
    }

    #[test]
    fn initial_selection_honors_initial_id() {
        let list =
            UserMessageList::new(vec![msg("a", "1"), msg("b", "2"), msg("c", "3")], Some("b"));
        assert_eq!(list.selected_index, 1);
    }

    #[test]
    fn unknown_initial_id_falls_back_to_most_recent() {
        let list = UserMessageList::new(vec![msg("a", "1"), msg("b", "2")], Some("zzz"));
        assert_eq!(list.selected_index, 1);
    }

    #[test]
    fn selection_wraps_both_directions() {
        let mut list = UserMessageList::new(vec![msg("a", "1"), msg("b", "2")], None);
        assert_eq!(list.selected_index, 1);
        list.handle_input("\x1b[B"); // down wraps to top
        assert_eq!(list.selected_index, 0);
        list.handle_input("\x1b[A"); // up wraps to bottom
        assert_eq!(list.selected_index, 1);
    }

    #[test]
    fn confirm_fires_on_select_with_entry_id() {
        let selected: Rc<RefCell<Option<String>>> = Rc::default();
        let slot = Rc::clone(&selected);
        let mut list = UserMessageList::new(vec![msg("a", "1"), msg("b", "2")], Some("a"));
        list.on_select = Some(Box::new(move |id| {
            *slot.borrow_mut() = Some(id.to_owned());
        }));
        list.handle_input("\r");
        assert_eq!(selected.borrow().as_deref(), Some("a"));
    }

    #[test]
    fn escape_fires_on_cancel() {
        let cancelled = Rc::new(RefCell::new(false));
        let slot = Rc::clone(&cancelled);
        let mut list = UserMessageList::new(vec![msg("a", "1")], None);
        list.on_cancel = Some(Box::new(move || {
            *slot.borrow_mut() = true;
        }));
        list.handle_input("\x1b");
        assert!(*cancelled.borrow());
    }

    #[test]
    fn empty_selector_requests_auto_cancel_once() {
        let cancelled = Rc::new(RefCell::new(0u32));
        let slot = Rc::clone(&cancelled);
        let mut selector = UserMessageSelectorComponent::new(
            Vec::new(),
            Box::new(|_| {}),
            Box::new(move || {
                *slot.borrow_mut() += 1;
            }),
            None,
        );
        assert!(selector.poll_auto_cancel());
        assert!(!selector.poll_auto_cancel());
        assert_eq!(*cancelled.borrow(), 1);
    }

    #[test]
    fn non_empty_selector_never_auto_cancels() {
        let mut selector = UserMessageSelectorComponent::new(
            vec![msg("a", "1")],
            Box::new(|_| {}),
            Box::new(|| panic!("must not cancel")),
            None,
        );
        assert!(!selector.poll_auto_cancel());
    }
}
