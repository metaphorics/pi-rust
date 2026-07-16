//! Session selector — port of `modes/interactive/components/session-selector.ts`.
//!
//! Deviations from the TS oracle (documented in the slice report):
//! - Session loaders are synchronous (`SessionManager::list/list_all` are sync);
//!   the loading/progress header states are ported and driven through the
//!   progress callback, but a load completes within one `handle_input` call, so
//!   the async staleness guards (`allLoadSeq`, scope checks) are unnecessary.
//! - Status auto-hide uses a render-time deadline (`Instant`) instead of
//!   `setTimeout`; the message disappears on the first render after expiry.
//! - Internal list→selector wiring uses drained [`SessionListEvent`]s instead of
//!   closures (Rust ownership); external callbacks are unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use pi_tui::component::{Component, Focusable};
use pi_tui::components::{Input, Text};
use pi_tui::keybindings::{KeybindingsManager, get_keybindings};
use pi_tui::line::Line;
use pi_tui::util::{truncate_to_width, visible_width};

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{key_hint, key_text};
use super::session_selector_search::{
    NameFilter, SortMode, filter_and_sort_sessions, has_session_name,
};
use crate::modes::interactive::theme::{ThemeBg, ThemeColor, theme};
use crate::session_manager::SessionInfo;

/// Persists a rename: `(session_path, new_name)`.
pub type RenameSessionFn = Box<dyn FnMut(&Path, &str) -> Result<(), String>>;

/// Oracle `SessionScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionScope {
    Current,
    All,
}

/// Oracle `truncateToWidth(text, maxWidth, ellipsis)` semantics on top of the
/// pi-tui two-arg helper (which never appends an ellipsis).
pub(crate) fn truncate_with_ellipsis(text: &str, max_width: usize, ellipsis: &str) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_owned();
    }
    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        return truncate_to_width(ellipsis, max_width);
    }
    let mut out = truncate_to_width(text, max_width - ellipsis_width);
    out.push_str(ellipsis);
    out
}

fn shorten_path_with_home(path: &str, home: Option<&str>) -> String {
    if path.is_empty() {
        return path.to_owned();
    }
    if let Some(home) = home
        && !home.is_empty()
        && let Some(rest) = path.strip_prefix(home)
    {
        return format!("~{rest}");
    }
    path.to_owned()
}

/// Oracle `shortenPath`.
fn shorten_path(path: &str) -> String {
    let home = dirs::home_dir();
    shorten_path_with_home(path, home.as_deref().map(|p| p.to_str().unwrap_or("")))
}

