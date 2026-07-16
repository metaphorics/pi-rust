//! Editor-submit dispatch chain, as a pure function.
//!
//! Port of `interactive-mode.ts` `setupEditorSubmitHandler` (:2634-2820):
//! built-in slash commands → `!`/`!!` bash → compaction gate (extension
//! commands pass through, everything else queues) → streaming gate (steer) →
//! normal prompt. Pure over a [`DispatchContext`] so the ordering is unit
//! testable without a TUI.

/// One built-in slash command (oracle `BUILTIN_SLASH_COMMANDS`,
/// core/slash-commands.ts:19-42). Names, descriptions, and argument hints are
/// byte-verbatim.
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

/// Verbatim `BUILTIN_SLASH_COMMANDS` (`APP_NAME` = "pi").
pub const BUILTIN_SLASH_COMMANDS: &[BuiltinSlashCommand] = &[
    BuiltinSlashCommand {
        name: "settings",
        description: "Open settings menu",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "model",
        description: "Select model (opens selector UI)",
        argument_hint: Some("<provider/model>"),
    },
    BuiltinSlashCommand {
        name: "scoped-models",
        description: "Enable/disable models for Ctrl+P cycling",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "export",
        description: "Export session (HTML default, or specify path: .html/.jsonl)",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "import",
        description: "Import and resume a session from a JSONL file",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "share",
        description: "Share session as a secret GitHub gist",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "copy",
        description: "Copy last agent message to clipboard",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "name",
        description: "Set session display name",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "session",
        description: "Show session info and stats",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "changelog",
        description: "Show changelog entries",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "hotkeys",
        description: "Show all keyboard shortcuts",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "fork",
        description: "Create a new fork from a previous user message",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "clone",
        description: "Duplicate the current session at the current position",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "tree",
        description: "Navigate session tree (switch branches)",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "trust",
        description: "Save project trust decision for future sessions",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "login",
        description: "Configure provider authentication",
        argument_hint: Some("<provider>"),
    },
    BuiltinSlashCommand {
        name: "logout",
        description: "Remove provider authentication",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "new",
        description: "Start a new session",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "compact",
        description: "Manually compact the session context",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "resume",
        description: "Resume a different session",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "reload",
        description: "Reload keybindings, extensions, skills, prompts, themes, and context files",
        argument_hint: None,
    },
    BuiltinSlashCommand {
        name: "quit",
        description: "Quit pi",
        argument_hint: None,
    },
];

/// Session state the dispatch chain consults (oracle reads these live off
/// `this.session`).
#[derive(Clone, Debug, Default)]
pub struct DispatchContext {
    pub is_compacting: bool,
    pub is_streaming: bool,
    pub is_bash_running: bool,
    /// Extension-registered command names (`ExtensionBridge::registered_commands`).
    pub extension_commands: Vec<String>,
}

impl DispatchContext {
    /// Oracle `isExtensionCommand` (:3998-4005).
    #[must_use]
    pub fn is_extension_command(&self, text: &str) -> bool {
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        let command_name = rest.split(' ').next().unwrap_or(rest);
        self.extension_commands.iter().any(|c| c == command_name)
    }
}

/// A recognized built-in slash command with parsed arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuiltinCommand {
    Settings,
    ScopedModels,
    Theme,
    Thinking,
    Images,
    Help,
    /// `/model [search term]`
    Model {
        search: Option<String>,
    },
    /// `/export [path]` — raw text after the command (handler parses).
    Export {
        raw: String,
    },
    /// `/import [path]`
    Import {
        raw: String,
    },
    Share,
    Copy,
    /// `/name [name]`
    Name {
        raw: String,
    },
    Session,
    Changelog,
    Hotkeys,
    Fork,
    Clone,
    Tree,
    Trust,
    /// `/login [provider]`
    Login {
        provider: Option<String>,
    },
    Logout,
    New,
    /// `/compact [instructions]`
    Compact {
        instructions: Option<String>,
    },
    Reload,
    Debug,
    ArminSaysHi,
    DementedElves,
    Resume,
    Quit,
}

/// The action the UI loop must take for one submitted line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchAction {
    /// Empty input after trim: ignore.
    Nothing,
    /// Built-in slash command.
    Builtin(BuiltinCommand),
    /// `!cmd` / `!!cmd`: run bash (`excluded` = `!!`, kept out of context).
    Bash { command: String, excluded: bool },
    /// Bash requested while one is running: warn and restore editor text.
    BashBusy { original_text: String },
    /// Compacting + extension command: prompt immediately (bypasses queue).
    ExtensionDuringCompaction { text: String },
    /// Compacting: queue for after compaction (steer).
    QueueCompaction { text: String },
    /// Streaming: `session.prompt(text, { streamingBehavior: "steer" })`.
    SteerStreaming { text: String },
    /// Idle: normal prompt.
    Prompt { text: String },
}

/// Slash-command argument after `"/name "`: trimmed; empty → `None` (matches
/// the oracle's falsy check on `text.slice(n).trim()`).
fn arg_after(text: &str, prefix: &str) -> Option<String> {
    text.strip_prefix(prefix).map(|rest| rest.trim().to_owned())
}

