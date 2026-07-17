//! Real Tui → VT-grid coverage for dialog, footer, editor, and status widgets.
mod common;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use common::vt_terminal::{VtHandle, VtTerminal};
use pi_coding_agent::modes::interactive::components::custom_editor::CustomEditor;
use pi_coding_agent::modes::interactive::components::footer::{
    FooterComponent, FooterData, FooterStats, format_cwd_for_footer, format_tokens,
};
use pi_coding_agent::modes::interactive::components::login_dialog::LoginDialogComponent;
use pi_coding_agent::modes::interactive::components::status_indicator::StatusIndicator;
use pi_coding_agent::modes::interactive::components::trust_selector::{
    ProjectTrustStoreEntry, TrustSelectorComponent, TrustSelectorOptions,
};
use pi_tui::components::{Editor, EditorTheme, EditorTui};
use pi_tui::{Container, Tui};
use pi_vt::Color;

fn mount(component: impl pi_tui::Component + 'static, rows: u16) -> (Tui, VtHandle) {
    let (terminal, handle) = VtTerminal::new(100, rows);
    let mut tui = Tui::new(terminal);
    tui.add_child(component);
    tui.set_focus_child(Some(0));
    tui.do_render();
    (tui, handle)
}

fn send_input(tui: &mut Tui, handle: &VtHandle, input: &str) {
    handle.send_input(input);
    tui.poll_terminal();
    tui.handle_input(input.to_owned());
}