/// Oracle `formatSessionDate` (pure over epoch millis for testability).
#[must_use]
pub fn format_session_date(modified_ms: i64, now_ms: i64) -> String {
    let diff_ms = now_ms - modified_ms;
    let diff_mins = diff_ms.div_euclid(60_000);
    let diff_hours = diff_ms.div_euclid(3_600_000);
    let diff_days = diff_ms.div_euclid(86_400_000);

    if diff_mins < 1 {
        return "now".to_owned();
    }
    if diff_mins < 60 {
        return format!("{diff_mins}m");
    }
    if diff_hours < 24 {
        return format!("{diff_hours}h");
    }
    if diff_days < 7 {
        return format!("{diff_days}d");
    }
    if diff_days < 30 {
        return format!("{}w", diff_days.div_euclid(7));
    }
    if diff_days < 365 {
        return format!("{}mo", diff_days.div_euclid(30));
    }
    format!("{}y", diff_days.div_euclid(365))
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Oracle `canonicalizePath` (utils/paths.ts): realpath, falling back to the
/// input when the path does not resolve.
fn canonicalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Status message shown in the header hint area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMessage {
    pub is_error: bool,
    pub message: String,
}

/// Oracle `SessionSelectorHeader`.
pub struct SessionSelectorHeader {
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    loading: bool,
    load_progress: Option<(usize, usize)>,
    show_path: bool,
    confirming_delete_path: Option<PathBuf>,
    status_message: Option<StatusMessage>,
    status_deadline: Option<Instant>,
    show_rename_hint: bool,
    lines: Vec<Line>,
}

impl SessionSelectorHeader {
    #[must_use]
    pub fn new(scope: SessionScope, sort_mode: SortMode, name_filter: NameFilter) -> Self {
        Self {
            scope,
            sort_mode,
            name_filter,
            loading: false,
            load_progress: None,
            show_path: false,
            confirming_delete_path: None,
            status_message: None,
            status_deadline: None,
            show_rename_hint: false,
            lines: Vec::new(),
        }
    }

    pub fn set_scope(&mut self, scope: SessionScope) {
        self.scope = scope;
    }

    pub fn set_sort_mode(&mut self, sort_mode: SortMode) {
        self.sort_mode = sort_mode;
    }

    pub fn set_name_filter(&mut self, name_filter: NameFilter) {
        self.name_filter = name_filter;
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        // Progress is scoped to the current load; clear whenever the loading state is set
        self.load_progress = None;
    }

    pub fn set_progress(&mut self, loaded: usize, total: usize) {
        self.load_progress = Some((loaded, total));
    }

    pub fn set_show_path(&mut self, show_path: bool) {
        self.show_path = show_path;
    }

    pub fn set_show_rename_hint(&mut self, show: bool) {
        self.show_rename_hint = show;
    }

    pub fn set_confirming_delete_path(&mut self, path: Option<PathBuf>) {
        self.confirming_delete_path = path;
    }

    pub fn set_status_message(&mut self, msg: Option<StatusMessage>, auto_hide: Option<Duration>) {
        self.status_deadline = None;
        let has_msg = msg.is_some();
        self.status_message = msg;
        if !has_msg {
            return;
        }
        if let Some(auto_hide) = auto_hide {
            self.status_deadline = Some(Instant::now() + auto_hide);
        }
    }

    fn expire_status(&mut self) {
        if let Some(deadline) = self.status_deadline
            && Instant::now() >= deadline
        {
            self.status_message = None;
            self.status_deadline = None;
        }
    }
}

impl Component for SessionSelectorHeader {
    fn render(&mut self, width: u16) -> &[Line] {
        self.expire_status();
        let width = usize::from(width);
        let t = theme();

        let title = match self.scope {
            SessionScope::Current => "Resume Session (Current Folder)",
            SessionScope::All => "Resume Session (All)",
        };
        let left_text = t.bold(title);

        let sort_label = match self.sort_mode {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        };
        let sort_text = format!(
            "{}{}",
            t.fg(ThemeColor::Muted, "Sort: "),
            t.fg(ThemeColor::Accent, sort_label)
        );

        let name_label = match self.name_filter {
            NameFilter::All => "All",
            NameFilter::Named => "Named",
        };
        let name_text = format!(
            "{}{}",
            t.fg(ThemeColor::Muted, "Name: "),
            t.fg(ThemeColor::Accent, name_label)
        );

        let scope_text = if self.loading {
            let progress_text = match self.load_progress {
                Some((loaded, total)) => format!("{loaded}/{total}"),
                None => "...".to_owned(),
            };
            format!(
                "{}{}",
                t.fg(ThemeColor::Muted, "○ Current Folder | "),
                t.fg(ThemeColor::Accent, &format!("Loading {progress_text}"))
            )
        } else if self.scope == SessionScope::Current {
            format!(
                "{}{}",
                t.fg(ThemeColor::Accent, "◉ Current Folder"),
                t.fg(ThemeColor::Muted, " | ○ All")
            )
        } else {
            format!(
                "{}{}",
                t.fg(ThemeColor::Muted, "○ Current Folder | "),
                t.fg(ThemeColor::Accent, "◉ All")
            )
        };

        let right_text = truncate_with_ellipsis(
            &format!("{scope_text}  {name_text}  {sort_text}"),
            width,
            "",
        );
        let available_left = width.saturating_sub(visible_width(&right_text) + 1);
        let left = truncate_with_ellipsis(&left_text, available_left, "");
        let spacing = width.saturating_sub(visible_width(&left) + visible_width(&right_text));

        // Build hint lines - changes based on state (all branches truncate to width)
        let hint_line_1: String;
        let hint_line_2: String;
        if self.confirming_delete_path.is_some() {
            let confirm_hint = format!(
                "Delete session? {} · {}",
                key_hint("tui.select.confirm", "confirm"),
                key_hint("tui.select.cancel", "cancel")
            );
            hint_line_1 = t.fg(
                ThemeColor::Error,
                &truncate_with_ellipsis(&confirm_hint, width, "…"),
            );
            hint_line_2 = String::new();
        } else if let Some(status) = &self.status_message {
            let color = if status.is_error {
                ThemeColor::Error
            } else {
                ThemeColor::Accent
            };
            hint_line_1 = t.fg(color, &truncate_with_ellipsis(&status.message, width, "…"));
            hint_line_2 = String::new();
        } else {
            let path_state = if self.show_path { "(on)" } else { "(off)" };
            let sep = t.fg(ThemeColor::Muted, " · ");
            let hint1 = format!(
                "{}{}{}",
                key_hint("tui.input.tab", "scope"),
                sep,
                t.fg(ThemeColor::Muted, "re:<pattern> regex · \"phrase\" exact")
            );
            let mut hint2_parts = vec![
                key_hint("app.session.toggleSort", "sort"),
                key_hint("app.session.toggleNamedFilter", "named"),
                key_hint("app.session.delete", "delete"),
                key_hint("app.session.togglePath", &format!("path {path_state}")),
            ];
            if self.show_rename_hint {
                hint2_parts.push(key_hint("app.session.rename", "rename"));
            }
            let hint2 = hint2_parts.join(&sep);
            hint_line_1 = truncate_with_ellipsis(&hint1, width, "…");
            hint_line_2 = truncate_with_ellipsis(&hint2, width, "…");
        }

        self.lines = vec![
            Line::from_ansi(&format!("{left}{}{right_text}", " ".repeat(spacing))),
            Line::from_ansi(&hint_line_1),
            Line::from_ansi(&hint_line_2),
        ];
        &self.lines
    }

    fn invalidate(&mut self) {}
}

/// A session tree node for hierarchical display.
#[derive(Debug)]
pub struct SessionTreeNode {
    pub session: SessionInfo,
    pub children: Vec<SessionTreeNode>,
    pub latest_activity: i64,
}

/// Flattened node for display with tree structure info.
#[derive(Debug, Clone)]
pub struct FlatSessionNode {
    pub session: SessionInfo,
    pub depth: usize,
    pub is_last: bool,
    /// For each ancestor level, whether there are more siblings after it.
    pub ancestor_continues: Vec<bool>,
}

/// Build a tree structure from sessions based on `parent_session_path`.
/// Returns root nodes sorted by latest subtree activity (descending).
#[must_use]
pub fn build_session_tree(sessions: &[SessionInfo]) -> Vec<SessionTreeNode> {
    struct Slot {
        session: SessionInfo,
        children: Vec<usize>,
    }

    let canonical: Vec<PathBuf> = sessions
        .iter()
        .map(|s| canonicalize_path(&s.path))
        .collect();

    // Last session wins on duplicate canonical paths (JS Map.set semantics).
    let mut by_path: HashMap<&Path, usize> = HashMap::new();
    for (i, path) in canonical.iter().enumerate() {
        by_path.insert(path.as_path(), i);
    }

    let mut slots: Vec<Slot> = sessions
        .iter()
        .map(|s| Slot {
            session: s.clone(),
            children: Vec::new(),
        })
        .collect();

    let mut root_indices: Vec<usize> = Vec::new();
    for (i, session) in sessions.iter().enumerate() {
        let node_idx = by_path[canonical[i].as_path()];
        let parent_idx = session.parent_session_path.as_deref().and_then(|p| {
            by_path
                .get(canonicalize_path(Path::new(p)).as_path())
                .copied()
        });
        match parent_idx {
            Some(parent) => slots[parent].children.push(node_idx),
            None => root_indices.push(node_idx),
        }
    }

    fn assemble(idx: usize, slots: &[Slot]) -> SessionTreeNode {
        let slot = &slots[idx];
        let children: Vec<SessionTreeNode> = slot
            .children
            .iter()
            .map(|&child| assemble(child, slots))
            .collect();
        let mut latest_activity = slot.session.modified_ms;
        for child in &children {
            latest_activity = latest_activity.max(child.latest_activity);
        }
        SessionTreeNode {
            session: slot.session.clone(),
            children,
            latest_activity,
        }
    }

    fn sort_nodes(nodes: &mut Vec<SessionTreeNode>) {
        nodes.sort_by_key(|node| std::cmp::Reverse(node.latest_activity));
        for node in nodes {
            sort_nodes(&mut node.children);
        }
    }

    let mut roots: Vec<SessionTreeNode> = root_indices
        .into_iter()
        .map(|idx| assemble(idx, &slots))
        .collect();
    sort_nodes(&mut roots);
    roots
}

/// Flatten tree into display list with tree structure metadata.
#[must_use]
pub fn flatten_session_tree(roots: &[SessionTreeNode]) -> Vec<FlatSessionNode> {
    fn walk(
        node: &SessionTreeNode,
        depth: usize,
        ancestor_continues: &[bool],
        is_last: bool,
        result: &mut Vec<FlatSessionNode>,
    ) {
        result.push(FlatSessionNode {
            session: node.session.clone(),
            depth,
            is_last,
            ancestor_continues: ancestor_continues.to_vec(),
        });

        for (i, child) in node.children.iter().enumerate() {
            let child_is_last = i == node.children.len() - 1;
            // Only show continuation line for non-root ancestors
            let continues = if depth > 0 { !is_last } else { false };
            let mut next = ancestor_continues.to_vec();
            next.push(continues);
            walk(child, depth + 1, &next, child_is_last, result);
        }
    }

    let mut result = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        walk(root, 0, &[], i == roots.len() - 1, &mut result);
    }
    result
}

