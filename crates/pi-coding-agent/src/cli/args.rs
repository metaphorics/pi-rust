#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

fn parse_thinking_level(s: &str) -> Option<ThinkingLevel> {
    match s {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListModels {
    All,
    Search(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownFlagValue {
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub r#type: DiagnosticType,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticType {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    pub thinking: Option<ThinkingLevel>,
    pub r#continue: bool,
    pub resume: bool,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub extensions: Option<Vec<String>>,
    pub no_extensions: bool,
    pub skills: Option<Vec<String>>,
    pub no_skills: bool,
    pub prompt_templates: Option<Vec<String>>,
    pub no_prompt_templates: bool,
    pub themes: Option<Vec<String>>,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub list_models: Option<ListModels>,
    pub offline: bool,
    pub verbose: bool,
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    pub unknown_flags: Vec<(String, UnknownFlagValue)>,
    pub diagnostics: Vec<Diagnostic>,
    pub print: bool,
    pub export: Option<String>,
}

impl Args {
    pub fn get_unknown_flag(&self, name: &str) -> Option<&UnknownFlagValue> {
        self.unknown_flags
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }
}

fn insert_unknown_flag(
    unknown_flags: &mut Vec<(String, UnknownFlagValue)>,
    key: String,
    val: UnknownFlagValue,
) {
    if let Some(pos) = unknown_flags.iter().position(|(k, _)| k == &key) {
        unknown_flags[pos].1 = val;
    } else {
        unknown_flags.push((key, val));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionFlag {
    pub name: String,
    pub r#type: String,
    pub description: Option<String>,
    pub extension_path: String,
}

pub fn parse_args(args: &[String]) -> Args {
    let mut result = Args::default();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        if arg == "--help" || arg == "-h" {
            result.help = true;
        } else if arg == "--version" || arg == "-v" {
            result.version = true;
        } else if arg == "--mode" && i + 1 < args.len() {
            i += 1;
            let mode_str = &args[i];
            if mode_str == "text" {
                result.mode = Some(Mode::Text);
            } else if mode_str == "json" {
                result.mode = Some(Mode::Json);
            } else if mode_str == "rpc" {
                result.mode = Some(Mode::Rpc);
            }
        } else if arg == "--continue" || arg == "-c" {
            result.r#continue = true;
        } else if arg == "--resume" || arg == "-r" {
            result.resume = true;
        } else if arg == "--provider" && i + 1 < args.len() {
            i += 1;
            result.provider = Some(args[i].clone());
        } else if arg == "--model" && i + 1 < args.len() {
            i += 1;
            result.model = Some(args[i].clone());
        } else if arg == "--api-key" && i + 1 < args.len() {
            i += 1;
            result.api_key = Some(args[i].clone());
        } else if arg == "--system-prompt" && i + 1 < args.len() {
            i += 1;
            result.system_prompt = Some(args[i].clone());
        } else if arg == "--append-system-prompt" && i + 1 < args.len() {
            i += 1;
            let mut list = result.append_system_prompt.take().unwrap_or_default();
            list.push(args[i].clone());
            result.append_system_prompt = Some(list);
        } else if arg == "--name" || arg == "-n" {
            if i + 1 < args.len() {
                i += 1;
                result.name = Some(args[i].clone());
            } else {
                result.diagnostics.push(Diagnostic {
                    r#type: DiagnosticType::Error,
                    message: "--name requires a value".to_string(),
                });
            }
        } else if arg == "--no-session" {
            result.no_session = true;
        } else if arg == "--session" && i + 1 < args.len() {
            i += 1;
            result.session = Some(args[i].clone());
        } else if arg == "--session-id" && i + 1 < args.len() {
            i += 1;
            result.session_id = Some(args[i].clone());
        } else if arg == "--fork" && i + 1 < args.len() {
            i += 1;
            result.fork = Some(args[i].clone());
        } else if arg == "--session-dir" && i + 1 < args.len() {
            i += 1;
            result.session_dir = Some(args[i].clone());
        } else if arg == "--models" && i + 1 < args.len() {
            i += 1;
            let val = &args[i];
            let models_vec: Vec<String> = val.split(',').map(|s| s.trim().to_string()).collect();
            result.models = Some(models_vec);
        } else if arg == "--no-tools" || arg == "-nt" {
            result.no_tools = true;
        } else if arg == "--no-builtin-tools" || arg == "-nbt" {
            result.no_builtin_tools = true;
        } else if (arg == "--tools" || arg == "-t") && i + 1 < args.len() {
            i += 1;
            let val = &args[i];
            let tools_vec: Vec<String> = val
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            result.tools = Some(tools_vec);
        } else if (arg == "--exclude-tools" || arg == "-xt") && i + 1 < args.len() {
            i += 1;
            let val = &args[i];
            let exclude_tools_vec: Vec<String> = val
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            result.exclude_tools = Some(exclude_tools_vec);
        } else if arg == "--thinking" && i + 1 < args.len() {
            i += 1;
            let level = &args[i];
            if let Some(tl) = parse_thinking_level(level) {
                result.thinking = Some(tl);
            } else {
                result.diagnostics.push(Diagnostic {
                    r#type: DiagnosticType::Warning,
                    message: format!(
                        "Invalid thinking level \"{}\". Valid values: off, minimal, low, medium, high, xhigh, max",
                        level
                    ),
                });
            }
        } else if arg == "--print" || arg == "-p" {
            result.print = true;
            if i + 1 < args.len() {
                let next = &args[i + 1];
                if !next.starts_with('@') && (!next.starts_with('-') || next.starts_with("---")) {
                    result.messages.push(next.clone());
                    i += 1;
                }
            }
        } else if arg == "--export" && i + 1 < args.len() {
            i += 1;
            result.export = Some(args[i].clone());
        } else if (arg == "--extension" || arg == "-e") && i + 1 < args.len() {
            i += 1;
            let mut list = result.extensions.take().unwrap_or_default();
            list.push(args[i].clone());
            result.extensions = Some(list);
        } else if arg == "--no-extensions" || arg == "-ne" {
            result.no_extensions = true;
        } else if arg == "--skill" && i + 1 < args.len() {
            i += 1;
            let mut list = result.skills.take().unwrap_or_default();
            list.push(args[i].clone());
            result.skills = Some(list);
        } else if arg == "--prompt-template" && i + 1 < args.len() {
            i += 1;
            let mut list = result.prompt_templates.take().unwrap_or_default();
            list.push(args[i].clone());
            result.prompt_templates = Some(list);
        } else if arg == "--theme" && i + 1 < args.len() {
            i += 1;
            let mut list = result.themes.take().unwrap_or_default();
            list.push(args[i].clone());
            result.themes = Some(list);
        } else if arg == "--no-skills" || arg == "-ns" {
            result.no_skills = true;
        } else if arg == "--no-prompt-templates" || arg == "-np" {
            result.no_prompt_templates = true;
        } else if arg == "--no-themes" {
            result.no_themes = true;
        } else if arg == "--no-context-files" || arg == "-nc" {
            result.no_context_files = true;
        } else if arg == "--list-models" {
            if i + 1 < args.len() && !args[i + 1].starts_with('-') && !args[i + 1].starts_with('@')
            {
                i += 1;
                result.list_models = Some(ListModels::Search(args[i].clone()));
            } else {
                result.list_models = Some(ListModels::All);
            }
        } else if arg == "--verbose" {
            result.verbose = true;
        } else if arg == "--approve" || arg == "-a" {
            result.project_trust_override = Some(true);
        } else if arg == "--no-approve" || arg == "-na" {
            result.project_trust_override = Some(false);
        } else if arg == "--offline" {
            result.offline = true;
        } else if let Some(stripped) = arg.strip_prefix('@') {
            result.file_args.push(stripped.to_string());
        } else if let Some(stripped) = arg.strip_prefix("--") {
            if let Some(eq_index) = stripped.find('=') {
                let flag_name = &stripped[..eq_index];
                let flag_val = &stripped[eq_index + 1..];
                insert_unknown_flag(
                    &mut result.unknown_flags,
                    flag_name.to_string(),
                    UnknownFlagValue::Str(flag_val.to_string()),
                );
            } else {
                let flag_name = stripped;
                if i + 1 < args.len() {
                    let next = &args[i + 1];
                    if !next.starts_with('-') && !next.starts_with('@') {
                        insert_unknown_flag(
                            &mut result.unknown_flags,
                            flag_name.to_string(),
                            UnknownFlagValue::Str(next.clone()),
                        );
                        i += 1;
                    } else {
                        insert_unknown_flag(
                            &mut result.unknown_flags,
                            flag_name.to_string(),
                            UnknownFlagValue::Bool(true),
                        );
                    }
                } else {
                    insert_unknown_flag(
                        &mut result.unknown_flags,
                        flag_name.to_string(),
                        UnknownFlagValue::Bool(true),
                    );
                }
            }
        } else if arg.starts_with('-') && !arg.starts_with("--") {
            result.diagnostics.push(Diagnostic {
                r#type: DiagnosticType::Error,
                message: format!("Unknown option: {}", arg),
            });
        } else {
            result.messages.push(arg.clone());
        }

        i += 1;
    }

    result
}

pub fn validate_arg_combinations(parsed: &Args) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Validate fork flags combination
    if parsed.fork.is_some() {
        let mut conflicting = Vec::new();
        if parsed.session.is_some() {
            conflicting.push("--session");
        }
        if parsed.r#continue {
            conflicting.push("--continue");
        }
        if parsed.resume {
            conflicting.push("--resume");
        }
        if parsed.no_session {
            conflicting.push("--no-session");
        }
        if !conflicting.is_empty() {
            diagnostics.push(Diagnostic {
                r#type: DiagnosticType::Error,
                message: format!("--fork cannot be combined with {}", conflicting.join(", ")),
            });
        }
    }

    // Validate session-id flags combination
    if parsed.session_id.is_some() {
        let mut conflicting = Vec::new();
        if parsed.session.is_some() {
            conflicting.push("--session");
        }
        if parsed.r#continue {
            conflicting.push("--continue");
        }
        if parsed.resume {
            conflicting.push("--resume");
        }
        if !conflicting.is_empty() {
            diagnostics.push(Diagnostic {
                r#type: DiagnosticType::Error,
                message: format!(
                    "--session-id cannot be combined with {}",
                    conflicting.join(", ")
                ),
            });
        }
    }

    diagnostics
}

pub fn get_help_text(extension_flags: Option<&[ExtensionFlag]>) -> String {
    let app_name = crate::config::APP_NAME;
    let config_dir_name = crate::config::CONFIG_DIR_NAME;
    let env_agent_dir = crate::config::env_agent_dir_key();
    let env_session_dir = crate::config::env_session_dir_key();

    let padded_env_agent_dir = format!("{:<32}", env_agent_dir);
    let padded_env_session_dir = format!("{:<32}", env_session_dir);

    let extension_flags_text = if let Some(flags) = extension_flags {
        if !flags.is_empty() {
            let mut s = String::new();
            s.push_str("\n\x1b[1mExtension CLI Flags:\x1b[22m\n");
            for flag in flags {
                let val_suffix = if flag.r#type == "string" {
                    " <value>"
                } else {
                    ""
                };
                let flag_str = format!("  --{}{}", flag.name, val_suffix);
                let desc_buf;
                let desc = match &flag.description {
                    Some(d) => d.as_str(),
                    None => {
                        desc_buf = format!("Registered by {}", flag.extension_path);
                        &desc_buf
                    }
                };
                s.push_str(&format!("{:<30}{}\n", flag_str, desc));
            }
            s
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    format!(
        "\x1b[1m{app_name}\x1b[22m - AI coding assistant with read, bash, edit, write tools

\x1b[1mUsage:\x1b[22m
  {app_name} [options] [@files...] [messages...]

\x1b[1mCommands:\x1b[22m
  {app_name} install <source> [-l]     Install extension source and add to settings
  {app_name} remove <source> [-l]      Remove extension source from settings
  {app_name} uninstall <source> [-l]   Alias for remove
  {app_name} update [source|self|pi]   Update pi (use --all for pi and extensions)
  {app_name} list                      List installed extensions from settings
  {app_name} config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  {app_name} <command> --help          Show help for install/remove/uninstall/update/list/config

\x1b[1mOptions:\x1b[22m
  --provider <name>              Provider name (default: google)
  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue previous session
  --resume, -r                   Select a session to resume
  --session <path|id>            Use specific session file or partial UUID
  --session-id <id>              Use exact project session ID, creating it if missing
  --fork <path|id>               Fork specific session file or partial UUID into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --name, -n <name>              Set session display name
  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling
                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names to enable
                                 Applies to built-in, extension, and custom tools
  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names to disable
                                 Applies to built-in, extension, and custom tools
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max
  --extension, -e <path>         Load an extension file (can be used multiple times)
  --no-extensions, -ne           Disable extension discovery (explicit -e paths still work)
  --skill <path>                 Load a skill file or directory (can be used multiple times)
  --no-skills, -ns               Disable skills discovery and loading
  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)
  --no-prompt-templates, -np     Disable prompt template discovery and loading
  --theme <path>                 Load a theme file or directory (can be used multiple times)
  --no-themes                    Disable theme discovery and loading
  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading
  --export <file>                Export session file to HTML and exit
  --list-models [search]         List available models (with optional fuzzy search)
  --verbose                      Force verbose startup (overrides quietStartup setting)
  --approve, -a                  Trust project-local files for this run
  --no-approve, -na              Ignore project-local files for this run
  --offline                      Disable startup network operations (same as PI_OFFLINE=1)
  --help, -h                     Show this help
  --version, -v                  Show version number

Extensions can register additional flags (e.g., --plan from plan-mode extension).{extension_flags_text}

\x1b[1mExamples:\x1b[22m
  # Interactive mode
  {app_name}

  # Interactive mode with initial prompt
  {app_name} \"List all .ts files in src/\"

  # Include files in initial message
  {app_name} @prompt.md @image.png \"What color is the sky?\"

  # Non-interactive mode (process and exit)
  {app_name} -p \"List all .ts files in src/\"

  # Multiple messages (interactive)
  {app_name} \"Read package.json\" \"What dependencies do we have?\"

  # Continue previous session
  {app_name} --continue \"What did we discuss?\"

  # Start a named session
  {app_name} --name \"Refactor auth module\"

  # Use different model
  {app_name} --provider openai --model gpt-4o-mini \"Help me refactor this code\"

  # Use model with provider prefix (no --provider needed)
  {app_name} --model openai/gpt-4o \"Help me refactor this code\"

  # Use model with thinking level shorthand
  {app_name} --model sonnet:high \"Solve this complex problem\"

  # Limit model cycling to specific models
  {app_name} --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
  {app_name} --models \"github-copilot/*\"

  # Cycle models with fixed thinking levels
  {app_name} --models sonnet:high,haiku:low

  # Start with a specific thinking level
  {app_name} --thinking high \"Solve this complex problem\"

  # Read-only mode (no file modifications possible)
  {app_name} --tools read,grep,find,ls -p \"Review the code in src/\"

  # Disable one tool while keeping the rest available
  {app_name} --exclude-tools ask_question

  # Export a session file to HTML
  {app_name} --export ~/{config_dir_name}/agent/sessions/--path--/session.jsonl
  {app_name} --export session.jsonl output.html

\x1b[1mEnvironment Variables:\x1b[22m
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  AZURE_OPENAI_BASE_URL            - Azure OpenAI/Cognitive Services base URL (e.g. https://{{resource}}.openai.azure.com)
  AZURE_OPENAI_RESOURCE_NAME       - Azure OpenAI resource name (alternative to base URL)
  AZURE_OPENAI_API_VERSION         - Azure OpenAI API version (default: v1)
  AZURE_OPENAI_DEPLOYMENT_NAME_MAP - Azure OpenAI model=deployment map (comma-separated)
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  GEMINI_API_KEY                   - Google Gemini API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  KIMI_API_KEY                     - Kimi For Coding API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  CLOUDFLARE_ACCOUNT_ID            - Cloudflare account id (required for both)
  CLOUDFLARE_GATEWAY_ID            - Cloudflare AI Gateway slug (required for AI Gateway)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key (api.xiaomimimo.com billing)
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  AWS_PROFILE                      - AWS profile for Amazon Bedrock
  AWS_ACCESS_KEY_ID                - AWS access key for Amazon Bedrock
  AWS_SECRET_ACCESS_KEY            - AWS secret key for Amazon Bedrock
  AWS_BEARER_TOKEN_BEDROCK         - Bedrock API key (bearer token)
  AWS_REGION                       - AWS region for Amazon Bedrock (e.g., us-east-1)
  {padded_env_agent_dir} - Config directory (default: ~/{config_dir_name}/agent)
  {padded_env_session_dir} - Session storage directory (overridden by --session-dir)
  PI_PACKAGE_DIR                   - Override package directory (for Nix/Guix store paths)
  PI_OFFLINE                       - Disable startup network operations when set to 1/true/yes
  PI_TELEMETRY                     - Override install telemetry when set to 1/true/yes or 0/false/no
  PI_SHARE_VIEWER_URL              - Base URL for /share command (default: https://pi.dev/session/)

\x1b[1mBuilt-in Tool Names:\x1b[22m
  read   - Read file contents
  bash   - Execute bash commands
  edit   - Edit files with find/replace
  write  - Write files (creates/overwrites)
  grep   - Search file contents (read-only, off by default)
  find   - Find files by glob pattern (read-only, off by default)
  ls     - List directory contents (read-only, off by default)
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isolated_flags_table() {
        #[allow(dead_code)]
        struct FlagTest {
            name: &'static str,
            args: Vec<&'static str>,
            check: Box<dyn Fn(&Args)>,
        }

        let test_cases = vec![
            FlagTest {
                name: "help flag long",
                args: vec!["--help"],
                check: Box::new(|p| assert!(p.help)),
            },
            FlagTest {
                name: "help flag short",
                args: vec!["-h"],
                check: Box::new(|p| assert!(p.help)),
            },
            FlagTest {
                name: "version flag long",
                args: vec!["--version"],
                check: Box::new(|p| assert!(p.version)),
            },
            FlagTest {
                name: "version flag short",
                args: vec!["-v"],
                check: Box::new(|p| assert!(p.version)),
            },
            FlagTest {
                name: "mode text",
                args: vec!["--mode", "text"],
                check: Box::new(|p| assert_eq!(p.mode, Some(Mode::Text))),
            },
            FlagTest {
                name: "mode json",
                args: vec!["--mode", "json"],
                check: Box::new(|p| assert_eq!(p.mode, Some(Mode::Json))),
            },
            FlagTest {
                name: "mode rpc",
                args: vec!["--mode", "rpc"],
                check: Box::new(|p| assert_eq!(p.mode, Some(Mode::Rpc))),
            },
            FlagTest {
                name: "mode invalid",
                args: vec!["--mode", "invalid"],
                check: Box::new(|p| assert_eq!(p.mode, None)),
            },
            FlagTest {
                name: "continue long",
                args: vec!["--continue"],
                check: Box::new(|p| assert!(p.r#continue)),
            },
            FlagTest {
                name: "continue short",
                args: vec!["-c"],
                check: Box::new(|p| assert!(p.r#continue)),
            },
            FlagTest {
                name: "resume long",
                args: vec!["--resume"],
                check: Box::new(|p| assert!(p.resume)),
            },
            FlagTest {
                name: "resume short",
                args: vec!["-r"],
                check: Box::new(|p| assert!(p.resume)),
            },
            FlagTest {
                name: "provider",
                args: vec!["--provider", "google"],
                check: Box::new(|p| assert_eq!(p.provider.as_deref(), Some("google"))),
            },
            FlagTest {
                name: "model",
                args: vec!["--model", "gpt-4o"],
                check: Box::new(|p| assert_eq!(p.model.as_deref(), Some("gpt-4o"))),
            },
            FlagTest {
                name: "api key",
                args: vec!["--api-key", "my-key"],
                check: Box::new(|p| assert_eq!(p.api_key.as_deref(), Some("my-key"))),
            },
            FlagTest {
                name: "system prompt",
                args: vec!["--system-prompt", "hello system"],
                check: Box::new(|p| assert_eq!(p.system_prompt.as_deref(), Some("hello system"))),
            },
            FlagTest {
                name: "append system prompt multiple",
                args: vec![
                    "--append-system-prompt",
                    "file1",
                    "--append-system-prompt",
                    "file2",
                ],
                check: Box::new(|p| {
                    assert_eq!(
                        p.append_system_prompt.as_ref().unwrap(),
                        &vec!["file1".to_string(), "file2".to_string()]
                    )
                }),
            },
            FlagTest {
                name: "name long",
                args: vec!["--name", "session-name"],
                check: Box::new(|p| assert_eq!(p.name.as_deref(), Some("session-name"))),
            },
            FlagTest {
                name: "name short",
                args: vec!["-n", "session-name"],
                check: Box::new(|p| assert_eq!(p.name.as_deref(), Some("session-name"))),
            },
            FlagTest {
                name: "no session",
                args: vec!["--no-session"],
                check: Box::new(|p| assert!(p.no_session)),
            },
            FlagTest {
                name: "session",
                args: vec!["--session", "session-file"],
                check: Box::new(|p| assert_eq!(p.session.as_deref(), Some("session-file"))),
            },
            FlagTest {
                name: "session id",
                args: vec!["--session-id", "my-uuid"],
                check: Box::new(|p| assert_eq!(p.session_id.as_deref(), Some("my-uuid"))),
            },
            FlagTest {
                name: "fork",
                args: vec!["--fork", "fork-file"],
                check: Box::new(|p| assert_eq!(p.fork.as_deref(), Some("fork-file"))),
            },
            FlagTest {
                name: "session dir",
                args: vec!["--session-dir", "/path/to/sessions"],
                check: Box::new(|p| {
                    assert_eq!(p.session_dir.as_deref(), Some("/path/to/sessions"))
                }),
            },
            FlagTest {
                name: "models cycling",
                args: vec!["--models", "gpt-4,claude-3-5,haiku"],
                check: Box::new(|p| {
                    assert_eq!(
                        p.models.as_ref().unwrap(),
                        &vec![
                            "gpt-4".to_string(),
                            "claude-3-5".to_string(),
                            "haiku".to_string()
                        ]
                    )
                }),
            },
            FlagTest {
                name: "no tools long",
                args: vec!["--no-tools"],
                check: Box::new(|p| assert!(p.no_tools)),
            },
            FlagTest {
                name: "no tools short",
                args: vec!["-nt"],
                check: Box::new(|p| assert!(p.no_tools)),
            },
            FlagTest {
                name: "no builtin tools long",
                args: vec!["--no-builtin-tools"],
                check: Box::new(|p| assert!(p.no_builtin_tools)),
            },
            FlagTest {
                name: "no builtin tools short",
                args: vec!["-nbt"],
                check: Box::new(|p| assert!(p.no_builtin_tools)),
            },
            FlagTest {
                name: "tools long",
                args: vec!["--tools", "read,write,bash"],
                check: Box::new(|p| {
                    assert_eq!(
                        p.tools.as_ref().unwrap(),
                        &vec!["read".to_string(), "write".to_string(), "bash".to_string()]
                    )
                }),
            },
            FlagTest {
                name: "tools short",
                args: vec!["-t", "read,write,bash"],
                check: Box::new(|p| {
                    assert_eq!(
                        p.tools.as_ref().unwrap(),
                        &vec!["read".to_string(), "write".to_string(), "bash".to_string()]
                    )
                }),
            },
            FlagTest {
                name: "exclude tools long",
                args: vec!["--exclude-tools", "grep,find"],
                check: Box::new(|p| {
                    assert_eq!(
                        p.exclude_tools.as_ref().unwrap(),
                        &vec!["grep".to_string(), "find".to_string()]
                    )
                }),
            },
            FlagTest {
                name: "exclude tools short",
                args: vec!["-xt", "grep,find"],
                check: Box::new(|p| {
                    assert_eq!(
                        p.exclude_tools.as_ref().unwrap(),
                        &vec!["grep".to_string(), "find".to_string()]
                    )
                }),
            },
            FlagTest {
                name: "thinking valid",
                args: vec!["--thinking", "medium"],
                check: Box::new(|p| assert_eq!(p.thinking, Some(ThinkingLevel::Medium))),
            },
            FlagTest {
                name: "print flag long",
                args: vec!["--print"],
                check: Box::new(|p| assert!(p.print)),
            },
            FlagTest {
                name: "print flag short",
                args: vec!["-p"],
                check: Box::new(|p| assert!(p.print)),
            },
            FlagTest {
                name: "print flag with message consumed",
                args: vec!["-p", "my prompt message"],
                check: Box::new(|p| {
                    assert!(p.print);
                    assert_eq!(p.messages, vec!["my prompt message".to_string()]);
                }),
            },
            FlagTest {
                name: "export flag",
                args: vec!["--export", "index.html"],
                check: Box::new(|p| assert_eq!(p.export.as_deref(), Some("index.html"))),
            },
            FlagTest {
                name: "extension flag long",
                args: vec!["--extension", "foo.ts"],
                check: Box::new(|p| {
                    assert_eq!(p.extensions.as_ref().unwrap(), &vec!["foo.ts".to_string()])
                }),
            },
            FlagTest {
                name: "extension flag short",
                args: vec!["-e", "foo.ts"],
                check: Box::new(|p| {
                    assert_eq!(p.extensions.as_ref().unwrap(), &vec!["foo.ts".to_string()])
                }),
            },
            FlagTest {
                name: "no extensions long",
                args: vec!["--no-extensions"],
                check: Box::new(|p| assert!(p.no_extensions)),
            },
            FlagTest {
                name: "no extensions short",
                args: vec!["-ne"],
                check: Box::new(|p| assert!(p.no_extensions)),
            },
            FlagTest {
                name: "skill",
                args: vec!["--skill", "skill_dir"],
                check: Box::new(|p| {
                    assert_eq!(p.skills.as_ref().unwrap(), &vec!["skill_dir".to_string()])
                }),
            },
            FlagTest {
                name: "prompt template",
                args: vec!["--prompt-template", "prompt_dir"],
                check: Box::new(|p| {
                    assert_eq!(
                        p.prompt_templates.as_ref().unwrap(),
                        &vec!["prompt_dir".to_string()]
                    )
                }),
            },
            FlagTest {
                name: "theme",
                args: vec!["--theme", "theme_dir"],
                check: Box::new(|p| {
                    assert_eq!(p.themes.as_ref().unwrap(), &vec!["theme_dir".to_string()])
                }),
            },
            FlagTest {
                name: "no skills long",
                args: vec!["--no-skills"],
                check: Box::new(|p| assert!(p.no_skills)),
            },
            FlagTest {
                name: "no skills short",
                args: vec!["-ns"],
                check: Box::new(|p| assert!(p.no_skills)),
            },
            FlagTest {
                name: "no prompt templates long",
                args: vec!["--no-prompt-templates"],
                check: Box::new(|p| assert!(p.no_prompt_templates)),
            },
            FlagTest {
                name: "no prompt templates short",
                args: vec!["-np"],
                check: Box::new(|p| assert!(p.no_prompt_templates)),
            },
            FlagTest {
                name: "no themes",
                args: vec!["--no-themes"],
                check: Box::new(|p| assert!(p.no_themes)),
            },
            FlagTest {
                name: "no context files long",
                args: vec!["--no-context-files"],
                check: Box::new(|p| assert!(p.no_context_files)),
            },
            FlagTest {
                name: "no context files short",
                args: vec!["-nc"],
                check: Box::new(|p| assert!(p.no_context_files)),
            },
            FlagTest {
                name: "list models basic",
                args: vec!["--list-models"],
                check: Box::new(|p| assert_eq!(p.list_models, Some(ListModels::All))),
            },
            FlagTest {
                name: "list models search",
                args: vec!["--list-models", "gpt"],
                check: Box::new(|p| {
                    assert_eq!(p.list_models, Some(ListModels::Search("gpt".to_string())))
                }),
            },
            FlagTest {
                name: "verbose",
                args: vec!["--verbose"],
                check: Box::new(|p| assert!(p.verbose)),
            },
            FlagTest {
                name: "approve long",
                args: vec!["--approve"],
                check: Box::new(|p| assert_eq!(p.project_trust_override, Some(true))),
            },
            FlagTest {
                name: "approve short",
                args: vec!["-a"],
                check: Box::new(|p| assert_eq!(p.project_trust_override, Some(true))),
            },
            FlagTest {
                name: "no approve long",
                args: vec!["--no-approve"],
                check: Box::new(|p| assert_eq!(p.project_trust_override, Some(false))),
            },
            FlagTest {
                name: "no approve short",
                args: vec!["-na"],
                check: Box::new(|p| assert_eq!(p.project_trust_override, Some(false))),
            },
            FlagTest {
                name: "offline",
                args: vec!["--offline"],
                check: Box::new(|p| assert!(p.offline)),
            },
            FlagTest {
                name: "file arg",
                args: vec!["@prompt.md"],
                check: Box::new(|p| assert_eq!(p.file_args, vec!["prompt.md".to_string()])),
            },
            FlagTest {
                name: "message arg",
                args: vec!["my custom message"],
                check: Box::new(|p| assert_eq!(p.messages, vec!["my custom message".to_string()])),
            },
        ];

        for tc in test_cases {
            let str_args: Vec<String> = tc.args.iter().map(|s| s.to_string()).collect();
            let parsed = parse_args(&str_args);
            (tc.check)(&parsed);
        }
    }

    #[test]
    fn test_precedence_and_repeated_flags() {
        // Repeated single-value flags overwrite
        let args = vec![
            "--provider".to_string(),
            "google".to_string(),
            "--provider".to_string(),
            "openai".to_string(),
        ];
        let parsed = parse_args(&args);
        assert_eq!(parsed.provider, Some("openai".to_string()));

        // Repeated --print with message consumption
        let args = vec![
            "-p".to_string(),
            "hello".to_string(),
            "-p".to_string(),
            "world".to_string(),
        ];
        let parsed = parse_args(&args);
        assert!(parsed.print);
        assert_eq!(
            parsed.messages,
            vec!["hello".to_string(), "world".to_string()]
        );
    }

    #[test]
    fn test_unknown_flags_and_order() {
        let args = vec![
            "--custom-val".to_string(),
            "hello".to_string(),
            "--custom-bool".to_string(),
            "--custom-eq=world".to_string(),
        ];
        let parsed = parse_args(&args);
        assert_eq!(parsed.unknown_flags.len(), 3);
        assert_eq!(
            parsed.unknown_flags[0],
            (
                "custom-val".to_string(),
                UnknownFlagValue::Str("hello".to_string())
            )
        );
        assert_eq!(
            parsed.unknown_flags[1],
            ("custom-bool".to_string(), UnknownFlagValue::Bool(true))
        );
        assert_eq!(
            parsed.unknown_flags[2],
            (
                "custom-eq".to_string(),
                UnknownFlagValue::Str("world".to_string())
            )
        );

        // Overwrites do not change insertion position
        let args = vec![
            "--z".to_string(),
            "--a".to_string(),
            "--z=new_val".to_string(),
        ];
        let parsed = parse_args(&args);
        assert_eq!(parsed.unknown_flags.len(), 2);
        assert_eq!(
            parsed.unknown_flags[0],
            (
                "z".to_string(),
                UnknownFlagValue::Str("new_val".to_string())
            )
        );
        assert_eq!(
            parsed.unknown_flags[1],
            ("a".to_string(), UnknownFlagValue::Bool(true))
        );
    }

    #[test]
    fn test_double_dash_handling() {
        let args = vec!["--".to_string(), "next_arg".to_string()];
        let parsed = parse_args(&args);
        assert_eq!(parsed.unknown_flags.len(), 1);
        assert_eq!(
            parsed.unknown_flags[0],
            (
                "".to_string(),
                UnknownFlagValue::Str("next_arg".to_string())
            )
        );

        let args = vec!["--".to_string()];
        let parsed = parse_args(&args);
        assert_eq!(parsed.unknown_flags.len(), 1);
        assert_eq!(
            parsed.unknown_flags[0],
            ("".to_string(), UnknownFlagValue::Bool(true))
        );
    }

    #[test]
    fn test_errors_and_warnings() {
        // Missing name value
        let args = vec!["--name".to_string()];
        let parsed = parse_args(&args);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].r#type, DiagnosticType::Error);
        assert_eq!(parsed.diagnostics[0].message, "--name requires a value");

        // Unknown single-dash option
        let args = vec!["-x".to_string()];
        let parsed = parse_args(&args);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].r#type, DiagnosticType::Error);
        assert_eq!(parsed.diagnostics[0].message, "Unknown option: -x");

        // Invalid thinking level warning
        let args = vec!["--thinking".to_string(), "invalid_level".to_string()];
        let parsed = parse_args(&args);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].r#type, DiagnosticType::Warning);
        assert_eq!(
            parsed.diagnostics[0].message,
            "Invalid thinking level \"invalid_level\". Valid values: off, minimal, low, medium, high, xhigh, max"
        );
    }

    #[test]
    fn test_combination_validations() {
        // --fork combinations
        let parsed = Args {
            fork: Some("foo".to_string()),
            session: Some("bar".to_string()),
            ..Args::default()
        };
        let diag = validate_arg_combinations(&parsed);
        assert_eq!(diag.len(), 1);
        assert_eq!(diag[0].message, "--fork cannot be combined with --session");

        let parsed = Args {
            fork: Some("foo".to_string()),
            r#continue: true,
            resume: true,
            ..Args::default()
        };
        let diag = validate_arg_combinations(&parsed);
        assert_eq!(diag.len(), 1);
        assert!(diag[0].message.contains("--continue"));
        assert!(diag[0].message.contains("--resume"));

        // --session-id combinations
        let parsed = Args {
            session_id: Some("foo".to_string()),
            session: Some("bar".to_string()),
            ..Args::default()
        };
        let diag = validate_arg_combinations(&parsed);
        assert_eq!(diag.len(), 1);
        assert_eq!(
            diag[0].message,
            "--session-id cannot be combined with --session"
        );
    }

    #[test]
    fn test_help_fixture() {
        let fixture_content = include_str!("../../tests/fixtures/help.txt");
        let expected = format!("{}\n", get_help_text(None));
        assert_eq!(fixture_content, expected);
    }

    #[test]
    fn test_version_fixture() {
        let fixture_content = include_str!("../../tests/fixtures/version.txt");
        let expected = format!("{}\n", env!("CARGO_PKG_VERSION"));
        assert_eq!(fixture_content, expected);
    }

    #[test]
    fn test_mode_resolution() {
        use crate::cli::{
            is_plain_runtime_metadata_command, resolve_app_mode, to_print_output_mode,
        };

        // resolve_app_mode
        let mut parsed = Args::default();
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Interactive);
        assert_eq!(resolve_app_mode(&parsed, false, true), AppMode::Print);
        assert_eq!(resolve_app_mode(&parsed, true, false), AppMode::Print);

        parsed.print = true;
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Print);

        parsed.print = false;
        parsed.mode = Some(Mode::Rpc);
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Rpc);

        parsed.mode = Some(Mode::Json);
        assert_eq!(resolve_app_mode(&parsed, true, true), AppMode::Json);

        // to_print_output_mode
        assert_eq!(to_print_output_mode(AppMode::Json), Mode::Json);
        assert_eq!(to_print_output_mode(AppMode::Print), Mode::Text);
        assert_eq!(to_print_output_mode(AppMode::Interactive), Mode::Text);
        assert_eq!(to_print_output_mode(AppMode::Rpc), Mode::Text);

        // is_plain_runtime_metadata_command
        let mut parsed = Args::default();
        assert!(!is_plain_runtime_metadata_command(&parsed));

        parsed.help = true;
        assert!(is_plain_runtime_metadata_command(&parsed));

        parsed.help = false;
        parsed.list_models = Some(ListModels::All);
        assert!(is_plain_runtime_metadata_command(&parsed));

        parsed.print = true;
        assert!(!is_plain_runtime_metadata_command(&parsed));
    }
}
