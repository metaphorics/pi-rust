//! Session picker module.
//! Oracle: `/home/alpha/exp/pi-rust/.references/pi/packages/coding-agent/src/cli/session-picker.ts`
//! Citing line anchors and keeping strings byte-verbatim.

use pi_tui::Tui;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::modes::interactive::components::session_selector::{
    SessionSelectorComponent, SessionSelectorOptions, SessionsLoader,
};
use crate::session_manager::SessionManager;

/// Result of session pick dialog.
/// Oracle session-picker.ts lines: 15-54
pub enum SessionPick {
    Selected(PathBuf),
    Cancelled,
    Quit,
}

/// Show TUI session selector and return selected session path or state
/// Oracle lines: 15-54 (selectSession)
pub fn select_session(ui: &mut Tui, cwd: &str, session_dir: Option<PathBuf>) -> SessionPick {
    use std::cell::RefCell;
    use std::rc::Rc;

    let done = Rc::new(RefCell::new(false));
    let result = Rc::new(RefCell::new(SessionPick::Cancelled));

    let done_clone = done.clone();
    let result_clone = result.clone();
    let on_select = Box::new(move |path: &Path| {
        *done_clone.borrow_mut() = true;
        *result_clone.borrow_mut() = SessionPick::Selected(path.to_path_buf());
    });

    let done_clone2 = done.clone();
    let result_clone2 = result.clone();
    let on_cancel = Box::new(move || {
        *done_clone2.borrow_mut() = true;
        *result_clone2.borrow_mut() = SessionPick::Cancelled;
    });

    let done_clone3 = done.clone();
    let result_clone3 = result.clone();
    let on_exit = Box::new(move || {
        *done_clone3.borrow_mut() = true;
        *result_clone3.borrow_mut() = SessionPick::Quit;
    });

    let cwd_str = cwd.to_string();
    let session_dir_clone = session_dir.clone();
    let current_loader: SessionsLoader =
        Box::new(move |_progress: &mut dyn FnMut(usize, usize)| {
            SessionManager::list(&cwd_str, session_dir_clone.clone(), None)
                .map_err(|e| e.to_string())
        });

    let session_dir_clone2 = session_dir.clone();
    let all_loader: SessionsLoader = Box::new(move |_progress: &mut dyn FnMut(usize, usize)| {
        SessionManager::list_all(session_dir_clone2.clone(), None).map_err(|e| e.to_string())
    });

    let options = SessionSelectorOptions {
        show_rename_hint: Some(false),
        ..Default::default()
    };

    let component = SessionSelectorComponent::new(
        current_loader,
        all_loader,
        on_select,
        on_cancel,
        on_exit,
        Box::new(|| {}),
        options,
        None,
    );

    ui.add_child(component);
    ui.set_focus_child(Some(0));
    ui.start_render_loop_hooks();

    while !*done.borrow() {
        ui.poll_terminal();
        ui.do_render();
        std::thread::sleep(Duration::from_millis(10));
    }

    super::startup_ui::clear_startup_tui(ui);
    Rc::try_unwrap(result).ok().unwrap().into_inner()
}