/// Actions the [`SessionList`] requests from its owner. Oracle wires these as
/// closures; Rust ownership makes drained events the equivalent seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionListEvent {
    Select(PathBuf),
    Cancel,
    ToggleScope,
    ToggleSort,
    ToggleNameFilter,
    TogglePath(bool),
    DeleteConfirmationChange(Option<PathBuf>),
    DeleteSession(PathBuf),
    RenameSession(PathBuf),
    Error(String),
}

/// Custom session list component with multi-line items and search.
///
/// Oracle `SessionList`.
pub struct SessionList {
    all_sessions: Vec<SessionInfo>,
    filtered_sessions: Vec<FlatSessionNode>,
    selected_index: usize,
    search_input: Input,
    show_cwd: bool,
    sort_mode: SortMode,
    name_filter: NameFilter,
    keybindings: KeybindingsManager,
    show_path: bool,
    confirming_delete_path: Option<PathBuf>,
    current_session_canonical_path: Option<PathBuf>,
    events: Vec<SessionListEvent>,
    max_visible: usize,
    focused: bool,
    lines: Vec<Line>,
}

impl SessionList {
    #[must_use]
    pub fn new(
        sessions: Vec<SessionInfo>,
        show_cwd: bool,
        sort_mode: SortMode,
        name_filter: NameFilter,
        keybindings: KeybindingsManager,
        current_session_file_path: Option<&Path>,
    ) -> Self {
        let mut list = Self {
            all_sessions: sessions,
            filtered_sessions: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            show_cwd,
            sort_mode,
            name_filter,
            keybindings,
            show_path: false,
            confirming_delete_path: None,
            current_session_canonical_path: current_session_file_path.map(canonicalize_path),
            events: Vec::new(),
            max_visible: 10,
            focused: false,
            lines: Vec::new(),
        };
        list.filter_sessions("");
        list
    }

    /// Drain events produced by the last `handle_input` call.
    pub fn take_events(&mut self) -> Vec<SessionListEvent> {
        std::mem::take(&mut self.events)
    }

    #[must_use]
    pub fn get_selected_session_path(&self) -> Option<&Path> {
        self.filtered_sessions
            .get(self.selected_index)
            .map(|node| node.session.path.as_path())
    }

    pub fn set_sort_mode(&mut self, sort_mode: SortMode) {
        self.sort_mode = sort_mode;
        let query = self.search_input.get_value().to_owned();
        self.filter_sessions(&query);
    }

    pub fn set_name_filter(&mut self, name_filter: NameFilter) {
        self.name_filter = name_filter;
        let query = self.search_input.get_value().to_owned();
        self.filter_sessions(&query);
    }

    pub fn set_sessions(&mut self, sessions: Vec<SessionInfo>, show_cwd: bool) {
        self.all_sessions = sessions;
        self.show_cwd = show_cwd;
        let query = self.search_input.get_value().to_owned();
        self.filter_sessions(&query);
    }

    fn filter_sessions(&mut self, query: &str) {
        let trimmed = query.trim();
        let name_filtered: Vec<SessionInfo> = match self.name_filter {
            NameFilter::All => self.all_sessions.clone(),
            NameFilter::Named => self
                .all_sessions
                .iter()
                .filter(|session| has_session_name(session))
                .cloned()
                .collect(),
        };

        if self.sort_mode == SortMode::Threaded && trimmed.is_empty() {
            // Threaded mode without search: show tree structure
            let roots = build_session_tree(&name_filtered);
            self.filtered_sessions = flatten_session_tree(&roots);
        } else {
            // Other modes or with search: flat list
            let filtered =
                filter_and_sort_sessions(&name_filtered, query, self.sort_mode, NameFilter::All);
            self.filtered_sessions = filtered
                .into_iter()
                .map(|session| FlatSessionNode {
                    session,
                    depth: 0,
                    is_last: true,
                    ancestor_continues: Vec::new(),
                })
                .collect();
        }
        self.selected_index = self
            .selected_index
            .min(self.filtered_sessions.len().saturating_sub(1));
    }

    fn set_confirming_delete_path(&mut self, path: Option<PathBuf>) {
        self.confirming_delete_path.clone_from(&path);
        self.events
            .push(SessionListEvent::DeleteConfirmationChange(path));
    }

    fn start_delete_confirmation_for_selected_session(&mut self) {
        let Some(selected) = self.filtered_sessions.get(self.selected_index) else {
            return;
        };
        let path = selected.session.path.clone();

        // Prevent deleting current session
        if self.is_current_session_path(&path) {
            self.events.push(SessionListEvent::Error(
                "Cannot delete the currently active session".to_owned(),
            ));
            return;
        }

        self.set_confirming_delete_path(Some(path));
    }

    fn is_current_session_path(&self, path: &Path) -> bool {
        let Some(current) = &self.current_session_canonical_path else {
            return false;
        };
        canonicalize_path(path) == *current
    }

