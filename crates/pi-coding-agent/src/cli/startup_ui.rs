//! Startup UI module.
//! Oracle: `/home/alpha/exp/pi-rust/.references/pi/packages/coding-agent/src/cli/startup-ui.ts`
//! Citing line anchors and keeping strings byte-verbatim.

use parking_lot::Mutex;
use pi_tui::Tui;
use pi_tui::keybindings::set_keybindings;
use pi_tui::terminal::Terminal;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{APP_NAME, CONFIG_DIR_NAME, PACKAGE_NAME, env_agent_dir_key};
use crate::modes::interactive::app_keybindings::create_app_keybindings;
use crate::modes::interactive::components::extension_input::ExtensionInput;
use crate::modes::interactive::components::extension_selector::ExtensionSelector;
use crate::modes::interactive::components::first_time_setup::{
    FirstTimeSetup, FirstTimeSetupOptions, FirstTimeSetupResult,
};
use crate::modes::interactive::theme::{
    TerminalTheme, detect_terminal_background_from_env, init_theme, resolve_theme_setting,
    set_theme,
};
use crate::settings_manager::SettingsManager;

/// First-time setup runs when all of these hold:
/// - this is the official Pi distribution (not a fork/rebrand)
/// - experimental features are enabled (PI_EXPERIMENTAL=1)
/// - the default agent directory is used (no custom agent dir override)
/// - setup was not completed before (settings.json does not exist)
///
/// Oracle lines: 108-132.
pub fn should_run_first_time_setup(settings_path: &Path) -> bool {
    let official = PACKAGE_NAME == "@earendil-works/pi-coding-agent"
        && APP_NAME == "pi"
        && CONFIG_DIR_NAME == ".pi";
    should_run_first_time_setup_with(
        official,
        std::env::var("PI_EXPERIMENTAL").ok().as_deref() == Some("1"),
        std::env::var_os(env_agent_dir_key()).is_some(),
        settings_path.exists(),
    )
}

fn should_run_first_time_setup_with(
    official: bool,
    experimental: bool,
    custom_agent_dir: bool,
    settings_exist: bool,
) -> bool {
    official && experimental && !custom_agent_dir && !settings_exist
}

/// Create a startup Tui using the settings manager to resolve theme settings.
/// Oracle lines: 77-85
pub fn create_startup_tui(
    agent_dir: &Path,
    settings_manager: &Arc<Mutex<SettingsManager>>,
    terminal: impl Terminal + 'static,
) -> Tui {
    let settings = settings_manager.lock();
    let theme_setting = settings.get_theme();
    let detected = detect_terminal_background_from_env(None);
    let resolved = resolve_theme_setting(theme_setting, detected.theme);

    // Oracle lines 66-74: init_theme(None, ..) falls back to terminal default.
    init_theme(resolved.as_deref(), false);

    set_keybindings(create_app_keybindings(agent_dir));
    Tui::new(terminal)
}

/// Clear the startup Tui by clearing children and forcing a render.
/// Oracle lines: 102-106 (clearStartupTui)
pub fn clear_startup_tui(ui: &mut Tui) {
    ui.root_mut().clear();
    ui.do_render();
    std::thread::sleep(Duration::from_millis(25));
}

/// Show a selector list to choose from options.
/// Oracle lines: 134-163 (showStartupSelector)
pub fn show_startup_selector(ui: &mut Tui, title: &str, options: &[String]) -> Option<usize> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let done = Rc::new(RefCell::new(false));
    let result = Rc::new(RefCell::new(None));

    let options_clone = options.to_vec();
    let done_clone = done.clone();
    let result_clone = result.clone();

    let on_submit = Box::new(move |value: String| {
        *done_clone.borrow_mut() = true;
        if let Some(pos) = options_clone.iter().position(|o| o == &value) {
            *result_clone.borrow_mut() = Some(pos);
        }
    });

    let done_clone2 = done.clone();
    let on_cancel = Box::new(move || {
        *done_clone2.borrow_mut() = true;
    });

    let mut selector = ExtensionSelector::new(title, options.to_vec());
    selector.on_submit = Some(on_submit);
    selector.on_cancel = Some(on_cancel);

    ui.add_child(selector);
    ui.set_focus_child(Some(0));
    ui.start_render_loop_hooks();

    while !*done.borrow() {
        ui.poll_terminal();
        ui.do_render();
        std::thread::sleep(Duration::from_millis(10));
    }

    clear_startup_tui(ui);
    *result.borrow()
}

