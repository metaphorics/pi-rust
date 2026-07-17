//! Real terminal-grid coverage for interactive transcript components.
mod common;

use common::vt_terminal::VtTerminal;
use pi_agent::AgentToolResult;
use pi_ai::{Api, AssistantMessage, Content, StopReason, TextContent, Usage};
use pi_coding_agent::modes::interactive::components::{
    assistant_message::AssistantMessageComponent, tool_execution::ToolExecutionComponent,
    user_message::UserMessageComponent,
};
use std::cell::RefCell;
use std::rc::Rc;

struct Shared<C: pi_tui::Component> {
    component: Rc<RefCell<C>>,
    cache: Vec<pi_tui::Line>,
}

impl<C: pi_tui::Component> Shared<C> {
    fn new(component: Rc<RefCell<C>>) -> Self {
        Self {
            component,
            cache: Vec::new(),
        }
    }
}

impl<C: pi_tui::Component> pi_tui::Component for Shared<C> {
    fn render(&mut self, width: u16) -> &[pi_tui::Line] {
        self.cache = self.component.borrow_mut().render(width).to_vec();
        &self.cache
    }
    fn invalidate(&mut self) {
        self.component.borrow_mut().invalidate();
    }
    fn last_render_status(&self) -> pi_tui::RenderStatus {
        self.component.borrow().last_render_status()
    }
}
use pi_tui::Tui;
use serde_json::json;

fn screen_with(component: impl pi_tui::Component + 'static) -> common::vt_terminal::VtHandle {
    let (term, handle) = VtTerminal::new(80, 24);
    let mut tui = Tui::new(term);
    tui.add_child(component);
    tui.do_render();
    handle
}

fn assistant(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![Content::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        api: Api::from("test"),
        provider: "test".into(),
        model: "test".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    }
}

#[test]
fn transcript_components_paint_through_the_tui_frame() {
    let handle = screen_with(UserMessageComponent::new("hello transcript"));
    handle.screen(|screen| {
        assert!(screen.contains("hello transcript"));
        assert_eq!(screen.cells_mutated_outside_sync(), 0);
    });
}

#[test]
fn tool_pending_and_success_transition_in_one_tui() {
    let (term, handle) = VtTerminal::new(80, 24);
    let tool = Rc::new(RefCell::new(ToolExecutionComponent::new(
        "read",
        json!({"path":"a.txt"}),
    )));
    let mut tui = Tui::new(term);
    tui.add_child(Shared::new(Rc::clone(&tool)));
    tui.do_render();
    let pending_style = handle.screen(|s| s.cell(1, 1).expect("pending tool cell").style.bg);
    tool.borrow_mut().end(AgentToolResult::text("done"), false);
    tui.do_render();
    handle.screen(|s| {
        assert_ne!(
            pending_style,
            s.cell(1, 1).expect("complete tool cell").style.bg
        );
        assert!(s.contains("done"));
        assert_eq!(s.cells_mutated_outside_sync(), 0);
    });
}

#[test]
fn assistant_stream_update_and_tool_expansion_change_the_same_grid() {
    let (term, handle) = VtTerminal::new(80, 24);
    let assistant_component = Rc::new(RefCell::new(AssistantMessageComponent::new(Some(
        assistant("first partial"),
    ))));
    let mut tui = Tui::new(term);
    tui.add_child(Shared::new(Rc::clone(&assistant_component)));
    tui.do_render();
    handle.screen(|s| assert!(s.contains("first partial")));
    assistant_component
        .borrow_mut()
        .update_message(assistant("first partial\nsecond partial"));
    tui.do_render();
    handle.screen(|s| {
        assert!(s.contains("second partial"));
        assert_eq!(s.cells_mutated_outside_sync(), 0);
    });

    let (term, handle) = VtTerminal::new(80, 40);
    let tool = Rc::new(RefCell::new(ToolExecutionComponent::new("read", json!({}))));
    tool.borrow_mut().end(
        AgentToolResult::text(
            (0..30)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        false,
    );
    let mut tui = Tui::new(term);
    tui.add_child(Shared::new(Rc::clone(&tool)));
    tui.do_render();
    handle.screen(|s| assert!(!s.contains("line 5")));
    tool.borrow_mut().set_expanded(true);
    tui.do_render();
    handle.screen(|s| {
        assert!(s.contains("line 5"));
        assert_eq!(s.cells_mutated_outside_sync(), 0);
    });
}

#[test]
fn edit_output_paints_added_and_removed_diff_rows() {
    let mut tool = ToolExecutionComponent::new("edit", json!({}));
    tool.end(AgentToolResult::text("-1 old\n+1 new"), false);
    let handle = screen_with(tool);
    handle.screen(|screen| {
        let old = (0..24)
            .flat_map(|row| (0..80).filter_map(move |col| screen.cell(col, row)))
            .find(|cell| cell.ch == '-')
            .expect("removed marker")
            .style
            .fg;
        let new = (0..24)
            .flat_map(|row| (0..80).filter_map(move |col| screen.cell(col, row)))
            .find(|cell| cell.ch == '+')
            .expect("added marker")
            .style
            .fg;
        assert_ne!(old, new);
    });
}

#[test]
fn transcript_components_report_cache_hits_after_second_render() {
    let mut user = UserMessageComponent::new("cached");
    let _ = pi_tui::Component::render(&mut user, 80);
    let _ = pi_tui::Component::render(&mut user, 80);
    assert_eq!(
        pi_tui::Component::last_render_status(&user),
        pi_tui::RenderStatus::Unchanged
    );

    let mut tool = ToolExecutionComponent::new("read", json!({}));
    let _ = pi_tui::Component::render(&mut tool, 80);
    let _ = pi_tui::Component::render(&mut tool, 80);
    assert_eq!(
        pi_tui::Component::last_render_status(&tool),
        pi_tui::RenderStatus::Unchanged
    );
}
