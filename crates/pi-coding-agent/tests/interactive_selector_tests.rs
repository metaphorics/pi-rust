//! Real terminal-grid coverage for interactive selector components.
mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::vt_terminal::VtTerminal;
use pi_ai::types::ModelThinkingLevel;
use pi_coding_agent::modes::interactive::components::session_selector::{
    SessionSelectorComponent, SessionSelectorOptions, SessionsLoader,
};
use pi_coding_agent::modes::interactive::components::show_images_selector::ShowImagesSelectorComponent;
use pi_coding_agent::modes::interactive::components::theme_selector::ThemeSelectorComponent;
use pi_coding_agent::modes::interactive::components::thinking_selector::ThinkingSelectorComponent;

use pi_coding_agent::modes::interactive::components::user_message_selector::{
    UserMessageItem, UserMessageSelectorComponent,
};
use pi_coding_agent::session_manager::SessionInfo;
use std::path::{Path, PathBuf};

use pi_tui::Tui;

fn mount(component: impl pi_tui::Component + 'static) -> (Tui, common::vt_terminal::VtHandle) {
    let (term, handle) = VtTerminal::new(80, 24);
    let mut tui = Tui::new(term);
    tui.add_child(component);
    tui.set_focus_child(Some(0));
    tui.do_render();
    (tui, handle)
}
fn send_input(tui: &mut Tui, handle: &common::vt_terminal::VtHandle, input: &str) {
    handle.send_input(input);
    tui.poll_terminal();
    tui.handle_input(input.to_owned());
}