fn nonempty(arg: Option<String>) -> Option<String> {
    arg.filter(|a| !a.is_empty())
}

fn parse_builtin(text: &str) -> Option<BuiltinCommand> {
    // Order and match forms verbatim from setupEditorSubmitHandler
    // (:2640-2767): exact match or "cmd " prefix where the oracle allows args.
    match text {
        "/settings" => return Some(BuiltinCommand::Settings),
        "/scoped-models" => return Some(BuiltinCommand::ScopedModels),
        "/share" => return Some(BuiltinCommand::Share),
        "/copy" => return Some(BuiltinCommand::Copy),
        "/session" => return Some(BuiltinCommand::Session),
        "/changelog" => return Some(BuiltinCommand::Changelog),
        "/sessions" => return Some(BuiltinCommand::Resume),
        "/theme" | "/themes" => return Some(BuiltinCommand::Theme),
        "/thinking" => return Some(BuiltinCommand::Thinking),
        "/images" => return Some(BuiltinCommand::Images),
        "/help" => return Some(BuiltinCommand::Help),
        "/hotkeys" => return Some(BuiltinCommand::Hotkeys),
        "/fork" => return Some(BuiltinCommand::Fork),
        "/clone" => return Some(BuiltinCommand::Clone),
        "/tree" => return Some(BuiltinCommand::Tree),
        "/trust" => return Some(BuiltinCommand::Trust),
        "/logout" => return Some(BuiltinCommand::Logout),
        "/new" | "/clear" => return Some(BuiltinCommand::New),
        "/reload" => return Some(BuiltinCommand::Reload),
        "/debug" => return Some(BuiltinCommand::Debug),
        "/arminsayshi" => return Some(BuiltinCommand::ArminSaysHi),
        "/dementedelves" => return Some(BuiltinCommand::DementedElves),
        "/resume" => return Some(BuiltinCommand::Resume),
        "/quit" | "/exit" => return Some(BuiltinCommand::Quit),
        "/model" => return Some(BuiltinCommand::Model { search: None }),
        "/export" => {
            return Some(BuiltinCommand::Export {
                raw: text.to_owned(),
            });
        }
        "/import" => {
            return Some(BuiltinCommand::Import {
                raw: text.to_owned(),
            });
        }
        "/name" => {
            return Some(BuiltinCommand::Name {
                raw: text.to_owned(),
            });
        }
        "/login" => return Some(BuiltinCommand::Login { provider: None }),
        "/compact" => {
            return Some(BuiltinCommand::Compact { instructions: None });
        }
        _ => {}
    }
    if text.starts_with("/model ") {
        return Some(BuiltinCommand::Model {
            search: nonempty(arg_after(text, "/model ")),
        });
    }
    if text.starts_with("/export ") {
        return Some(BuiltinCommand::Export {
            raw: text.to_owned(),
        });
    }
    if text.starts_with("/import ") {
        return Some(BuiltinCommand::Import {
            raw: text.to_owned(),
        });
    }
    if text.starts_with("/name ") {
        return Some(BuiltinCommand::Name {
            raw: text.to_owned(),
        });
    }
    if text.starts_with("/login ") {
        return Some(BuiltinCommand::Login {
            provider: nonempty(arg_after(text, "/login ")),
        });
    }
    if text.starts_with("/compact ") {
        return Some(BuiltinCommand::Compact {
            instructions: nonempty(arg_after(text, "/compact ")),
        });
    }
    None
}

/// The dispatch chain (oracle `onSubmit`, :2635-2820).
#[must_use]
pub fn dispatch_input(text: &str, ctx: &DispatchContext) -> DispatchAction {
    let text = text.trim();
    if text.is_empty() {
        return DispatchAction::Nothing;
    }

    // 1. Built-in slash commands (checked before everything else).
    if let Some(command) = parse_builtin(text) {
        return DispatchAction::Builtin(command);
    }

    // 2. Bash: `!` normal, `!!` excluded from context. An empty command
    //    (bare `!`/`!!`) falls through to the gates below (oracle :2773).
    if let Some(stripped) = text.strip_prefix('!') {
        let excluded = text.starts_with("!!");
        let command = stripped.strip_prefix('!').unwrap_or(stripped).trim();
        if !command.is_empty() {
            if ctx.is_bash_running {
                return DispatchAction::BashBusy {
                    original_text: text.to_owned(),
                };
            }
            return DispatchAction::Bash {
                command: command.to_owned(),
                excluded,
            };
        }
    }

    // 3. Compaction gate: extension commands execute immediately, everything
    //    else queues for after compaction.
    if ctx.is_compacting {
        if ctx.is_extension_command(text) {
            return DispatchAction::ExtensionDuringCompaction {
                text: text.to_owned(),
            };
        }
        return DispatchAction::QueueCompaction {
            text: text.to_owned(),
        };
    }

    // 4. Streaming gate: steer.
    if ctx.is_streaming {
        return DispatchAction::SteerStreaming {
            text: text.to_owned(),
        };
    }

    // 5. Normal prompt.
    DispatchAction::Prompt {
        text: text.to_owned(),
    }
}
