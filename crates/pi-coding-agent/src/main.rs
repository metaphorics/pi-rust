//! `pi` binary entry point — port of `packages/coding-agent/src/main.ts:473-859`.
//!
//! Boot order (P5 plan): offline env → cwd/agent dir → bootstrap settings →
//! package subcommands → parse args → diagnostics → --version → --export →
//! mode resolution → validations → migrations → startup settings →
//! first-time setup → session dir → session manager → --name → trust →
//! runtime factory → help/list-models → piped stdin → initial message →
//! theme init → extension bind → mode dispatch.
//!
//! This file is the binary's CLI printer: `println!`/`eprintln!` are the
//! oracle's console stdout/stderr boot contract. Once a wire mode is
//! dispatched, stdout belongs to `WireOut` (stdout-purity invariant).

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use pi_agent::AgentThinkingLevel;
use pi_coding_agent::cli::args::{Args, DiagnosticType, ListModels, Mode, UnknownFlagValue};
use pi_coding_agent::cli::{
    self, AppMode, initial_message, model_select, session_select, startup_ui,
};
use pi_coding_agent::config::{
    env_session_dir_key, expand_tilde_path, get_agent_dir, normalize_path,
};
use pi_coding_agent::export_html::{
    ExportHtmlOptions, export_session_file_to_html, rpc_export_html_handler,
};
use pi_coding_agent::extension_bridge::{NoopExtensionBridge, SessionStartReason};
use pi_coding_agent::extensions::LauncherSource;
use pi_coding_agent::extensions::binding::{
    BindOptions, ExtensionBinding, SessionHostActions, bind_extensions,
};
use pi_coding_agent::migrations::run_migrations;
use pi_coding_agent::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveModeOptions,
};
use pi_coding_agent::modes::interactive::theme::init_theme;
use pi_coding_agent::modes::interactive::trust_store::ProjectTrustStore;
use pi_coding_agent::modes::print::{PrintModeOptions, PrintOutputMode, run_print_mode};
use pi_coding_agent::modes::rpc::{RpcModeOptions, run_rpc_mode};
use pi_coding_agent::resource_loader::{ResourceLoaderOptions, load_prompt_templates, load_skills};
use pi_coding_agent::session::runtime::{
    AgentSessionRuntime, CreateRuntimeFactory, CreateRuntimeOptions, CreateRuntimeResult,
};
use pi_coding_agent::session::services::{
    CreateAgentSessionServicesOptions, DiagnosticLevel, RuntimeDiagnostic,
    create_agent_session_services,
};
use pi_coding_agent::session::{
    AgentSession, AgentSessionConfig, format_no_models_available_message, parse_thinking_level,
};
use pi_coding_agent::settings_manager::SettingsManager;
use pi_coding_agent::system_prompt::load_project_context_files;
use pi_coding_agent::modes::interactive::components::config_selector::{
    ConfigSelectorComponent, ConfigWriteScope, ScopedResolvedPaths,
};
use pi_coding_agent::{
    AuthStorage, DefaultPackageManager, get_config_command_help, get_config_command_usage,
    handle_package_command, parse_config_command,
};

fn eprintln_red(message: &str) {
    eprintln!("\x1b[31m{message}\x1b[39m");
}

fn eprintln_yellow(message: &str) {
    eprintln!("\x1b[33m{message}\x1b[39m");
}