#[test]
fn image_picker_moves_selects_and_cancels_on_the_real_grid() {
    let selected = Rc::new(RefCell::new(None));
    let cancelled = Rc::new(RefCell::new(0_usize));
    let selected_slot = Rc::clone(&selected);
    let cancelled_slot = Rc::clone(&cancelled);
    let component = ShowImagesSelectorComponent::new(
        true,
        Box::new(move |value| *selected_slot.borrow_mut() = Some(value)),
        Box::new(move || *cancelled_slot.borrow_mut() += 1),
    );
    let (mut tui, handle) = mount(component);

    handle.screen(|screen| {
        assert!(screen.contains("Yes"));
        assert!(screen.contains("Show images inline in terminal"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "\x1b[B");
    tui.do_render();
    handle.screen(|screen| {
        assert!(screen.contains("No"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "\r");
    assert_eq!(*selected.borrow(), Some(false));
    send_input(&mut tui, &handle, "\x1b");
    assert_eq!(*cancelled.borrow(), 1);
}

#[test]
fn thinking_picker_renders_descriptions_and_selects_the_highlight() {
    let selected = Rc::new(RefCell::new(None));
    let selected_slot = Rc::clone(&selected);
    let component = ThinkingSelectorComponent::new(
        ModelThinkingLevel::Low,
        &[ModelThinkingLevel::Low, ModelThinkingLevel::High],
        Box::new(move |value| *selected_slot.borrow_mut() = Some(value)),
        Box::new(|| {}),
    );
    let (mut tui, handle) = mount(component);
    handle.screen(|screen| {
        assert!(screen.contains("low"));
        assert!(screen.contains("high"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "\x1b[B");
    tui.do_render();
    send_input(&mut tui, &handle, "\r");
    assert_eq!(*selected.borrow(), Some(ModelThinkingLevel::High));
    handle.screen(|screen| assert_eq!(screen.cells_mutated_outside_sync(), 0));
}

#[test]
fn user_message_picker_selects_and_cancels_through_tui_input() {
    let selected = Rc::new(RefCell::new(None));
    let cancelled = Rc::new(RefCell::new(0_usize));
    let selected_slot = Rc::clone(&selected);
    let cancelled_slot = Rc::clone(&cancelled);
    let component = UserMessageSelectorComponent::new(
        vec![
            UserMessageItem {
                id: "one".into(),
                text: "first fork point".into(),
                timestamp: None,
            },
            UserMessageItem {
                id: "two".into(),
                text: "second fork point".into(),
                timestamp: None,
            },
        ],
        Box::new(move |id| *selected_slot.borrow_mut() = Some(id.to_owned())),
        Box::new(move || *cancelled_slot.borrow_mut() += 1),
        Some("one"),
    );
    let (mut tui, handle) = mount(component);
    handle.screen(|screen| {
        assert!(screen.contains("Fork from Message"));
        assert!(screen.contains("first fork point"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "\x1b[B");
    tui.do_render();
    send_input(&mut tui, &handle, "\r");
    assert_eq!(selected.borrow().as_deref(), Some("two"));
    send_input(&mut tui, &handle, "\x1b");
    assert_eq!(*cancelled.borrow(), 1);
}

#[test]
fn session_picker_filters_and_selects_through_the_real_grid() {
    let sessions = vec![
        SessionInfo {
            path: PathBuf::from("/tmp/first.jsonl"),
            id: "first".into(),
            cwd: "/tmp".into(),
            name: Some("first session".into()),
            parent_session_path: None,
            created: "2026-01-01T00:00:00Z".into(),
            modified_ms: 1_700_000_000_000,
            message_count: 1,
            first_message: "alpha request".into(),
            all_messages_text: "alpha request".into(),
        },
        SessionInfo {
            path: PathBuf::from("/tmp/second.jsonl"),
            id: "second".into(),
            cwd: "/tmp".into(),
            name: Some("needle session".into()),
            parent_session_path: None,
            created: "2026-01-02T00:00:00Z".into(),
            modified_ms: 1_700_000_100_000,
            message_count: 2,
            first_message: "needle request".into(),
            all_messages_text: "needle request".into(),
        },
    ];
    let current: SessionsLoader = Box::new({
        let sessions = sessions.clone();
        move |_| Ok(sessions.clone())
    });
    let all: SessionsLoader = Box::new(move |_| Ok(sessions.clone()));
    let selected = Rc::new(RefCell::new(None));
    let selected_slot = Rc::clone(&selected);
    let component = SessionSelectorComponent::new(
        current,
        all,
        Box::new(move |path: &Path| *selected_slot.borrow_mut() = Some(path.to_path_buf())),
        Box::new(|| {}),
        Box::new(|| {}),
        Box::new(|| {}),
        SessionSelectorOptions::default(),
        None,
    );
    let (mut tui, handle) = mount(component);
    handle.screen(|screen| {
        assert!(screen.contains("first session"));
        assert!(screen.contains("needle session"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "needle");
    tui.do_render();
    handle.screen(|screen| {
        assert!(screen.contains("needle session"));
        assert!(!screen.contains("first session"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "\r");
    assert_eq!(
        selected.borrow().as_deref(),
        Some(Path::new("/tmp/second.jsonl"))
    );
}

#[test]
fn theme_picker_previews_highlights_selects_and_cancels_on_the_grid() {
    let selected = Rc::new(RefCell::new(None));
    let previewed = Rc::new(RefCell::new(Vec::new()));
    let cancelled = Rc::new(RefCell::new(0_usize));
    let selected_slot = Rc::clone(&selected);
    let previewed_slot = Rc::clone(&previewed);
    let cancelled_slot = Rc::clone(&cancelled);
    let component = ThemeSelectorComponent::new(
        "dark",
        Box::new(move |theme| *selected_slot.borrow_mut() = Some(theme)),
        Box::new(move || *cancelled_slot.borrow_mut() += 1),
        Box::new(move |theme| previewed_slot.borrow_mut().push(theme)),
    );
    let (mut tui, handle) = mount(component);
    handle.screen(|screen| {
        assert!(screen.contains("dark"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
    send_input(&mut tui, &handle, "\x1b[B");
    tui.do_render();
    assert!(!previewed.borrow().is_empty());
    send_input(&mut tui, &handle, "\r");
    assert!(selected.borrow().is_some());
    send_input(&mut tui, &handle, "\x1b");
    assert_eq!(*cancelled.borrow(), 1);
}