#[test]
fn footer_renders_three_ordered_lines_from_fixed_data() {
    let component = FooterComponent::new(FooterData {
        cwd: Box::new(|| "/home/test/work".to_owned()),
        git_branch: Box::new(|| Some("feature/dialogs".to_owned())),
        session_name: Box::new(|| Some("acceptance".to_owned())),
        stats: Box::new(|| FooterStats {
            input: 1_250,
            output: 42,
            cache_read: 500,
            cache_write: 250,
            cost: 0.125,
            context_percent: Some(73.2),
            context_window: 200_000,
            model: Some("model-x".to_owned()),
            provider: Some("provider-a".to_owned()),
            reasoning: true,
            thinking_level: Some("high".to_owned()),
            using_subscription: false,
            experimental: true,
        }),
        extension_statuses: Box::new(|| {
            vec![
                ("z-last".to_owned(), "Z ready".to_owned()),
                ("a-first".to_owned(), "A ready".to_owned()),
            ]
        }),
        available_provider_count: Box::new(|| 2),
    });
    let (_tui, handle) = mount(component, 12);
    handle.screen(|screen| {
        let pwd = screen.find_row("feature/dialogs").expect("pwd row");
        let stats = screen.find_row("↑1.3k").expect("stats row");
        let statuses = screen.find_row("A ready Z ready").expect("status row");
        assert_eq!(stats, pwd + 1);
        assert_eq!(statuses, stats + 1);
        assert!(screen.row_text(stats).contains("CH25.0%"));
        assert!(screen.row_text(stats).contains("model-x • high"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
}

struct TestEditorTui;
impl EditorTui for TestEditorTui {
    fn request_render(&self) {}
    fn terminal_rows(&self) -> u16 {
        24
    }
}
static EDITOR_TUI: TestEditorTui = TestEditorTui;

#[test]
fn custom_editor_interceptor_consumes_bound_key_and_forwards_other_input() {
    let consumed = Rc::new(Cell::new(0));
    let consumed_slot = Rc::clone(&consumed);
    let editor = Editor::new(&EDITOR_TUI, EditorTheme);
    let mut component = CustomEditor::new(editor, move |data| {
        if data == "z" {
            consumed_slot.set(consumed_slot.get() + 1);
            true
        } else {
            false
        }
    });
    component.set_bash_mode(true);
    let (mut tui, handle) = mount(component, 8);
    send_input(&mut tui, &handle, "z");
    send_input(&mut tui, &handle, "q");
    tui.do_render();
    assert_eq!(consumed.get(), 1);
    handle.screen(|screen| {
        assert!(screen.contains("q"));
        assert!(!screen.contains("z"));
        assert_ne!(
            screen.cell(0, 0).expect("top editor border").style.fg,
            Color::Default
        );
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
}

#[test]
fn status_indicators_render_working_and_retry_messages() {
    let mut statuses = Container::new();
    statuses.add_child(StatusIndicator::working("Thinking..."));
    statuses.add_child(StatusIndicator::retry(2, 4, 7));
    let (_tui, handle) = mount(statuses, 10);
    handle.screen(|screen| {
        assert!(screen.contains("Thinking..."));
        assert!(screen.contains("Retrying (2/4) in 7s..."));
        assert!(screen.contains("to cancel"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
}

#[test]
fn trust_dialog_renders_verbatim_state_and_fires_enter_and_escape_callbacks() {
    let selected = Rc::new(RefCell::new(None));
    let cancelled = Rc::new(Cell::new(0));
    let selected_slot = Rc::clone(&selected);
    let cancelled_slot = Rc::clone(&cancelled);
    let component = TrustSelectorComponent::new(TrustSelectorOptions {
        cwd: "/tmp/pi-dialog-project".to_owned(),
        saved_decision: Some(ProjectTrustStoreEntry {
            path: "/tmp/pi-dialog-project".to_owned(),
            decision: true,
        }),
        project_trusted: true,
        on_select: Box::new(move |choice| *selected_slot.borrow_mut() = Some(choice)),
        on_cancel: Box::new(move || cancelled_slot.set(cancelled_slot.get() + 1)),
    });
    let (mut tui, handle) = mount(component, 24);
    handle.screen(|screen| {
        assert!(screen.contains("Project trust"));
        assert!(screen.contains("Saved decision: trusted (/tmp/pi-dialog-project)"));
        assert!(screen.contains("Current session: trusted"));
        assert!(screen.contains("Trust"));
        assert!(screen.contains("Do not trust"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "\r");
    assert!(
        selected
            .borrow()
            .as_ref()
            .is_some_and(|choice| choice.trusted)
    );
    send_input(&mut tui, &handle, "\x1b");
    assert_eq!(cancelled.get(), 1);
    tui.do_render();
    handle.screen(|screen| assert_eq!(screen.cells_mutated_outside_sync(), 0));
}

#[test]
fn login_dialog_renders_prompt_masks_input_and_fires_submit_and_cancel() {
    let completed = Rc::new(RefCell::new(Vec::new()));
    let submitted = Rc::new(RefCell::new(Vec::new()));
    let completed_slot = Rc::clone(&completed);
    let submitted_slot = Rc::clone(&submitted);
    let mut component = LoginDialogComponent::new(
        "aws-bedrock",
        move |success, message| completed_slot.borrow_mut().push((success, message)),
        Some("AWS Bedrock"),
        None,
    );
    component.set_masked(true);
    component.show_prompt("Enter your AWS secret access key:", Some("secret"));
    component.on_submit = Some(Box::new(move |value| {
        submitted_slot.borrow_mut().push(value);
    }));
    let (mut tui, handle) = mount(component, 16);
    handle.screen(|screen| {
        assert!(screen.contains("Login to AWS Bedrock"));
        assert!(screen.contains("Enter your AWS secret access key:"));
        assert!(screen.contains("e.g., secret"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "hunter2");
    tui.do_render();
    handle.screen(|screen| {
        assert!(screen.contains("•••••••"));
        assert!(!screen.contains("hunter2"));
    });
    send_input(&mut tui, &handle, "\r");
    assert_eq!(submitted.borrow().as_slice(), ["hunter2"]);
    send_input(&mut tui, &handle, "\x1b");
    assert_eq!(
        completed.borrow().as_slice(),
        [(false, Some("Login cancelled".to_owned()))]
    );
}

#[test]
fn footer_pure_format_helpers_match_oracle_boundaries() {
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(1_000), "1.0k");
    assert_eq!(format_tokens(1_250), "1.3k");
    assert_eq!(format_tokens(9_999), "10.0k");
    assert_eq!(format_tokens(10_000), "10k");
    assert_eq!(format_tokens(1_000_000), "1.0M");
    assert_eq!(
        format_cwd_for_footer("/home/a/work", Some("/home/a")),
        "~/work"
    );
    assert_eq!(
        format_cwd_for_footer("/srv/work", Some("/home/a")),
        "/srv/work"
    );
}