fn is_truthy_env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(value) => {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

/// Print parse/combination diagnostics; exit 1 when any is an error
/// (main.ts:509-517).
fn report_arg_diagnostics(diagnostics: &[pi_coding_agent::cli::args::Diagnostic]) {
    for diagnostic in diagnostics {
        match diagnostic.r#type {
            DiagnosticType::Error => eprintln_red(&format!("Error: {}", diagnostic.message)),
            DiagnosticType::Warning => {
                eprintln_yellow(&format!("Warning: {}", diagnostic.message));
            }
        }
    }
    if diagnostics
        .iter()
        .any(|d| d.r#type == DiagnosticType::Error)
    {
        std::process::exit(1);
    }
}

/// Oracle `reportDiagnostics` (main.ts:87-93).
fn report_runtime_diagnostics(diagnostics: &[RuntimeDiagnostic]) {
    for diagnostic in diagnostics {
        match diagnostic.level {
            DiagnosticLevel::Error => eprintln_red(&format!("Error: {}", diagnostic.message)),
            DiagnosticLevel::Warning => {
                eprintln_yellow(&format!("Warning: {}", diagnostic.message));
            }
            DiagnosticLevel::Info => eprintln!("\x1b[2m{}\x1b[22m", diagnostic.message),
        }
    }
}

/// Oracle `hasTrustRequiringProjectResources` (trust-manager.ts:184-206).
fn has_trust_requiring_project_resources(cwd: &Path) -> bool {
    const TRUST_REQUIRING: [&str; 5] =
        ["settings.json", "extensions", "skills", "prompts", "themes"];
    let config_dir = cwd.join(pi_coding_agent::config::CONFIG_DIR_NAME);
    if TRUST_REQUIRING
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }
    let user_agents_skills = dirs::home_dir().map(|home| home.join(".agents").join("skills"));
    let mut current = cwd.to_path_buf();
    loop {
        let agents_skills = current.join(".agents").join("skills");
        if Some(&agents_skills) != user_agents_skills.as_ref() && agents_skills.exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

/// Oracle `resolveProjectTrusted` (core/project-trust.ts:46-99), minus the
/// extension `project_trust` hook (extensions load after trust is known in
/// the sidecar architecture).
fn resolve_project_trusted(
    cwd: &Path,
    agent_dir: &Path,
    trust_override: Option<bool>,
    default_project_trust: &str,
    interactive_ui: Option<&Arc<Mutex<SettingsManager>>>,
) -> bool {
    if let Some(overridden) = trust_override {
        return overridden;
    }
    if !has_trust_requiring_project_resources(cwd) {
        return true;
    }
    let store = ProjectTrustStore::new(agent_dir);
    if let Ok(Some(entry)) = store.get_entry(cwd) {
        return entry.decision;
    }
    match default_project_trust {
        "always" => return true,
        "never" => return false,
        _ => {}
    }
    let Some(settings_manager) = interactive_ui else {
        return false;
    };

    // Oracle prompt (project-trust.ts:24-27) over the startup selector with
    // getProjectTrustOptions(includeSessionOnly).
    let trust_path = cwd.display().to_string();
    let parent = cwd.parent().map(|p| p.display().to_string());
    let mut labels = vec!["Trust".to_string()];
    if let Some(parent) = &parent {
        labels.push(format!("Trust parent folder ({parent})"));
    }
    labels.push("Trust (this session only)".to_string());
    labels.push("Do not trust".to_string());
    labels.push("Do not trust (this session only)".to_string());

    let title = format!(
        "Trust project folder?\n{trust_path}\n\nThis allows pi to load {} settings and resources, install missing project packages, and execute project extensions.",
        pi_coding_agent::config::CONFIG_DIR_NAME
    );
    let terminal = pi_tui::terminal::ProcessTerminal::new();
    let mut ui = startup_ui::create_startup_tui(agent_dir, settings_manager, terminal);
    let selected = startup_ui::show_startup_selector(&mut ui, &title, &labels);
    ui.stop();
    let Some(index) = selected else {
        return false;
    };
    let label = labels[index].as_str();
    let (trusted, saved): (bool, Option<(String, bool)>) = match label {
        "Trust" => (true, Some((trust_path.clone(), true))),
        "Trust (this session only)" => (true, None),
        "Do not trust" => (false, Some((trust_path.clone(), false))),
        "Do not trust (this session only)" => (false, None),
        _ => (true, parent.map(|p| (p, true))),
    };
    if let Some((path, decision)) = saved {
        let _ = store.set_many(&[
            pi_coding_agent::modes::interactive::components::trust_selector::ProjectTrustUpdate {
                path,
                decision: Some(decision),
            },
        ]);
    }
    trusted
}
/// Oracle `selectConfig` (cli/config-selector.ts:20-56): standalone TUI
/// hosting the config selector; returns when the selector is closed (Esc)
/// or exited (q) — both exit the process with code 0 afterwards.
fn select_config(
    resolved_paths: &ScopedResolvedPaths,
    settings_manager: Arc<Mutex<SettingsManager>>,
    cwd: &Path,
    agent_dir: &Path,
    write_scope: ConfigWriteScope,
    project_mode_available: bool,
) {
    let theme_name = settings_manager.lock().get_theme().map(str::to_owned);
    init_theme(theme_name.as_deref(), true);
    // Esc/Tab/confirm dispatch through the app keybinding catalog (the
    // startup TUI installs the same set).
    pi_tui::keybindings::set_keybindings(
        pi_coding_agent::modes::interactive::app_keybindings::create_app_keybindings(agent_dir),
    );
    let mut ui = pi_tui::Tui::new(pi_tui::terminal::ProcessTerminal::new());
    let rows = ui.terminal().rows();
    let done = std::rc::Rc::new(std::cell::Cell::new(false));
    let close_flag = done.clone();
    let exit_flag = done.clone();
    let selector = ConfigSelectorComponent::new(
        resolved_paths,
        settings_manager,
        cwd,
        agent_dir,
        Box::new(move || close_flag.set(true)),
        Box::new(move || exit_flag.set(true)),
        // The loop below repaints every tick; explicit render requests are
        // satisfied by the next iteration.
        Box::new(|| {}),
        Some(rows),
        write_scope,
        project_mode_available,
    );
    ui.add_child(selector);
    ui.set_focus_child(Some(0));
    ui.start_render_loop_hooks();
    while !done.get() {
        ui.poll_terminal();
        ui.do_render();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    ui.stop();
    pi_coding_agent::modes::interactive::theme::watcher::stop_theme_watcher();
}

/// Oracle `handleConfigCommand` (package-manager-cli.ts:553-624). Runs
/// before general arg parsing; `Some(exit_code)` when the first argument
/// is `config`.
fn handle_config_command(raw_args: &[String], cwd: &Path, agent_dir: &Path) -> Option<i32> {
    let options = parse_config_command(raw_args)?;
    if options.help {
        print!("{}", get_config_command_help());
        return Some(0);
    }
    if let Some(arg) = &options.invalid_option {
        eprintln!("Unknown option {arg} for \"config\".");
        eprintln!(
            "Use \"{} --help\" or \"{}\".",
            pi_coding_agent::config::APP_NAME,
            get_config_command_usage()
        );
        return Some(1);
    }
    if let Some(arg) = &options.invalid_argument {
        eprintln!("Unexpected argument {arg}.");
        eprintln!("Usage: {}", get_config_command_usage());
        return Some(1);
    }

    // Oracle `createCommandSettingsManager`: the command settings manager
    // starts untrusted (`{ projectTrusted: false }` — no project settings
    // loaded) and receives the resolved trust afterwards. Trust is prompted
    // interactively only when both stdio ends are TTYs
    // (`getCommandAppMode`, package-manager-cli.ts:495-497).
    let startup_settings = {
        let mut settings = SettingsManager::create(cwd, Some(agent_dir.to_path_buf()));
        settings.set_project_trusted(false);
        Arc::new(Mutex::new(settings))
    };
    let default_project_trust = startup_settings
        .lock()
        .get_default_project_trust()
        .to_string();
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let trusted = resolve_project_trusted(
        cwd,
        agent_dir,
        options.project_trust_override,
        &default_project_trust,
        interactive.then_some(&startup_settings),
    );
    // Oracle sets projectTrusted on the command settings manager before
    // anything reads it (createCommandSettingsManager): untrusted managers
    // hold no project settings and surface no project parse errors.
    startup_settings.lock().set_project_trusted(trusted);
    if options.local && !trusted {
        eprintln!("Project is not trusted. Use --approve to modify local resource config.");
        return Some(1);
    }
    // Oracle `reportSettingsErrors(settingsManager, "config command")`.
    for (scope, error) in startup_settings.lock().load_errors() {
        eprintln_yellow(&format!(
            "Warning (config command, {scope} settings): {error}"
        ));
    }

    let mut global_settings = SettingsManager::create(cwd, Some(agent_dir.to_path_buf()));
    global_settings.set_project_trusted(false);
    let mut global_manager = DefaultPackageManager::new(cwd, agent_dir, global_settings);
    let global_paths = match global_manager.resolve() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln_red(&format!("Error: {error}"));
            return Some(1);
        }
    };
    let project_paths = if trusted {
        let mut settings = SettingsManager::create(cwd, Some(agent_dir.to_path_buf()));
        settings.set_project_trusted(true);
        match DefaultPackageManager::new(cwd, agent_dir, settings).resolve() {
            Ok(paths) => paths,
            Err(error) => {
                eprintln_red(&format!("Error: {error}"));
                return Some(1);
            }
        }
    } else {
        global_paths.clone()
    };

    let selector_settings = {
        let mut settings = SettingsManager::create(cwd, Some(agent_dir.to_path_buf()));
        settings.set_project_trusted(trusted);
        Arc::new(Mutex::new(settings))
    };
    select_config(
        &ScopedResolvedPaths {
            global: global_paths,
            project: project_paths,
        },
        selector_settings,
        cwd,
        agent_dir,
        if options.local {
            ConfigWriteScope::Project
        } else {
            ConfigWriteScope::Global
        },
        trusted,
    );
    Some(0)
}

/// CLI flags consumed inside the runtime factory (cloned once; `messages`
/// is consumed later by `prepare_initial_message` and never read here).
struct FactoryConfig {
    parsed: Args,
    default_project_trust: String,
    interactive_trust_prompt: bool,
    startup_settings: Arc<Mutex<SettingsManager>>,
}

fn build_runtime_factory(
    config: Arc<FactoryConfig>,
    auth_storage: Arc<AuthStorage>,
) -> CreateRuntimeFactory {
    Arc::new(move |options: CreateRuntimeOptions| {
        let config = config.clone();
        let auth_storage = auth_storage.clone();
        Box::pin(async move {
            let parsed = &config.parsed;
            let cwd = options.cwd.clone();
            let agent_dir = options.agent_dir.clone();

            // Project trust before any project resource loads.
            let trusted = resolve_project_trusted(
                &cwd,
                &agent_dir,
                parsed.project_trust_override,
                &config.default_project_trust,
                config
                    .interactive_trust_prompt
                    .then_some(&config.startup_settings),
            );
            let mut settings = SettingsManager::create(&cwd, Some(agent_dir.clone()));
            settings.set_project_trusted(trusted);
            let settings = Arc::new(Mutex::new(settings));

            let mut loader_options = ResourceLoaderOptions::new(&cwd);
            loader_options.agent_dir = agent_dir.clone();
            loader_options.additional_extension_paths =
                parsed.extensions.clone().unwrap_or_default();
            loader_options.additional_skill_paths = parsed.skills.clone().unwrap_or_default();
            loader_options.additional_prompt_paths =
                parsed.prompt_templates.clone().unwrap_or_default();
            loader_options.additional_theme_paths = parsed.themes.clone().unwrap_or_default();
            loader_options.no_extensions = parsed.no_extensions;
            loader_options.no_skills = parsed.no_skills;
            loader_options.no_prompt_templates = parsed.no_prompt_templates;
            loader_options.no_themes = parsed.no_themes;
            loader_options.no_context_files = parsed.no_context_files;

            let services = create_agent_session_services(CreateAgentSessionServicesOptions {
                cwd: cwd.clone(),
                agent_dir: Some(agent_dir.clone()),
                auth_storage: Some(auth_storage.clone()),
                settings_manager: Some(settings.clone()),
                model_registry: None,
                resource_loader_options: Some(loader_options),
            });
            let mut diagnostics = services.diagnostics.clone();

            // Resource contents for system-prompt assembly.
            let discovered = services.resource_loader.lock().discovered().clone();
            let skills = load_skills(&discovered.skills);
            let prompt_templates = load_prompt_templates(&discovered.prompts);
            let context_files = if parsed.no_context_files {
                Vec::new()
            } else {
                load_project_context_files(&services.cwd, &services.agent_dir)
            };

            // Model scope (--models or settings enabledModels).
            let (model_patterns, default_provider, default_model, default_thinking) = {
                let guard = settings.lock();
                (
                    parsed.models.clone().or_else(|| guard.get_enabled_models()),
                    guard.get_default_provider().map(str::to_owned),
                    guard.get_default_model().map(str::to_owned),
                    guard.get_default_thinking_level().map(str::to_owned),
                )
            };
            let registry = services.model_registry.clone();
            let scope = match &model_patterns {
                Some(patterns) if !patterns.is_empty() => {
                    registry.read().await.resolve_model_scope(patterns).await
                }
                _ => Default::default(),
            };
            for warning in &scope.warnings {
                diagnostics.push(RuntimeDiagnostic {
                    level: DiagnosticLevel::Warning,
                    message: warning.clone(),
                });
            }

            let session_context = options.session_manager.build_session_context();
            let has_existing_session = !session_context.messages.is_empty();
            let has_thinking_entry = options
                .session_manager
                .get_branch(None)
                .iter()
                .any(|entry| {
                    matches!(
                        entry,
                        pi_coding_agent::session_types::SessionEntry::ThinkingLevelChange { .. }
                    )
                });

            let built = {
                let registry_guard = registry.read().await;
                model_select::build_session_options(
                    parsed,
                    &scope.scoped_models,
                    has_existing_session,
                    &registry_guard,
                    default_provider.as_deref(),
                    default_model.as_deref(),
                )
                .await
            };
            diagnostics.extend(built.diagnostics);
            let mut model = built.options.model.clone();
            let mut model_fallback_message: Option<String> = None;

            // Restore model from session (sdk.ts:195-204).
            if model.is_none()
                && has_existing_session
                && let Some(saved) = &session_context.model
            {
                let registry_guard = registry.read().await;
                if let Some(restored) = registry_guard
                    .find(&saved.provider, &saved.model_id)
                    .cloned()
                    && registry_guard.has_configured_auth(&restored).await
                {
                    model = Some(restored);
                }
                if model.is_none() {
                    model_fallback_message = Some(format!(
                        "Could not restore model {}/{}",
                        saved.provider, saved.model_id
                    ));
                }
            }

            // findInitialModel fallback chain (sdk.ts:206-222).
            if model.is_none() {
                let default_thinking_model = default_thinking
                    .as_deref()
                    .map(model_select::model_thinking_from_str);
                let (found, _) = registry
                    .read()
                    .await
                    .find_initial_model(
                        default_provider.as_deref(),
                        default_model.as_deref(),
                        default_thinking_model.flatten(),
                    )
                    .await;
                model = found;
                match (&model, &mut model_fallback_message) {
                    (None, message) => {
                        *message = Some(format_no_models_available_message());
                    }
                    (Some(model), Some(message)) => {
                        message.push_str(&format!(". Using {}/{}", model.provider, model.id));
                    }
                    _ => {}
                }
            }

            // Thinking level restore + defaults (sdk.ts:224-243).
            let default_thinking_level = default_thinking
                .as_deref()
                .map(parse_thinking_level)
                .unwrap_or(AgentThinkingLevel::Medium);
            let mut thinking_level = built.options.thinking_level;
            if thinking_level.is_none() && has_existing_session {
                thinking_level = Some(if has_thinking_entry {
                    parse_thinking_level(&session_context.thinking_level)
                } else {
                    default_thinking_level
                });
            }
            let thinking_level = thinking_level.unwrap_or(default_thinking_level);
            let thinking_level =
                model_select::clamp_initial_thinking(model.as_ref(), thinking_level);

            // Tool selection (sdk.ts:245-251).
            let allowed_tool_names = built
                .options
                .tools
                .clone()
                .or_else(|| built.options.no_tools_all.then(Vec::new));
            let excluded_tool_names = built.options.exclude_tools.clone();
            let initial_active_tool_names = built.options.tools.clone().or_else(|| {
                (built.options.no_tools_all || built.options.no_builtin_tools).then(Vec::new)
            });

            // --api-key binds to the resolved model's provider (main.ts:684-694).
            if let Some(api_key) = &parsed.api_key {
                match &model {
                    Some(model) => auth_storage
                        .set_runtime_api_key(model.provider.to_string(), api_key.clone()),
                    None => diagnostics.push(RuntimeDiagnostic {
                        level: DiagnosticLevel::Error,
                        message: "--api-key requires a model to be specified via --model, --provider/--model, or --models".to_string(),
                    }),
                }
            }

            let cli_thinking_override = parsed.thinking.is_some() || built.cli_thinking_from_model;
            let session = AgentSession::new(AgentSessionConfig {
                session_manager: options.session_manager,
                settings_manager: services.settings_manager.clone(),
                model_registry: services.model_registry.clone(),
                cwd: services.cwd.clone(),
                stream_fn: None,
                model,
                thinking_level,
                scoped_models: built.options.scoped_models.clone(),
                custom_tools: Vec::new(),
                initial_active_tool_names,
                allowed_tool_names,
                excluded_tool_names,
                skills,
                prompt_templates,
                context_files,
                custom_system_prompt: parsed.system_prompt.clone(),
                append_system_prompt: parsed
                    .append_system_prompt
                    .as_ref()
                    .map(|parts| parts.join("\n\n")),
            });
            // Oracle main.ts:723-726: explicit CLI thinking re-applies (and
            // persists) the clamped level.
            if session.model().is_some() && cli_thinking_override {
                session.set_thinking_level(session.thinking_level());
            }

            Ok(CreateRuntimeResult {
                session,
                services,
                diagnostics,
                model_fallback_message,
            })
        })
    })
}

/// Detect discovered extensions and bind the sidecar (decision 7 + Phase 6
/// bind API). `None` when no extensions are discovered or startup fails
/// (warned; the agent continues without extensions).
///
/// `--no-extensions` is NOT checked here: the resource loader already
/// suppresses auto-discovery under `-ne` while keeping explicit `-e` paths
/// (oracle main.ts:665-669 — `additionalExtensionPaths` load regardless of
/// `noExtensions`). `-ne` alone therefore yields zero discovered paths and
/// Bun is never resolved; `-ne -e <path>` still binds the sidecar.
async fn bind_extensions_for_mode(
    runtime: &Arc<AgentSessionRuntime>,
    parsed: &Args,
    app_mode: AppMode,
    cwd: &Path,
    agent_dir: &Path,
) -> Option<Arc<ExtensionBinding>> {
    let extension_paths = {
        let services = runtime.services();
        let loader = services.resource_loader.lock();
        loader.discovered().extension_paths()
    };
    if extension_paths.is_empty() {
        return None;
    }

    let session_dir = runtime.session().with_session_manager(|manager| {
        let dir = manager.get_session_dir();
        if dir.as_os_str().is_empty() {
            pi_coding_agent::config::get_sessions_dir()
        } else {
            dir.to_path_buf()
        }
    });
    let actions = SessionHostActions::new();
    let mut bind_options = BindOptions::new(
        extension_paths,
        LauncherSource::detect(cwd.to_path_buf()),
        cwd.to_path_buf(),
        agent_dir.to_path_buf(),
        session_dir,
        Arc::new(|error: pi_ext_protocol::ExtensionError| {
            eprintln!(
                "\x1b[31mExtension error ({}): {}\x1b[39m",
                error.extension_path, error.error
            );
        }),
        actions.clone(),
    );
    bind_options.runtime = Some(runtime.clone());
    bind_options.mode = match app_mode {
        AppMode::Interactive => pi_ext_protocol::ExtensionMode::Tui,
        AppMode::Rpc => pi_ext_protocol::ExtensionMode::Rpc,
        AppMode::Json => pi_ext_protocol::ExtensionMode::Json,
        AppMode::Print => pi_ext_protocol::ExtensionMode::Print,
    };
    // Only RPC binds a host-side ExtensionUiHost today; other modes keep
    // pi's in-runner no-op UI.
    bind_options.has_ui = app_mode == AppMode::Rpc;
    bind_options.flag_values = parsed
        .unknown_flags
        .iter()
        .map(|(name, value)| {
            let value = match value {
                UnknownFlagValue::Bool(flag) => pi_ext_protocol::FlagValue::Boolean(*flag),
                UnknownFlagValue::Str(text) => pi_ext_protocol::FlagValue::String(text.clone()),
            };
            (name.clone(), value)
        })
        .collect::<BTreeMap<_, _>>();

    let binding = match bind_extensions(&runtime.session(), bind_options) {
        Ok(Some(binding)) => binding,
        Ok(None) => return None,
        Err(error) => {
            eprintln_yellow(&format!("Warning: {error}"));
            eprintln_yellow("Hint: Start without extensions using \"pi -ne\".");
            return None;
        }
    };
    actions.attach(&binding);
    actions.attach_runtime(runtime.clone());
    if let Err(error) = binding.start(SessionStartReason::Startup).await {
        eprintln_yellow(&format!(
            "Warning: extensions disabled — sidecar startup failed: {error}"
        ));
        eprintln_yellow("Hint: Start without extensions using \"pi -ne\".");
        return None;
    }
    Some(binding)
}

#[tokio::main]
async fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();

    // Offline mode env (main.ts:475-479).
    let offline = raw_args.iter().any(|arg| arg == "--offline") || is_truthy_env_flag("PI_OFFLINE");
    if offline {
        // Single-threaded here: before the runtime spawns worker I/O.
        unsafe {
            std::env::set_var("PI_OFFLINE", "1");
            std::env::set_var("PI_SKIP_VERSION_CHECK", "1");
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|error| {
        eprintln_red(&format!("Error: cannot resolve working directory: {error}"));
        std::process::exit(1);
    });
    let cwd_str = cwd.to_string_lossy().into_owned();
    let agent_dir = get_agent_dir();

    // Bootstrap settings: apply httpProxy before any HTTP client exists
    // (main.ts:487-489; reqwest honors these env vars).
    {
        let bootstrap = SettingsManager::create(&cwd, Some(agent_dir.clone()));
        if let Some(proxy) = bootstrap.http_proxy() {
            unsafe {
                std::env::set_var("HTTP_PROXY", proxy);
                std::env::set_var("HTTPS_PROXY", proxy);
            }
        }
    }

    // Package subcommands run before general arg parsing (main.ts:490-505).
    {
        let settings = SettingsManager::create(&cwd, Some(agent_dir.clone()));
        let mut package_manager = DefaultPackageManager::new(&cwd, &agent_dir, settings);
        if let Some(output) = handle_package_command(&raw_args, &mut package_manager) {
            if !output.stdout.is_empty() {
                print!("{}", output.stdout);
                if !output.stdout.ends_with('\n') {
                    println!();
                }
            }
            if !output.stderr.is_empty() {
                eprint!("{}", output.stderr);
                if !output.stderr.ends_with('\n') {
                    eprintln!();
                }
            }
            std::process::exit(output.exit_code);
        }
    }
    // `pi config` runs before general arg parsing (main.ts:504-506).
    if let Some(exit_code) = handle_config_command(&raw_args, &cwd, &agent_dir) {
        std::process::exit(exit_code);
    }

    let mut parsed = cli::parse_args(&raw_args);
    report_arg_diagnostics(&parsed.diagnostics.clone());

    if parsed.version {
        println!("{}", pi_coding_agent::config::VERSION);
        std::process::exit(0);
    }

    // --export <session.jsonl> [output] (main.ts:525-538).
    if let Some(export_input) = &parsed.export {
        let output_path = parsed.messages.first().cloned().map(PathBuf::from);
        match export_session_file_to_html(
            export_input,
            ExportHtmlOptions {
                output_path,
                theme_name: None,
                tool_renderer: None,
            },
        ) {
            Ok(path) => {
                println!("Exported to: {}", path.display());
                std::process::exit(0);
            }
            Err(message) => {
                eprintln_red(&format!("Error: {message}"));
                std::process::exit(1);
            }
        }
    }

    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let app_mode = cli::resolve_app_mode(&parsed, stdin_is_tty, stdout_is_tty);

    if parsed.mode == Some(Mode::Rpc) && !parsed.file_args.is_empty() {
        eprintln_red("Error: @file arguments are not supported in RPC mode");
        std::process::exit(1);
    }

    // Fork/session-id combination validation (main.ts:205-242).
    report_arg_diagnostics(&cli::validate_arg_combinations(&parsed));
    if let Some(session_id) = &parsed.session_id
        && let Err(error) = pi_coding_agent::session_manager::assert_valid_session_id(session_id)
    {
        eprintln_red(&format!("Error: {error}"));
        std::process::exit(1);
    }

    // Startup migrations (idempotent; main.ts:554).
    let migration = run_migrations(&cwd);
    for warning in &migration.deprecation_warnings {
        eprintln_yellow(&format!("Warning: {warning}"));
    }
    for provider in &migration.migrated_auth_providers {
        eprintln_yellow(&format!(
            "Warning: migrated {provider} credentials to auth.json"
        ));
    }

    let startup_settings = Arc::new(Mutex::new(SettingsManager::create(
        &cwd,
        Some(agent_dir.clone()),
    )));

    // First-time setup: theme choice before runtime services exist
    // (main.ts:566-570).
    if app_mode == AppMode::Interactive
        && !parsed.help
        && parsed.list_models.is_none()
        && startup_ui::should_run_first_time_setup(&pi_coding_agent::config::get_settings_path())
    {
        let terminal = pi_tui::terminal::ProcessTerminal::new();
        let mut ui = startup_ui::create_startup_tui(&agent_dir, &startup_settings, terminal);
        startup_ui::show_first_time_setup(&mut ui, &startup_settings);
        ui.stop();
    }

    // Session dir: --session-dir > env > settings (main.ts:577-581).
    let session_dir: Option<PathBuf> = parsed
        .session_dir
        .as_deref()
        .map(normalize_path)
        .or_else(|| {
            std::env::var(env_session_dir_key())
                .ok()
                .map(|value| expand_tilde_path(&value))
        })
        .or_else(|| startup_settings.lock().get_session_dir().map(PathBuf::from));

    let mut session_manager = session_select::create_session_manager(
        &parsed,
        &cwd_str,
        session_dir.clone(),
        &agent_dir,
        &startup_settings,
    );

    // Missing stored session cwd (main.ts:583-596).
    if session_manager.get_session_file().is_some() {
        let session_cwd = session_manager.get_cwd().to_path_buf();
        if !session_cwd.as_os_str().is_empty() && !session_cwd.exists() {
            let message = format!(
                "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
                session_cwd.display(),
                session_manager
                    .get_session_file()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                cwd.display()
            );
            let mut continue_in_cwd = false;
            if app_mode == AppMode::Interactive {
                let terminal = pi_tui::terminal::ProcessTerminal::new();
                let mut ui =
                    startup_ui::create_startup_tui(&agent_dir, &startup_settings, terminal);
                let selected = startup_ui::show_startup_selector(
                    &mut ui,
                    &format!("{message}\n\nContinue in the current directory?"),
                    &["Continue".to_string(), "Cancel".to_string()],
                );
                ui.stop();
                continue_in_cwd = selected == Some(0);
                if !continue_in_cwd {
                    std::process::exit(0);
                }
            }
            if continue_in_cwd {
                let session_file = session_manager
                    .get_session_file()
                    .map(Path::to_path_buf)
                    .expect("session file present");
                session_manager = match pi_coding_agent::session_manager::SessionManager::open(
                    &session_file,
                    session_dir.clone(),
                    Some(&cwd_str),
                ) {
                    Ok(manager) => manager,
                    Err(error) => {
                        eprintln_red(&format!("Error: {error}"));
                        std::process::exit(1);
                    }
                };
            } else {
                eprintln_red(&message);
                std::process::exit(1);
            }
        }
    }

    // --name (main.ts:597-604).
    if let Some(name) = parsed.name.clone() {
        let name = name.trim().to_string();
        if name.is_empty() {
            eprintln_red("Error: --name requires a non-empty value");
            std::process::exit(1);
        }
        if let Err(error) = session_manager.append_session_info(&name) {
            eprintln_red(&format!("Error: {error}"));
            std::process::exit(1);
        }
    }

    // Runtime factory + initial runtime (main.ts:614-745).
    let trust_prompt_interactive =
        app_mode == AppMode::Interactive && !parsed.help && parsed.list_models.is_none();
    let default_project_trust = startup_settings
        .lock()
        .get_default_project_trust()
        .to_string();
    let auth_storage = Arc::new(AuthStorage::new(agent_dir.join("auth.json")));
    let factory_config = Arc::new(FactoryConfig {
        parsed: parsed.clone(),
        default_project_trust,
        interactive_trust_prompt: trust_prompt_interactive,
        startup_settings: startup_settings.clone(),
    });
    let factory = build_runtime_factory(factory_config, auth_storage.clone());
    let runtime = match AgentSessionRuntime::create(
        factory,
        CreateRuntimeOptions {
            cwd: session_manager.get_cwd().to_path_buf(),
            agent_dir: agent_dir.clone(),
            session_manager,
            session_start_reason: SessionStartReason::Startup,
            previous_session_file: None,
        },
        Arc::new(NoopExtensionBridge::default()),
    )
    .await
    {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            eprintln_red(&format!("Error: {error}"));
            std::process::exit(1);
        }
    };

    // Help / --list-models (plan boot order: after runtime creation).
    // Extension CLI flags require the sidecar's registrations; metadata
    // commands never spawn the sidecar (perf gate), so help prints the
    // builtin surface.
    if parsed.help {
        println!("{}", cli::get_help_text(None));
        std::process::exit(0);
    }
    if let Some(list) = &parsed.list_models {
        let services = runtime.services();
        let registry = services.model_registry.read().await;
        let pattern = match list {
            ListModels::All => None,
            ListModels::Search(pattern) => Some(pattern.as_str()),
        };
        model_select::list_models(&registry, pattern).await;
        std::process::exit(0);
    }

    // Piped stdin (never in RPC mode; main.ts:762-768). `resolve_app_mode`
    // already routes non-TTY stdin to print mode.
    let stdin_content = if app_mode == AppMode::Rpc {
        None
    } else {
        initial_message::read_piped_stdin(stdin_is_tty)
    };

    let auto_resize_images = runtime
        .services()
        .settings_manager
        .lock()
        .get_image_auto_resize();
    let initial = initial_message::prepare_initial_message(
        &mut parsed,
        &cwd,
        auto_resize_images,
        stdin_content,
    );

    // Theme init (main.ts:778).
    let theme_name = runtime
        .services()
        .settings_manager
        .lock()
        .get_theme()
        .map(str::to_owned);
    init_theme(theme_name.as_deref(), app_mode == AppMode::Interactive);

    // Runtime diagnostics gate (main.ts:786-795).
    let diagnostics = runtime.diagnostics();
    report_runtime_diagnostics(&diagnostics);
    if diagnostics
        .iter()
        .any(|d| d.level == DiagnosticLevel::Error)
    {
        std::process::exit(1);
    }

    // Non-interactive with no model (main.ts:797-800).
    if app_mode != AppMode::Interactive && runtime.session().model().is_none() {
        eprintln_red(&format_no_models_available_message());
        std::process::exit(1);
    }

    // Extension detection + bind (Phase 6 bind API; zero extensions ⇒ no Bun).
    let binding = bind_extensions_for_mode(&runtime, &parsed, app_mode, &cwd, &agent_dir).await;

    let exit_code = match app_mode {
        AppMode::Rpc => {
            run_rpc_mode(
                runtime.clone(),
                RpcModeOptions {
                    export_html: Some(rpc_export_html_handler(theme_name)),
                },
            )
            .await
        }
        AppMode::Interactive => {
            let terminal = pi_tui::terminal::ProcessTerminal::new();
            let mode = InteractiveMode::new(
                runtime.clone(),
                terminal,
                InteractiveModeOptions {
                    initial_message: initial.initial_message,
                    initial_messages: std::mem::take(&mut parsed.messages),
                    model_fallback_message: runtime.model_fallback_message(),
                    handle_signals: true,
                    ..Default::default()
                },
            );
            let outcome = mode.run().await;
            if let Some(farewell) = outcome.farewell {
                println!("{farewell}");
            }
            outcome.exit_code
        }
        AppMode::Print | AppMode::Json => {
            run_print_mode(
                runtime.clone(),
                PrintModeOptions {
                    mode: if app_mode == AppMode::Json {
                        PrintOutputMode::Json
                    } else {
                        PrintOutputMode::Text
                    },
                    messages: std::mem::take(&mut parsed.messages),
                    initial_message: initial.initial_message,
                    initial_images: initial.initial_images,
                },
            )
            .await
        }
    };

    // Graceful sidecar shutdown before exit.
    if let Some(binding) = binding {
        binding.shutdown().await;
    }
    pi_coding_agent::modes::interactive::theme::watcher::stop_theme_watcher();
    std::process::exit(exit_code);
}
