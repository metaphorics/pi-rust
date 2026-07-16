//! pi-coding-agent: CLI host, modes, session manager, settings, tools, extension bridge.
//!
//! Port of packages/coding-agent.

pub mod config;
pub mod extension_bridge;
pub mod migrations;
pub mod resource_loader;
pub mod serde_util;
pub mod session_manager;
pub mod session_types;
pub mod settings_manager;

pub use config::{
    APP_NAME, CONFIG_DIR_NAME, PACKAGE_NAME, encode_session_cwd, env_agent_dir_key,
    env_session_dir_key, get_agent_dir, get_auth_path, get_default_session_dir_path,
    get_package_dir, get_sessions_dir, get_settings_path,
};
pub use extension_bridge::{DiscoveredExtensions, ExtensionBridge, NoopExtensionBridge};
pub use migrations::{MigrationResult, run_migrations, run_migrations_with_agent_dir};
pub use resource_loader::{
    DefaultResourceLoader, DiscoveredResources, ResourceLoaderOptions, ResourcePath,
    ResourceSource, discover_extensions_in_dir,
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

// Keep workspace deps linked while sibling modules land.
#[allow(unused_imports)]
use pi_agent as _;
#[allow(unused_imports)]
use pi_ext_protocol as _;
#[allow(unused_imports)]
use pi_tui as _;