    fn build_tree_prefix(node: &FlatSessionNode) -> String {
        if node.depth == 0 {
            return String::new();
        }

        let mut prefix: String = node
            .ancestor_continues
            .iter()
            .map(|&continues| if continues { "│  " } else { "   " })
            .collect();
        prefix.push_str(if node.is_last { "└─ " } else { "├─ " });
        prefix
    }
}

impl Component for SessionList {
    fn render(&mut self, width: u16) -> &[Line] {
        let width_usize = usize::from(width);
        let t = theme();
        let mut lines: Vec<Line> = Vec::new();

        // Render search input
        lines.extend_from_slice(self.search_input.render(width));
        lines.push(Line::empty()); // Blank line after search

        if self.filtered_sessions.is_empty() {
            let empty_message: String = if self.name_filter == NameFilter::Named {
                let toggle_key = key_text("app.session.toggleNamedFilter");
                if self.show_cwd {
                    format!("  No named sessions found. Press {toggle_key} to show all.")
                } else {
                    format!(
                        "  No named sessions in current folder. Press {toggle_key} to show all, or Tab to view all."
                    )
                }
            } else if self.show_cwd {
                // "All" scope - no sessions anywhere that match filter
                "  No sessions found".to_owned()
            } else {
                // "Current folder" scope - hint to try "all"
                "  No sessions in current folder. Press Tab to view all.".to_owned()
            };
            lines.push(Line::from_ansi(&t.fg(
                ThemeColor::Muted,
                &truncate_with_ellipsis(&empty_message, width_usize, "…"),
            )));
            self.lines = lines;
            return &self.lines;
        }

        // Calculate visible range with scrolling
        let len = self.filtered_sessions.len() as isize;
        let max_visible = self.max_visible as isize;
        let start_index = (self.selected_index as isize - max_visible / 2)
            .min(len - max_visible)
            .max(0);
        let end_index = (start_index + max_visible).min(len);
        let now = now_ms();

        // Render visible sessions (one line each with tree structure)
        for i in start_index..end_index {
            let node = &self.filtered_sessions[i as usize];
            let session = &node.session;
            let is_selected = i as usize == self.selected_index;
            let is_confirming_delete =
                Some(session.path.as_path()) == self.confirming_delete_path.as_deref();
            let is_current = self.is_current_session_path(&session.path);

            // Build tree prefix
            let prefix = Self::build_tree_prefix(node);

            // Session display text (name or first message)
            let has_name = session.name.as_deref().is_some_and(|n| !n.is_empty());
            let display_text = session.name.as_deref().unwrap_or(&session.first_message);
            let normalized_message: String = display_text
                .chars()
                .map(|c| {
                    if matches!(c, '\x00'..='\x1f' | '\x7f') {
                        ' '
                    } else {
                        c
                    }
                })
                .collect::<String>()
                .trim()
                .to_owned();

            // Right side: message count and age
            let age = format_session_date(session.modified_ms, now);
            let msg_count = session.message_count.to_string();
            let mut right_part = format!("{msg_count} {age}");
            if self.show_cwd && !session.cwd.is_empty() {
                right_part = format!("{} {right_part}", shorten_path(&session.cwd));
            }
            if self.show_path {
                right_part = format!(
                    "{} {right_part}",
                    shorten_path(&session.path.to_string_lossy())
                );
            }

            // Cursor
            let cursor = if is_selected {
                t.fg(ThemeColor::Accent, "› ")
            } else {
                "  ".to_owned()
            };

            // Calculate available width for message
            let prefix_width = visible_width(&prefix) as isize;
            let right_width = visible_width(&right_part) as isize + 2; // +2 for spacing
            let available_for_msg = width as isize - 2 - prefix_width - right_width; // -2 for cursor

            let truncated_msg = truncate_with_ellipsis(
                &normalized_message,
                available_for_msg.max(10) as usize,
                "…",
            );

            // Style message
            let message_color: Option<ThemeColor> = if is_confirming_delete {
                Some(ThemeColor::Error)
            } else if is_current {
                Some(ThemeColor::Accent)
            } else if has_name {
                Some(ThemeColor::Warning)
            } else {
                None
            };
            let mut styled_msg = match message_color {
                Some(color) => t.fg(color, &truncated_msg),
                None => truncated_msg,
            };
            if is_selected {
                styled_msg = t.bold(&styled_msg);
            }

            // Build line
            let left_part = format!("{cursor}{}{styled_msg}", t.fg(ThemeColor::Dim, &prefix));
            let left_width = visible_width(&left_part) as isize;
            let spacing =
                (width as isize - left_width - visible_width(&right_part) as isize).max(1) as usize;
            let styled_right = t.fg(
                if is_confirming_delete {
                    ThemeColor::Error
                } else {
                    ThemeColor::Dim
                },
                &right_part,
            );

            let mut line = format!("{left_part}{}{styled_right}", " ".repeat(spacing));
            if is_selected {
                line = t.bg(ThemeBg::SelectedBg, &line);
            }
            lines.push(Line::from_ansi(&truncate_with_ellipsis(
                &line,
                width_usize,
                "...",
            )));
        }

        // Add scroll indicator if needed
        if start_index > 0 || end_index < len {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, len);
            let scroll_info = t.fg(
                ThemeColor::Muted,
                &truncate_with_ellipsis(&scroll_text, width_usize, ""),
            );
            lines.push(Line::from_ansi(&scroll_info));
        }

