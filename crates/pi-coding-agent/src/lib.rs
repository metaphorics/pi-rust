//! pi-coding-agent: CLI host, modes, session manager, settings, tools, extension bridge.
//!
//! Port of packages/coding-agent.

pub mod auth_storage;
pub mod cli;
pub mod config;
pub mod export_html;
pub mod extension_bridge;
pub mod extensions;
pub mod migrations;
pub mod model_registry;
pub mod package_manager;
pub mod package_manager_cli;
pub mod resolve_config_value;
pub mod resource_loader;
pub mod serde_util;
pub mod session;
pub mod session_manager;
pub mod session_types;
pub mod settings_manager;
pub mod source_info;
pub mod system_prompt;
pub mod wire_out;

pub use auth_storage::{AuthStatus, AuthStorage, AuthStorageError};
pub use model_registry::{
    DefinitionCost, ModelDefinition, ModelOverride, ModelRegistry, OverrideCost,
    ProviderConfigInput, ResolveResult, ResolvedRequestAuth,
};
pub use resolve_config_value::clear_config_value_cache;

pub use cli::{
    AppMode, Args, Diagnostic, DiagnosticType, ExtensionFlag, ListModels, Mode, ThinkingLevel,
    UnknownFlagValue, args, get_help_text, is_plain_runtime_metadata_command, parse_args,
    resolve_app_mode, to_print_output_mode, validate_arg_combinations,
};
pub use config::{
    APP_NAME, CONFIG_DIR_NAME, PACKAGE_NAME, encode_session_cwd, env_agent_dir_key,
    env_session_dir_key, get_agent_dir, get_auth_path, get_default_session_dir_path,
    get_package_dir, get_sessions_dir, get_settings_path,
};
pub use export_html::{
    ExportHtmlOptions, RenderedToolResult, ToolHtmlRenderer, export_session_file_to_html,
    export_session_to_html, rpc_export_html_handler,
};
pub use extension_bridge::{
    BeforeCompactDecision, CompactHooks, CompactionOverride, DiscoveredExtensions, ExtensionBridge,
    ExtensionUiHost, ForkPosition, HookOutcome, NoopExtensionBridge, NotifyType, RegisteredCommand,
    SessionLifecycleEvent, SessionShutdownReason, SessionStartReason, UiDialogOptions,
    WidgetPlacement,
};
pub use extensions::actions::{ActionServerConfig, HostActions, NotificationSink};
pub use extensions::binding::{
    BindOptions, ExtensionBinding, SessionHostActions, SidecarBridge, bind_extensions,
};
pub use extensions::events::{
    DEFAULT_HOOK_TIMEOUT, EmitError, EventForwarder, ExtensionErrorSink, StateOverlay, StateSource,
    agent_thinking_level, session_state_block, wire_thinking_level,
};
pub use extensions::session_sync::{SessionSync, session_file_string};
pub use extensions::{
    BridgeState, BunEnvironment, ClientConfig, ClientError, DeadReason, DisabledReason,
    ExtensionHost, ExtensionHostConfig, ExtensionPathError, HostError, Incoming, LauncherSource,
    LoadOutcome, SidecarConnection, SidecarLauncher, SidecarTimeouts,
};
pub use migrations::{MigrationResult, run_migrations, run_migrations_with_agent_dir};
pub use package_manager::{
    CommandRunner, ConfiguredPackage, DefaultPackageManager, PackageManagerError, PackageScope,
    PackageSource, PathMetadata, ProcessCommandRunner, ProgressAction, ProgressEvent, ProgressType,
    ResolvedPaths, ResolvedResource, ResourceOrigin, ResourceType, apply_patterns, parse_source,
};
pub use package_manager_cli::{
    ConfigCommandOptions, InstallMethod, LatestRelease, PackageCommand, PackageCommandOptions,
    PackageCommandOutput, ProcessSelfUpdater, SelfUpdateOutcome, SelfUpdater, UpdateTarget,
    detect_install_method, get_config_command_help, get_config_command_usage,
    get_package_command_help, get_package_command_usage, handle_package_command,
    handle_package_command_with_self_updater, parse_config_command, parse_package_command,
};
pub use resource_loader::{
    DefaultResourceLoader, DiscoveredResources, ResourceLoaderOptions, ResourcePath,
    ResourceSource, discover_extensions_in_dir,
};
pub use session::runtime::{
    AgentSessionRuntime, CreateRuntimeFactory, CreateRuntimeOptions, CreateRuntimeResult,
    ReplaceResult,
};
pub use session::services::{
    AgentSessionServices, CreateAgentSessionServicesOptions, DiagnosticLevel, RuntimeDiagnostic,
    create_agent_session_services,
};
pub use session::{
    AgentSession, AgentSessionConfig, AgentSessionEvent, AgentSessionEventListener, BashResult,
    CompactionReason, CompactionResult, ContextUsage, CustomMessageDelivery, ModelCycleResult,
    NavigateTreeOptions, NavigateTreeResult, PromptOptions, PromptTemplate, ScopedModel,
    SendCustomMessageOptions, SessionStats, SessionToolDefinition, StreamingBehavior, ToolInfo,
    convert_to_llm, format_no_api_key_found_message, format_no_model_selected_message,
    format_no_models_available_message,
};
pub use session_manager::{
    NewSessionOptions, ResolvedSession, SessionContext, SessionError, SessionInfo, SessionManager,
    SessionModelRef, SessionTreeNode, assert_valid_session_id, build_context_entries,
    build_session_context, find_most_recent_session, load_entries_from_file,
    migrate_session_entries, migrate_to_current_version, resolve_session_path,
};
pub use session_types::{
    CURRENT_SESSION_VERSION, FileEntry, SessionEntry, SessionHeader, parse_session_entries,
    parse_session_entry_line, serialize_file_entry_line, serialize_session_jsonl,
};
pub use settings_manager::{
    Settings, SettingsManager, SettingsScope, deep_merge_settings, migrate_settings,
    parse_settings_json, serialize_settings_json,
};
pub use source_info::{SourceInfo, SourceOrigin, SourceScope};
pub use system_prompt::{
    BuildSystemPromptOptions, ContextFile, Skill, build_system_prompt, format_skills_for_prompt,
    get_docs_path, get_examples_path, get_readme_path, load_project_context_files,
};
pub use wire_out::WireOut;

pub mod modes;
pub mod tools;
