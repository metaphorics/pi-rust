//! pi-coding-agent: CLI host, modes, session manager, settings, tools, extension bridge.
//!
//! Port of packages/coding-agent.

pub mod config;
pub mod cli;
pub mod source_info;
pub mod extension_bridge;
pub mod export_html;
pub mod migrations;
pub mod resource_loader;
pub mod serde_util;
pub mod session_manager;
pub mod session_types;
pub mod settings_manager;
pub mod auth_storage;
pub mod model_registry;
pub mod resolve_config_value;
pub mod session;
pub mod system_prompt;
pub mod wire_out;
pub mod package_manager;
pub mod package_manager_cli;

pub use auth_storage::{AuthStorage, AuthStatus, AuthStorageError};
pub use model_registry::{ModelRegistry, ResolvedRequestAuth, ResolveResult, ProviderConfigInput, ModelDefinition, ModelOverride, OverrideCost, DefinitionCost};
pub use resolve_config_value::clear_config_value_cache;

pub use cli::{
    args, parse_args, get_help_text, validate_arg_combinations, Args, Mode, AppMode, ThinkingLevel, ListModels,
    UnknownFlagValue, Diagnostic, DiagnosticType, ExtensionFlag, resolve_app_mode,
    to_print_output_mode, is_plain_runtime_metadata_command,
};
pub use config::{
    APP_NAME, CONFIG_DIR_NAME, PACKAGE_NAME, encode_session_cwd, env_agent_dir_key,
    env_session_dir_key, get_agent_dir, get_auth_path, get_default_session_dir_path,
    get_package_dir, get_sessions_dir, get_settings_path,
};
pub use extension_bridge::{
    DiscoveredExtensions, ExtensionBridge, ExtensionUiHost, ForkPosition, HookOutcome,
    NoopExtensionBridge, NotifyType, RegisteredCommand, SessionLifecycleEvent,
    SessionShutdownReason, SessionStartReason, UiDialogOptions, WidgetPlacement,
};
pub use export_html::{
    ExportHtmlOptions, RenderedToolResult, ToolHtmlRenderer, export_session_file_to_html,
    export_session_to_html, rpc_export_html_handler,
};
pub use source_info::{SourceInfo, SourceOrigin, SourceScope};
pub use migrations::{MigrationResult, run_migrations, run_migrations_with_agent_dir};
pub use resource_loader::{
    DefaultResourceLoader, DiscoveredResources, ResourceLoaderOptions, ResourcePath,
    ResourceSource, discover_extensions_in_dir,
};
pub use package_manager::{
    CommandRunner, ConfiguredPackage, DefaultPackageManager, PackageManagerError, PackageScope,
    PackageSource, PathMetadata, ProcessCommandRunner, ProgressAction, ProgressEvent, ProgressType,
    ResolvedPaths, ResolvedResource, ResourceOrigin, ResourceType, apply_patterns, parse_source,
};
pub use package_manager_cli::{
    InstallMethod, LatestRelease, PackageCommand, PackageCommandOptions, PackageCommandOutput,
    ProcessSelfUpdater, SelfUpdateOutcome, SelfUpdater, UpdateTarget, detect_install_method,
    get_package_command_help, get_package_command_usage, handle_package_command,
    handle_package_command_with_self_updater, parse_package_command,
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
pub use wire_out::WireOut;
pub use system_prompt::{
    BuildSystemPromptOptions, ContextFile, Skill, build_system_prompt, format_skills_for_prompt,
    get_docs_path, get_examples_path, get_readme_path, load_project_context_files,
};
pub use session::{
    AgentSession, AgentSessionConfig, AgentSessionEvent, AgentSessionEventListener, BashResult,
    CompactionReason, CompactionResult, ContextUsage, CustomMessageDelivery, ModelCycleResult,
    NavigateTreeOptions, NavigateTreeResult, PromptOptions, PromptTemplate, ScopedModel,
    SendCustomMessageOptions, SessionStats, SessionToolDefinition, StreamingBehavior, ToolInfo,
    convert_to_llm, format_no_api_key_found_message, format_no_model_selected_message,
    format_no_models_available_message,
};
pub use session::runtime::{
    AgentSessionRuntime, CreateRuntimeFactory, CreateRuntimeOptions, CreateRuntimeResult,
    ReplaceResult,
};
pub use session::services::{
    AgentSessionServices, CreateAgentSessionServicesOptions, DiagnosticLevel, RuntimeDiagnostic,
    create_agent_session_services,
};

pub mod modes;
pub mod tools;