        self.lines = lines;
        &self.lines
    }

    fn invalidate(&mut self) {
        self.search_input.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();

        // Handle delete confirmation state first - intercept all keys
        if self.confirming_delete_path.is_some() {
            if kb.matches(data, "tui.select.confirm") {
                let path_to_delete = self.confirming_delete_path.clone();
                drop(kb);
                self.set_confirming_delete_path(None);
                if let Some(path) = path_to_delete {
                    self.events.push(SessionListEvent::DeleteSession(path));
                }
                return;
            }
            if kb.matches(data, "tui.select.cancel") {
                drop(kb);
                self.set_confirming_delete_path(None);
                return;
            }
            // Ignore all other keys while confirming
            return;
        }

        if kb.matches(data, "tui.input.tab") {
            self.events.push(SessionListEvent::ToggleScope);
            return;
        }

        if kb.matches(data, "app.session.toggleSort") {
            self.events.push(SessionListEvent::ToggleSort);
            return;
        }

        if self
            .keybindings
            .matches(data, "app.session.toggleNamedFilter")
        {
            self.events.push(SessionListEvent::ToggleNameFilter);
            return;
        }

        // Ctrl+P: toggle path display
        if kb.matches(data, "app.session.togglePath") {
            self.show_path = !self.show_path;
            self.events
                .push(SessionListEvent::TogglePath(self.show_path));
            return;
        }

        // Ctrl+D: initiate delete confirmation (useful on terminals that don't
        // distinguish Ctrl+Backspace from Backspace)
        if kb.matches(data, "app.session.delete") {
            drop(kb);
            self.start_delete_confirmation_for_selected_session();
            return;
        }

        // Rename selected session
        if kb.matches(data, "app.session.rename") {
            if let Some(selected) = self.filtered_sessions.get(self.selected_index) {
                self.events.push(SessionListEvent::RenameSession(
                    selected.session.path.clone(),
                ));
            }
            return;
        }

        // Ctrl+Backspace: non-invasive convenience alias for delete
        // Only triggers deletion when the query is empty; otherwise it is forwarded to the input
        if kb.matches(data, "app.session.deleteNoninvasive") {
            drop(kb);
            if !self.search_input.get_value().is_empty() {
                self.search_input.handle_input(data);
                let query = self.search_input.get_value().to_owned();
                self.filter_sessions(&query);
                return;
            }

            self.start_delete_confirmation_for_selected_session();
            return;
        }

        // Up arrow
        if kb.matches(data, "tui.select.up") {
            self.selected_index = self.selected_index.saturating_sub(1);
        }
        // Down arrow
        else if kb.matches(data, "tui.select.down") {
            self.selected_index =
                (self.selected_index + 1).min(self.filtered_sessions.len().saturating_sub(1));
        }
        // Page up - jump up by maxVisible items
        else if kb.matches(data, "tui.select.pageUp") {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible);
        }
        // Page down - jump down by maxVisible items
        else if kb.matches(data, "tui.select.pageDown") {
            self.selected_index = (self.selected_index + self.max_visible)
                .min(self.filtered_sessions.len().saturating_sub(1));
        }
        // Enter
        else if kb.matches(data, "tui.select.confirm") {
            if let Some(selected) = self.filtered_sessions.get(self.selected_index) {
                self.events
                    .push(SessionListEvent::Select(selected.session.path.clone()));
            }
        }
        // Escape - cancel
        else if kb.matches(data, "tui.select.cancel") {
            self.events.push(SessionListEvent::Cancel);
        }
        // Pass everything else to search input
        else {
            drop(kb);
            self.search_input.handle_input(data);
            let query = self.search_input.get_value().to_owned();
            self.filter_sessions(&query);
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

// Focusable implementation - propagate to searchInput for IME cursor positioning
impl Focusable for SessionList {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}

/// How a session file was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMethod {
    Trash,
    Unlink,
}

/// Result of [`delete_session_file`].
#[derive(Debug)]
pub struct DeleteResult {
    pub ok: bool,
    pub method: DeleteMethod,
    pub error: Option<String>,
}

/// Delete a session file, trying the `trash` CLI first, then falling back to
/// permanent removal. Oracle `deleteSessionFile` (synchronous in Rust).
pub fn delete_session_file(session_path: &Path) -> DeleteResult {
    let path_str = session_path.to_string_lossy();
    let mut cmd = Command::new("trash");
    if path_str.starts_with('-') {
        cmd.arg("--");
    }
    cmd.arg(session_path.as_os_str());
    let trash_result = cmd.output();

    let get_trash_error_hint =
        |trash_result: &std::io::Result<std::process::Output>| -> Option<String> {
            let mut parts: Vec<String> = Vec::new();
            match trash_result {
                Err(err) => parts.push(err.to_string()),
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stderr = stderr.trim();
                    if !stderr.is_empty() {
                        parts.push(stderr.lines().next().unwrap_or(stderr).to_owned());
                    }
                }
            }
            if parts.is_empty() {
                return None;
            }
            let joined = format!("trash: {}", parts.join(" · "));
            Some(joined.chars().take(207).collect())
        };

    // If trash reports success, or the file is gone afterwards, treat it as successful
    let trash_succeeded = matches!(&trash_result, Ok(output) if output.status.success());
    if trash_succeeded || !session_path.exists() {
        return DeleteResult {
            ok: true,
            method: DeleteMethod::Trash,
            error: None,
        };
    }

    // Fallback to permanent deletion
    match std::fs::remove_file(session_path) {
        Ok(()) => DeleteResult {
            ok: true,
            method: DeleteMethod::Unlink,
            error: None,
        },
        Err(err) => {
            let unlink_error = err.to_string();
            let error = match get_trash_error_hint(&trash_result) {
                Some(hint) => format!("{unlink_error} ({hint})"),
                None => unlink_error,
            };
            DeleteResult {
                ok: false,
                method: DeleteMethod::Unlink,
                error: Some(error),
            }
        }
    }
}

/// Loads sessions, reporting `(loaded, total)` progress. Synchronous: the Rust
/// `SessionManager::list/list_all` APIs are blocking.
pub type SessionsLoader =
    Box<dyn FnMut(&mut dyn FnMut(usize, usize)) -> Result<Vec<SessionInfo>, String>>;

/// Optional construction knobs. Oracle constructor `options` bag.
#[derive(Default)]
pub struct SessionSelectorOptions {
    /// Called with `(session_path, new_name)` to persist a rename.
    pub rename_session: Option<RenameSessionFn>,
    pub show_rename_hint: Option<bool>,
    pub keybindings: Option<KeybindingsManager>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectorMode {
    List,
    Rename,
}

/// Component that renders a session selector.
///
/// Oracle `SessionSelectorComponent`.
pub struct SessionSelectorComponent {
    can_rename: bool,
    session_list: SessionList,
    header: SessionSelectorHeader,
    scope: SessionScope,
    sort_mode: SortMode,
    name_filter: NameFilter,
    current_sessions: Option<Vec<SessionInfo>>,
    all_sessions: Option<Vec<SessionInfo>>,
    current_sessions_loader: SessionsLoader,
    all_sessions_loader: SessionsLoader,
    request_render: Box<dyn Fn()>,
    rename_session: Option<RenameSessionFn>,
    current_loading: bool,
    all_loading: bool,

