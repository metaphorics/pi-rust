pub mod args;
pub mod session_picker;
pub mod startup_ui;

pub use args::{
    AppMode, Args, Diagnostic, DiagnosticType, ExtensionFlag, ListModels, Mode, ThinkingLevel,
    UnknownFlagValue, get_help_text, parse_args, validate_arg_combinations,
};

/// Resolve the AppMode from parsed CLI arguments, process stdin, and stdout TTY status.
pub fn resolve_app_mode(parsed: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> AppMode {
    if parsed.mode == Some(Mode::Rpc) {
        return AppMode::Rpc;
    }
    if parsed.mode == Some(Mode::Json) {
        return AppMode::Json;
    }
    if parsed.print || !stdin_is_tty || !stdout_is_tty {
        return AppMode::Print;
    }
    AppMode::Interactive
}

/// Convert AppMode to Mode, excluding Rpc.
pub fn to_print_output_mode(app_mode: AppMode) -> Mode {
    match app_mode {
        AppMode::Json => Mode::Json,
        _ => Mode::Text,
    }
}

/// Check if the parsed arguments are for a plain metadata command (e.g. help, list-models)
/// that shouldn't take over stdout.
pub fn is_plain_runtime_metadata_command(parsed: &Args) -> bool {
    !parsed.print && parsed.mode.is_none() && (parsed.help || parsed.list_models.is_some())
}