/// Show an input prompt for text entry.
/// Oracle lines: 207-239 (showStartupInput)
/// Note: Oracle showStartupInput takes `placeholder`, but Rust `ExtensionInput` does not support it.
/// Therefore, the `_placeholder` parameter is unused here.
pub fn show_startup_input(ui: &mut Tui, title: &str, _placeholder: &str) -> Option<String> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let done = Rc::new(RefCell::new(false));
    let result = Rc::new(RefCell::new(None));

    let done_clone = done.clone();
    let result_clone = result.clone();

    let on_submit = Box::new(move |value: String| {
        *done_clone.borrow_mut() = true;
        *result_clone.borrow_mut() = Some(value);
    });

    let done_clone2 = done.clone();
    let on_cancel = Box::new(move || {
        *done_clone2.borrow_mut() = true;
    });

    let mut input = ExtensionInput::new(title);
    input.on_submit = Some(on_submit);
    input.on_cancel = Some(on_cancel);

    ui.add_child(input);
    ui.set_focus_child(Some(0));
    ui.start_render_loop_hooks();

    while !*done.borrow() {
        ui.poll_terminal();
        ui.do_render();
        std::thread::sleep(Duration::from_millis(10));
    }

    clear_startup_tui(ui);
    result.borrow().clone()
}

/// Show first time setup dialog for theme selection.
/// Oracle lines: 166-205 (showFirstTimeSetup)
pub fn show_first_time_setup(
    ui: &mut Tui,
    settings_manager: &Arc<Mutex<SettingsManager>>,
) -> Option<FirstTimeSetupResult> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let done = Rc::new(RefCell::new(false));
    let result = Rc::new(RefCell::new(None));

    let detected = detect_terminal_background_from_env(None);

    let done_clone = done.clone();
    let result_clone = result.clone();
    let settings_manager_clone = settings_manager.clone();

    let on_submit = Box::new(move |res: FirstTimeSetupResult| {
        *done_clone.borrow_mut() = true;
        *result_clone.borrow_mut() = Some(res);
        settings_manager_clone.lock().set_theme(res.theme.as_str());
    });

    let done_clone2 = done.clone();
    let on_cancel = Box::new(move || {
        *done_clone2.borrow_mut() = true;
    });

    let on_theme_preview = Box::new(|t: TerminalTheme| {
        let _ = set_theme(t.as_str(), false);
    });

    let options = FirstTimeSetupOptions {
        detected_theme: detected.theme,
        on_theme_preview,
        on_submit,
        on_cancel,
    };

    let component = FirstTimeSetup::new(options);
    ui.add_child(component);
    ui.set_focus_child(Some(0));
    ui.start_render_loop_hooks();

    while !*done.borrow() {
        ui.poll_terminal();
        ui.do_render();
        std::thread::sleep(Duration::from_millis(10));
    }

    clear_startup_tui(ui);
    *result.borrow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_time_setup_gate_requires_every_condition() {
        assert!(should_run_first_time_setup_with(true, true, false, false));
        assert!(!should_run_first_time_setup_with(false, true, false, false));
        assert!(!should_run_first_time_setup_with(true, false, false, false));
        assert!(!should_run_first_time_setup_with(true, true, true, false));
        assert!(!should_run_first_time_setup_with(true, true, false, true));
    }
}