    mode: SelectorMode,
    rename_input: Input,
    rename_submitted: std::rc::Rc<std::cell::RefCell<Option<String>>>,
    rename_target_path: Option<PathBuf>,
    rename_title: Text,
    rename_hint: Text,

    on_select: Box<dyn FnMut(&Path)>,
    on_cancel: Box<dyn FnMut()>,
    #[allow(dead_code)] // Mirrors oracle `onExit`; never triggered by SessionList.
    on_exit: Box<dyn FnMut()>,

    top_border: DynamicBorder,
    bottom_border: DynamicBorder,

    focused: bool,
    lines: Vec<Line>,
}

impl SessionSelectorComponent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        current_sessions_loader: SessionsLoader,
        all_sessions_loader: SessionsLoader,
        on_select: Box<dyn FnMut(&Path)>,
        on_cancel: Box<dyn FnMut()>,
        on_exit: Box<dyn FnMut()>,
        request_render: Box<dyn Fn()>,
        options: SessionSelectorOptions,
        current_session_file_path: Option<&Path>,
    ) -> Self {
        let keybindings = options
            .keybindings
            .unwrap_or_else(KeybindingsManager::with_defaults);
        let scope = SessionScope::Current;
        let sort_mode = SortMode::Threaded;
        let name_filter = NameFilter::All;

        let mut header = SessionSelectorHeader::new(scope, sort_mode, name_filter);
        let can_rename = options.rename_session.is_some();
        header.set_show_rename_hint(options.show_rename_hint.unwrap_or(can_rename));

        // Create session list (starts empty, will be populated after load)
        let session_list = SessionList::new(
            Vec::new(),
            false,
            sort_mode,
            name_filter,
            keybindings,
            current_session_file_path,
        );

        let rename_submitted: std::rc::Rc<std::cell::RefCell<Option<String>>> =
            std::rc::Rc::default();
        let mut rename_input = Input::new();
        let slot = std::rc::Rc::clone(&rename_submitted);
        rename_input.on_submit = Some(Box::new(move |value: &str| {
            *slot.borrow_mut() = Some(value.to_owned());
        }));

        // Content is (re)styled from the live theme at render time.
        let rename_title = Text::new(String::new(), 1, 0, None);
        let rename_hint = Text::new(String::new(), 1, 0, None);

        let accent_border =
            || DynamicBorder::new(Some(Box::new(|s: &str| theme().fg(ThemeColor::Accent, s))));

        let mut selector = Self {
            can_rename,
            session_list,
            header,
            scope,
            sort_mode,
            name_filter,
            current_sessions: None,
            all_sessions: None,
            current_sessions_loader,
            all_sessions_loader,
            request_render,
            rename_session: options.rename_session,
            current_loading: false,
            all_loading: false,
            mode: SelectorMode::List,
            rename_input,
            rename_submitted,
            rename_target_path: None,
            rename_title,
            rename_hint,
            on_select,
            on_cancel,
            on_exit,
            top_border: accent_border(),
            bottom_border: accent_border(),
            focused: false,
            lines: Vec::new(),
        };

        // Start loading current sessions immediately
        selector.load_scope(SessionScope::Current, LoadReason::Initial);
        selector
    }

    fn enter_rename_mode(&mut self, session_path: PathBuf, current_name: Option<&str>) {
        self.mode = SelectorMode::Rename;
        self.rename_target_path = Some(session_path);
        self.rename_input.set_value(current_name.unwrap_or(""));
        self.rename_input.set_focused(true);
        (self.request_render)();
    }

    fn exit_rename_mode(&mut self) {
        self.mode = SelectorMode::List;
        self.rename_target_path = None;
        (self.request_render)();
    }

    fn confirm_rename(&mut self, value: &str) {
        let next = value.trim();
        if next.is_empty() {
            return;
        }
        let Some(target) = self.rename_target_path.clone() else {
            self.exit_rename_mode();
            return;
        };
        if self.rename_session.is_none() {
            self.exit_rename_mode();
            return;
        }

        if let Some(rename_session) = &mut self.rename_session {
            // Oracle ignores rename errors (fire-and-forget promise).
            let _ = rename_session(&target, next);
        }
        self.refresh_sessions_after_mutation();
        self.exit_rename_mode();
    }

    fn load_scope(&mut self, scope: SessionScope, reason: LoadReason) {
        let show_cwd = scope == SessionScope::All;

        // Mark loading
        match scope {
            SessionScope::Current => self.current_loading = true,
            SessionScope::All => self.all_loading = true,
        }

        self.header.set_scope(scope);
        self.header.set_loading(true);
        (self.request_render)();

        let result = {
            let header = &mut self.header;
            let request_render = &self.request_render;
            let mut on_progress = |loaded: usize, total: usize| {
                header.set_progress(loaded, total);
                request_render();
            };
            match scope {
                SessionScope::Current => (self.current_sessions_loader)(&mut on_progress),
                SessionScope::All => (self.all_sessions_loader)(&mut on_progress),
            }
        };

        match result {
            Ok(sessions) => {
                match scope {
                    SessionScope::Current => {
                        self.current_sessions = Some(sessions.clone());
                        self.current_loading = false;
                    }
                    SessionScope::All => {
                        self.all_sessions = Some(sessions.clone());
                        self.all_loading = false;
                    }
                }

                self.header.set_loading(false);
                self.session_list.set_sessions(sessions, show_cwd);
                (self.request_render)();
            }
            Err(message) => {
                match scope {
                    SessionScope::Current => self.current_loading = false,
                    SessionScope::All => self.all_loading = false,
                }

                self.header.set_loading(false);
                self.header.set_status_message(
                    Some(StatusMessage {
                        is_error: true,
                        message: format!("Failed to load sessions: {message}"),
                    }),
                    Some(Duration::from_millis(4000)),
                );

                if reason == LoadReason::Initial {
                    self.session_list.set_sessions(Vec::new(), show_cwd);
                }
                (self.request_render)();
            }
        }
    }

    fn toggle_sort_mode(&mut self) {
        // Cycle: threaded -> recent -> relevance -> threaded
        self.sort_mode = match self.sort_mode {
            SortMode::Threaded => SortMode::Recent,
            SortMode::Recent => SortMode::Relevance,
            SortMode::Relevance => SortMode::Threaded,
        };
        self.header.set_sort_mode(self.sort_mode);
        self.session_list.set_sort_mode(self.sort_mode);
        (self.request_render)();
    }

    fn toggle_name_filter(&mut self) {
        self.name_filter = match self.name_filter {
            NameFilter::All => NameFilter::Named,
            NameFilter::Named => NameFilter::All,
        };
        self.header.set_name_filter(self.name_filter);
        self.session_list.set_name_filter(self.name_filter);
        (self.request_render)();
    }

    fn refresh_sessions_after_mutation(&mut self) {
        self.load_scope(self.scope, LoadReason::Refresh);
    }

    fn toggle_scope(&mut self) {
        if self.scope == SessionScope::Current {
            self.scope = SessionScope::All;
            self.header.set_scope(self.scope);

            if let Some(all_sessions) = &self.all_sessions {
                self.header.set_loading(false);
                self.session_list.set_sessions(all_sessions.clone(), true);
                (self.request_render)();
                return;
            }

            if !self.all_loading {
                self.load_scope(SessionScope::All, LoadReason::Toggle);
            }
            return;
        }

        self.scope = SessionScope::Current;
        self.header.set_scope(self.scope);
        self.header.set_loading(self.current_loading);
        self.session_list
            .set_sessions(self.current_sessions.clone().unwrap_or_default(), false);
        (self.request_render)();
    }

    fn handle_delete_session(&mut self, session_path: &Path) {
        let result = delete_session_file(session_path);

        if result.ok {
            if let Some(current_sessions) = &mut self.current_sessions {
                current_sessions.retain(|s| s.path != session_path);
            }
            if let Some(all_sessions) = &mut self.all_sessions {
                all_sessions.retain(|s| s.path != session_path);
            }

            let sessions = match self.scope {
                SessionScope::All => self.all_sessions.clone().unwrap_or_default(),
                SessionScope::Current => self.current_sessions.clone().unwrap_or_default(),
            };
            let show_cwd = self.scope == SessionScope::All;
            self.session_list.set_sessions(sessions, show_cwd);

            let msg = match result.method {
                DeleteMethod::Trash => "Session moved to trash",
                DeleteMethod::Unlink => "Session deleted",
            };
            self.header.set_status_message(
                Some(StatusMessage {
                    is_error: false,
                    message: msg.to_owned(),
                }),
                Some(Duration::from_millis(2000)),
            );
            self.refresh_sessions_after_mutation();
        } else {
            let error_message = result.error.as_deref().unwrap_or("Unknown error");
            self.header.set_status_message(
                Some(StatusMessage {
                    is_error: true,
                    message: format!("Failed to delete: {error_message}"),
                }),
                Some(Duration::from_millis(3000)),
            );
        }

        (self.request_render)();
    }

    fn handle_rename_request(&mut self, session_path: &Path) {
        if self.rename_session.is_none() {
            return;
        }
        if self.scope == SessionScope::Current && self.current_loading {
            return;
        }
        if self.scope == SessionScope::All && self.all_loading {
            return;
        }

        let sessions = match self.scope {
            SessionScope::All => self.all_sessions.as_deref().unwrap_or(&[]),
            SessionScope::Current => self.current_sessions.as_deref().unwrap_or(&[]),
        };
        let current_name = sessions
            .iter()
            .find(|s| s.path == session_path)
            .and_then(|s| s.name.clone());
        self.enter_rename_mode(session_path.to_path_buf(), current_name.as_deref());
    }

    fn clear_status_message(&mut self) {
        self.header.set_status_message(None, None);
    }

    fn process_list_events(&mut self) {
        for event in self.session_list.take_events() {
            match event {
                SessionListEvent::Select(path) => {
                    self.clear_status_message();
                    (self.on_select)(&path);
                }
                SessionListEvent::Cancel => {
                    self.clear_status_message();
                    (self.on_cancel)();
                }
                SessionListEvent::ToggleScope => self.toggle_scope(),
                SessionListEvent::ToggleSort => self.toggle_sort_mode(),
                SessionListEvent::ToggleNameFilter => self.toggle_name_filter(),
                SessionListEvent::TogglePath(show_path) => {
                    self.header.set_show_path(show_path);
                    (self.request_render)();
                }
                SessionListEvent::DeleteConfirmationChange(path) => {
                    self.header.set_confirming_delete_path(path);
                    (self.request_render)();
                }
                SessionListEvent::DeleteSession(path) => self.handle_delete_session(&path),
                SessionListEvent::RenameSession(path) => self.handle_rename_request(&path),
                SessionListEvent::Error(message) => {
                    self.header.set_status_message(
                        Some(StatusMessage {
                            is_error: true,
                            message,
                        }),
                        Some(Duration::from_millis(3000)),
                    );
                    (self.request_render)();
                }
            }
        }
    }

    /// Whether rename is available (oracle `canRename`).
    #[must_use]
    pub fn can_rename(&self) -> bool {
        self.can_rename
    }

    /// Oracle `getSessionList`.
    pub fn session_list(&mut self) -> &mut SessionList {
        &mut self.session_list
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadReason {
    Initial,
    Refresh,
    Toggle,
}

impl Component for SessionSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        // Base layout: Spacer, accent border, Spacer, [header, Spacer], content,
        // Spacer, accent border (oracle `buildBaseLayout`).
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::empty());
        lines.extend_from_slice(self.top_border.render(width));
        lines.push(Line::empty());
        match self.mode {
            SelectorMode::List => {
                lines.extend_from_slice(self.header.render(width));
                lines.push(Line::empty());
                lines.extend_from_slice(self.session_list.render(width));
            }
            SelectorMode::Rename => {
                let t = theme();
                self.rename_title.set_text(t.bold("Rename Session"));
                self.rename_hint.set_text(t.fg(
                    ThemeColor::Muted,
                    &format!(
                        "{} to save · {} to cancel",
                        key_text("tui.select.confirm"),
                        key_text("tui.select.cancel")
                    ),
                ));
                lines.extend_from_slice(self.rename_title.render(width));
                lines.push(Line::empty());
                lines.extend_from_slice(self.rename_input.render(width));
                lines.push(Line::empty());
                lines.extend_from_slice(self.rename_hint.render(width));
            }
        }
        lines.push(Line::empty());
        lines.extend_from_slice(self.bottom_border.render(width));

        self.lines = lines;
        &self.lines
    }

    fn invalidate(&mut self) {
        self.top_border.invalidate();
        self.bottom_border.invalidate();
        self.header.invalidate();
        self.session_list.invalidate();
        self.rename_input.invalidate();
        self.rename_title.invalidate();
        self.rename_hint.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        if self.mode == SelectorMode::Rename {
            let cancelled = get_keybindings().matches(data, "tui.select.cancel");
            if cancelled {
                self.exit_rename_mode();
                return;
            }
            self.rename_input.handle_input(data);
            let submitted = self.rename_submitted.borrow_mut().take();
            if let Some(value) = submitted {
                self.confirm_rename(&value);
            }
            return;
        }

        self.session_list.handle_input(data);
        self.process_list_events();
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

// Focusable implementation - propagate to sessionList for IME cursor positioning
impl Focusable for SessionSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.session_list.set_focused(focused);
        self.rename_input.set_focused(focused);
        if focused && self.mode == SelectorMode::Rename {
            self.rename_input.set_focused(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, path: &str, parent: Option<&str>, modified_ms: i64) -> SessionInfo {
        SessionInfo {
            path: PathBuf::from(path),
            id: id.to_owned(),
            cwd: "/cwd".to_owned(),
            name: None,
            parent_session_path: parent.map(str::to_owned),
            created: "2026-07-16T12:00:00Z".to_owned(),
            modified_ms,
            message_count: 1,
            first_message: format!("first {id}"),
            all_messages_text: format!("messages {id}"),
        }
    }

    #[test]
    fn format_session_date_buckets() {
        let now = 1_700_000_000_000_i64;
        assert_eq!(format_session_date(now - 30_000, now), "now");
        assert_eq!(format_session_date(now - 5 * 60_000, now), "5m");
        assert_eq!(format_session_date(now - 3 * 3_600_000, now), "3h");
        assert_eq!(format_session_date(now - 2 * 86_400_000, now), "2d");
        assert_eq!(format_session_date(now - 10 * 86_400_000, now), "1w");
        assert_eq!(format_session_date(now - 40 * 86_400_000, now), "1mo");
        assert_eq!(format_session_date(now - 400 * 86_400_000, now), "1y");
        // Future dates report "now" (JS floor of negative diff is < 1).
        assert_eq!(format_session_date(now + 60_000, now), "now");
    }

    #[test]
    fn shorten_path_replaces_home_prefix() {
        assert_eq!(
            shorten_path_with_home("/home/u/proj", Some("/home/u")),
            "~/proj"
        );
        assert_eq!(shorten_path_with_home("/other", Some("/home/u")), "/other");
        assert_eq!(shorten_path_with_home("", Some("/home/u")), "");
    }

    #[test]
    fn truncate_with_ellipsis_appends_only_when_truncating() {
        assert_eq!(truncate_with_ellipsis("short", 10, "…"), "short");
        let out = truncate_with_ellipsis("0123456789", 5, "…");
        assert!(out.ends_with('…'), "{out:?}");
        assert_eq!(visible_width(&out), 5);
        assert_eq!(truncate_with_ellipsis("anything", 0, "…"), "");
    }

    #[test]
    fn build_tree_parents_and_activity_sort() {
        // parent (old) with a fresh child outranks a middle-aged root.
        let sessions = vec![
            session("root-old", "/tmp/root-old.jsonl", None, 100),
            session("mid", "/tmp/mid.jsonl", None, 500),
            session(
                "child",
                "/tmp/child.jsonl",
                Some("/tmp/root-old.jsonl"),
                900,
            ),
        ];
        let roots = build_session_tree(&sessions);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].session.id, "root-old");
        assert_eq!(roots[0].latest_activity, 900);
        assert_eq!(roots[0].children.len(), 1);
        assert_eq!(roots[0].children[0].session.id, "child");
        assert_eq!(roots[1].session.id, "mid");
    }

    #[test]
    fn missing_parent_becomes_root() {
        let sessions = vec![session(
            "orphan",
            "/tmp/orphan.jsonl",
            Some("/tmp/gone.jsonl"),
            10,
        )];
        let roots = build_session_tree(&sessions);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].session.id, "orphan");
    }

    #[test]
    fn flatten_tree_marks_depth_and_last_flags() {
        let sessions = vec![
            session("r", "/tmp/r.jsonl", None, 100),
            session("c1", "/tmp/c1.jsonl", Some("/tmp/r.jsonl"), 90),
            session("c2", "/tmp/c2.jsonl", Some("/tmp/r.jsonl"), 80),
            session("g", "/tmp/g.jsonl", Some("/tmp/c1.jsonl"), 70),
        ];
        let roots = build_session_tree(&sessions);
        let flat = flatten_session_tree(&roots);
        let ids: Vec<&str> = flat.iter().map(|n| n.session.id.as_str()).collect();
        assert_eq!(ids, vec!["r", "c1", "g", "c2"]);

        assert_eq!(flat[0].depth, 0);
        assert!(flat[0].is_last);

        // c1 has a following sibling (c2).
        assert_eq!(flat[1].depth, 1);
        assert!(!flat[1].is_last);
        assert_eq!(flat[1].ancestor_continues, vec![false]);

        // grandchild under c1: root-level ancestor never continues; c1 does.
        assert_eq!(flat[2].depth, 2);
        assert!(flat[2].is_last);
        assert_eq!(flat[2].ancestor_continues, vec![false, true]);

        assert_eq!(flat[3].depth, 1);
        assert!(flat[3].is_last);
    }

    #[test]
    fn tree_prefix_shapes() {
        let node = FlatSessionNode {
            session: session("x", "/tmp/x.jsonl", None, 1),
            depth: 0,
            is_last: true,
            ancestor_continues: vec![],
        };
        assert_eq!(SessionList::build_tree_prefix(&node), "");

        let node = FlatSessionNode {
            depth: 2,
            is_last: false,
            ancestor_continues: vec![false, true],
            ..node
        };
        assert_eq!(SessionList::build_tree_prefix(&node), "   │  ├─ ");

        let node = FlatSessionNode {
            depth: 1,
            is_last: true,
            ancestor_continues: vec![false],
            ..node
        };
        assert_eq!(SessionList::build_tree_prefix(&node), "   └─ ");
    }
}
