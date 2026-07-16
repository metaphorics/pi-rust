//! Package installation and resource discovery.
//!
//! This is the Rust port of `core/package-manager.ts`. Package sources are
//! persisted exactly as pi expects while installed content lives under the
//! user or project package roots.

use crate::config::CONFIG_DIR_NAME;
use crate::settings_manager::{SettingsError, SettingsManager};
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use thiserror::Error;

const NETWORK_TIMEOUT_MS: u64 = 10_000;
const RESOURCE_TYPES: [ResourceType; 4] = [
    ResourceType::Extensions,
    ResourceType::Skills,
    ResourceType::Prompts,
    ResourceType::Themes,
];

pub type Result<T, E = PackageManagerError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum PackageManagerError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageScope {
    User,
    Project,
    Temporary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageSource {
    Npm {
        spec: String,
        name: String,
        version: Option<String>,
        range: Option<String>,
        pinned: bool,
    },
    Git {
        repo: String,
        host: String,
        path: String,
        git_ref: Option<String>,
        pinned: bool,
    },
    Local {
        path: String,
    },
}

impl PackageSource {
    pub fn pinned(&self) -> bool {
        matches!(
            self,
            Self::Npm { pinned: true, .. } | Self::Git { pinned: true, .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressType {
    Start,
    Progress,
    Complete,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressAction {
    Install,
    Remove,
    Update,
    Clone,
    Pull,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub r#type: ProgressType,
    pub action: ProgressAction,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathMetadata {
    pub source: String,
    pub scope: PackageScope,
    pub origin: ResourceOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceOrigin {
    Package,
    TopLevel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedResource {
    pub path: PathBuf,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPaths {
    pub extensions: Vec<ResolvedResource>,
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredPackage {
    pub source: String,
    pub scope: PackageScope,
    pub filtered: bool,
    pub installed_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ResourceType {
    fn key(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    fn accepts(self, path: &Path) -> bool {
        match self {
            Self::Extensions => {
                matches!(path.extension().and_then(OsStr::to_str), Some("ts" | "js"))
            }
            Self::Skills | Self::Prompts => path.extension() == Some(OsStr::new("md")),
            Self::Themes => path.extension() == Some(OsStr::new("json")),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct PackageFilter {
    autoload: Option<bool>,
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PackageFilter {
    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            autoload: object.get("autoload").and_then(Value::as_bool),
            extensions: string_array(object.get("extensions")),
            skills: string_array(object.get("skills")),
            prompts: string_array(object.get("prompts")),
            themes: string_array(object.get("themes")),
        })
    }

    fn patterns(&self, resource_type: ResourceType) -> Option<&[String]> {
        match resource_type {
            ResourceType::Extensions => self.extensions.as_deref(),
            ResourceType::Skills => self.skills.as_deref(),
            ResourceType::Prompts => self.prompts.as_deref(),
            ResourceType::Themes => self.themes.as_deref(),
        }
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, command: &str, args: &[String], cwd: Option<&Path>) -> Result<()>;
    fn capture(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> Result<String>;
}

#[derive(Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, command: &str, args: &[String], cwd: Option<&Path>) -> Result<()> {
        let mut child = Command::new(command);
        child.args(args).env("GIT_TERMINAL_PROMPT", "0");
        if let Some(cwd) = cwd {
            child.current_dir(cwd);
        }
        let status = child.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(PackageManagerError::Message(format!(
                "{command} {} failed with code {}",
                args.join(" "),
                status
                    .code()
                    .map_or_else(|| "unknown".into(), |code| code.to_string())
            )))
        }
    }

    fn capture(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> Result<String> {
        let mut command_builder = Command::new(command);
        command_builder
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command_builder.current_dir(cwd);
        }
        let mut child = command_builder.spawn()?;
        let mut stdout = child.stdout.take().expect("piped stdout is available");
        let mut stderr = child.stderr.take().expect("piped stderr is available");
        let stdout_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let stderr_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        });
        let deadline = timeout_ms.map(|timeout| Instant::now() + Duration::from_millis(timeout));
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                child.kill()?;
                let _ = child.wait();
                return Err(PackageManagerError::Message(format!(
                    "{command} {} timed out after {}ms",
                    args.join(" "),
                    timeout_ms.unwrap_or_default()
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = stdout_reader
            .join()
            .map_err(|_| PackageManagerError::Message("stdout reader thread panicked".into()))??;
        let stderr = stderr_reader
            .join()
            .map_err(|_| PackageManagerError::Message("stderr reader thread panicked".into()))??;
        if status.success() {
            Ok(String::from_utf8_lossy(&stdout).trim().to_string())
        } else {
            let message = if stderr.is_empty() { &stdout } else { &stderr };
            Err(PackageManagerError::Message(format!(
                "{command} {} failed with code {}: {}",
                args.join(" "),
                status
                    .code()
                    .map_or_else(|| "unknown".into(), |code| code.to_string()),
                String::from_utf8_lossy(message).trim()
            )))
        }
    }
}

pub struct DefaultPackageManager<R = ProcessCommandRunner> {
    cwd: PathBuf,
    agent_dir: PathBuf,
    settings_manager: SettingsManager,
    runner: R,
    progress_callback: Option<Box<dyn FnMut(ProgressEvent) + Send>>,
}

impl DefaultPackageManager<ProcessCommandRunner> {
    pub fn new(
        cwd: impl AsRef<Path>,
        agent_dir: impl AsRef<Path>,
        settings_manager: SettingsManager,
    ) -> Self {
        Self::with_runner(cwd, agent_dir, settings_manager, ProcessCommandRunner)
    }
}

impl<R: CommandRunner> DefaultPackageManager<R> {
    pub fn with_runner(
        cwd: impl AsRef<Path>,
        agent_dir: impl AsRef<Path>,
        settings_manager: SettingsManager,
        runner: R,
    ) -> Self {
        Self {
            cwd: absolute(cwd.as_ref(), Path::new(".")),
            agent_dir: absolute(agent_dir.as_ref(), Path::new(".")),
            settings_manager,
            runner,
            progress_callback: None,
        }
    }

    pub fn settings_manager(&self) -> &SettingsManager {
        &self.settings_manager
    }

    pub fn settings_manager_mut(&mut self) -> &mut SettingsManager {
        &mut self.settings_manager
    }

    pub fn into_settings_manager(self) -> SettingsManager {
        self.settings_manager
    }

    pub fn set_progress_callback(
        &mut self,
        callback: Option<Box<dyn FnMut(ProgressEvent) + Send>>,
    ) {
        self.progress_callback = callback;
    }

    pub fn parse_source(&self, source: &str) -> PackageSource {
        parse_source(source)
    }

    pub fn add_source_to_settings(&mut self, source: &str, local: bool) -> Result<bool> {
        let scope = if local {
            PackageScope::Project
        } else {
            PackageScope::User
        };
        self.assert_project_trusted(scope)?;
        let current = self.package_entries(scope);
        let normalized = self.normalize_source_for_settings(source, scope);
        let identity = self.source_match_key_input(source);
        let match_index = current.iter().position(|entry| {
            self.source_match_key_settings(package_entry_source(entry), scope) == identity
        });
        let mut next = current;
        if let Some(index) = match_index {
            if package_entry_source(&next[index]) == normalized {
                return Ok(false);
            }
            if let Some(object) = next[index].as_object_mut() {
                object.insert("source".into(), Value::String(normalized));
            } else {
                next[index] = Value::String(normalized);
            }
        } else {
            next.push(Value::String(normalized));
        }
        self.persist_packages(scope, next)?;
        Ok(true)
    }

    pub fn remove_source_from_settings(&mut self, source: &str, local: bool) -> Result<bool> {
        let scope = if local {
            PackageScope::Project
        } else {
            PackageScope::User
        };
        self.assert_project_trusted(scope)?;
        let current = self.package_entries(scope);
        let identity = self.source_match_key_input(source);
        let next: Vec<_> = current
            .iter()
            .filter(|entry| {
                self.source_match_key_settings(package_entry_source(entry), scope) != identity
            })
            .cloned()
            .collect();
        if next.len() == current.len() {
            return Ok(false);
        }
        self.persist_packages(scope, next)?;
        Ok(true)
    }

    pub fn install(&mut self, source: &str, local: bool) -> Result<()> {
        let scope = if local {
            PackageScope::Project
        } else {
            PackageScope::User
        };
        self.assert_project_trusted(scope)?;
        self.with_progress(
            ProgressAction::Install,
            source,
            format!("Installing {source}..."),
            |this| this.install_parsed(&parse_source(source), scope),
        )
    }

    pub fn install_and_persist(&mut self, source: &str, local: bool) -> Result<()> {
        self.install(source, local)?;
        self.add_source_to_settings(source, local)?;
        Ok(())
    }

    pub fn remove(&mut self, source: &str, local: bool) -> Result<()> {
        let scope = if local {
            PackageScope::Project
        } else {
            PackageScope::User
        };
        self.assert_project_trusted(scope)?;
        self.with_progress(
            ProgressAction::Remove,
            source,
            format!("Removing {source}..."),
            |this| match parse_source(source) {
                PackageSource::Npm { name, .. } => this.uninstall_npm(&name, scope),
                PackageSource::Git { host, path, .. } => {
                    let target = this.git_install_path(&host, &path, scope)?;
                    if target.exists() {
                        fs::remove_dir_all(&target)?;
                        this.prune_empty_git_parents(&target, scope)?;
                    }
                    Ok(())
                }
                PackageSource::Local { .. } => Ok(()),
            },
        )
    }

    pub fn remove_and_persist(&mut self, source: &str, local: bool) -> Result<bool> {
        self.remove(source, local)?;
        self.remove_source_from_settings(source, local)
    }

    pub fn update(&mut self, source: Option<&str>) -> Result<()> {
        if offline_mode() {
            return Ok(());
        }
        let wanted = source.map(|source| self.source_match_key_input(source));
        let mut matches = Vec::new();
        for scope in [PackageScope::User, PackageScope::Project] {
            for entry in self.package_entries(scope) {
                let entry_source = package_entry_source(&entry).to_string();
                if wanted.as_ref().is_none_or(|identity| {
                    self.source_match_key_settings(&entry_source, scope) == *identity
                }) {
                    matches.push((entry_source, scope));
                }
            }
        }
        if matches.is_empty()
            && let Some(source) = source
        {
            return Err(PackageManagerError::Message(
                self.no_matching_message(source),
            ));
        }
        for (configured, scope) in matches {
            let parsed = parse_source(&configured);
            if matches!(parsed, PackageSource::Local { .. })
                || matches!(parsed, PackageSource::Npm { pinned: true, .. })
            {
                continue;
            }
            if let PackageSource::Npm {
                spec,
                name,
                version,
                range,
                ..
            } = &parsed
                && !self.should_update_npm(spec, name, version.as_deref(), range.as_deref(), scope)
            {
                continue;
            }
            self.with_progress(
                ProgressAction::Update,
                &configured,
                format!("Updating {configured}..."),
                |this| match &parsed {
                    PackageSource::Npm {
                        spec,
                        name,
                        version,
                        ..
                    } => {
                        let target = if version.is_some() {
                            spec.clone()
                        } else {
                            format!("{name}@latest")
                        };
                        this.install_npm(&target, scope)
                    }
                    PackageSource::Git { .. } => this.install_git(&parsed, scope),
                    PackageSource::Local { .. } => Ok(()),
                },
            )?;
        }
        Ok(())
    }

    pub fn list_configured_packages(&self) -> Vec<ConfiguredPackage> {
        let mut result = Vec::new();
        for scope in [PackageScope::User, PackageScope::Project] {
            for entry in self.package_entries(scope) {
                let source = package_entry_source(&entry).to_string();
                result.push(ConfiguredPackage {
                    installed_path: self.get_installed_path(&source, scope).ok().flatten(),
                    filtered: entry.is_object(),
                    source,
                    scope,
                });
            }
        }
        result
    }

    pub fn get_installed_path(&self, source: &str, scope: PackageScope) -> Result<Option<PathBuf>> {
        self.assert_project_trusted(scope)?;
        let path = match parse_source(source) {
            PackageSource::Npm { name, .. } => self
                .npm_install_root(scope)?
                .join("node_modules")
                .join(name),
            PackageSource::Git { host, path, .. } => self.git_install_path(&host, &path, scope)?,
            PackageSource::Local { path } => {
                resolve_local_source(&path, &self.base_dir(scope), &self.cwd)
            }
        };
        Ok(path.exists().then_some(canonical_or(path)))
    }

    pub fn resolve(&mut self) -> Result<ResolvedPaths> {
        let mut entries = Vec::new();
        for scope in [PackageScope::Project, PackageScope::User] {
            for entry in self.package_entries(scope) {
                entries.push((entry, scope));
            }
        }
        let entries = self.dedupe(entries);
        let mut result = ResolvedPaths::default();
        for (entry, scope) in entries {
            let configured_source = package_entry_source(&entry).to_string();
            let filter = PackageFilter::from_value(&entry);
            let resolved_scope_source = if scope == PackageScope::Project
                && filter.as_ref().and_then(|f| f.autoload) == Some(false)
            {
                self.find_user_delta_base(&configured_source)
                    .unwrap_or_else(|| configured_source.clone())
            } else {
                configured_source.clone()
            };
            let installed = match self.get_installed_path(
                &resolved_scope_source,
                if resolved_scope_source == configured_source {
                    scope
                } else {
                    PackageScope::User
                },
            )? {
                Some(path) => path,
                None => continue,
            };
            let parsed_source = parse_source(&resolved_scope_source);
            let mut metadata = PathMetadata {
                source: configured_source,
                scope,
                origin: ResourceOrigin::Package,
                base_dir: Some(installed.clone()),
            };
            if matches!(parsed_source, PackageSource::Local { .. }) && installed.is_file() {
                metadata.base_dir = installed.parent().map(Path::to_path_buf);
                add_resolved(
                    &mut result,
                    ResourceType::Extensions,
                    ResolvedResource {
                        path: installed,
                        enabled: true,
                        metadata,
                    },
                );
                continue;
            }
            let before = resolved_len(&result);
            self.collect_package_resources(&installed, filter.as_ref(), &metadata, &mut result)?;
            if matches!(parsed_source, PackageSource::Local { .. })
                && installed.is_dir()
                && resolved_len(&result) == before
            {
                add_resolved(
                    &mut result,
                    ResourceType::Extensions,
                    ResolvedResource {
                        path: installed,
                        enabled: true,
                        metadata,
                    },
                );
            }
        }
        sort_and_dedupe_resources(&mut result);
        Ok(result)
    }

    fn find_user_delta_base(&self, project_source: &str) -> Option<String> {
        let identity = self.source_match_key_settings(project_source, PackageScope::Project);
        self.package_entries(PackageScope::User)
            .into_iter()
            .find_map(|entry| {
                let source = package_entry_source(&entry);
                (self.source_match_key_settings(source, PackageScope::User) == identity)
                    .then(|| source.to_string())
            })
    }

    fn dedupe(&self, entries: Vec<(Value, PackageScope)>) -> Vec<(Value, PackageScope)> {
        let mut result: Vec<(Value, PackageScope)> = Vec::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for (entry, scope) in entries {
            let identity = self.source_match_key_settings(package_entry_source(&entry), scope);
            if let Some(&index) = seen.get(&identity) {
                let existing = &result[index];
                if existing.1 == PackageScope::Project && scope == PackageScope::User {
                    if existing.0.get("autoload").and_then(Value::as_bool) == Some(false) {
                        result.push((entry, scope));
                    }
                } else if scope == PackageScope::Project {
                    result[index] = (entry, scope);
                }
            } else {
                seen.insert(identity, result.len());
                result.push((entry, scope));
            }
        }
        result
    }

    fn collect_package_resources(
        &self,
        root: &Path,
        filter: Option<&PackageFilter>,
        metadata: &PathMetadata,
        output: &mut ResolvedPaths,
    ) -> Result<()> {
        let manifest = read_pi_manifest(root);
        let mut found = false;
        for resource_type in RESOURCE_TYPES {
            let manifest_patterns = manifest
                .as_ref()
                .and_then(|m| string_array(m.get(resource_type.key())));
            let all = if let Some(entries) = manifest_patterns.as_ref() {
                collect_manifest_files(root, resource_type, entries)
            } else {
                collect_resource_files(&root.join(resource_type.key()), resource_type)
            };
            if !all.is_empty() {
                found = true;
            }
            let manifest_enabled = manifest_patterns.as_ref().map_or_else(
                || all.iter().cloned().collect::<HashSet<_>>(),
                |patterns| apply_patterns(&all, &override_patterns(patterns), root),
            );
            let states: Vec<(PathBuf, bool)> = match filter {
                Some(filter) if filter.autoload == Some(false) => {
                    let patterns = filter.patterns(resource_type).unwrap_or_default();
                    apply_delta_patterns(&all, patterns, root)
                        .into_iter()
                        .collect()
                }
                Some(filter) if filter.patterns(resource_type).is_some() => {
                    let patterns = filter.patterns(resource_type).unwrap_or_default();
                    let enabled = apply_patterns(&all, patterns, root);
                    all.into_iter()
                        .map(|path| {
                            let is_enabled =
                                enabled.contains(&path) && manifest_enabled.contains(&path);
                            (path, is_enabled)
                        })
                        .collect()
                }
                _ => all
                    .into_iter()
                    .map(|path| {
                        let enabled = manifest_enabled.contains(&path);
                        (path, enabled)
                    })
                    .collect(),
            };
            for (path, enabled) in states {
                add_resolved(
                    output,
                    resource_type,
                    ResolvedResource {
                        path,
                        enabled,
                        metadata: metadata.clone(),
                    },
                );
            }
        }
        if !found && manifest.is_none() {
            for name in ["index.ts", "index.js"] {
                let path = root.join(name);
                if path.is_file() {
                    add_resolved(
                        output,
                        ResourceType::Extensions,
                        ResolvedResource {
                            path,
                            enabled: true,
                            metadata: metadata.clone(),
                        },
                    );
                    break;
                }
            }
        }
        Ok(())
    }

    fn install_parsed(&self, parsed: &PackageSource, scope: PackageScope) -> Result<()> {
        match parsed {
            PackageSource::Npm { spec, .. } => self.install_npm(spec, scope),
            PackageSource::Git { .. } => self.install_git(parsed, scope),
            PackageSource::Local { path } => {
                let resolved = resolve_local_source(path, &self.cwd, &self.cwd);
                if resolved.exists() {
                    Ok(())
                } else {
                    Err(PackageManagerError::Message(format!(
                        "Path does not exist: {}",
                        resolved.display()
                    )))
                }
            }
        }
    }

    fn install_npm(&self, spec: &str, scope: PackageScope) -> Result<()> {
        let root = self.npm_install_root(scope)?;
        ensure_npm_project(&root)?;
        let (command, prefix) = self.npm_command()?;
        let manager = command_name(&command, &prefix);
        let mut args = prefix;
        if manager == "bun" {
            args.extend(strings([
                "install",
                spec,
                "--cwd",
                &root.to_string_lossy(),
                "--omit=peer",
            ]));
        } else if manager == "pnpm" {
            args.extend(strings([
                "install",
                spec,
                "--prefix",
                &root.to_string_lossy(),
                "--config.auto-install-peers=false",
                "--config.strict-peer-dependencies=false",
                "--config.strict-dep-builds=false",
            ]));
        } else {
            args.extend(strings([
                "install",
                spec,
                "--prefix",
                &root.to_string_lossy(),
                "--legacy-peer-deps",
            ]));
        }
        self.runner.run(&command, &args, None)
    }
    fn should_update_npm(
        &self,
        spec: &str,
        name: &str,
        version: Option<&str>,
        range: Option<&str>,
        scope: PackageScope,
    ) -> bool {
        let installed_path = match self.npm_install_root(scope) {
            Ok(root) => root.join("node_modules").join(name),
            Err(_) => return true,
        };
        let installed_version = fs::read_to_string(installed_path.join("package.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("version")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let Some(installed_version) = installed_version else {
            return true;
        };
        let (command, mut args) = match self.npm_command() {
            Ok(command) => command,
            Err(_) => return true,
        };
        args.extend(strings([
            "view",
            if version.is_some() { spec } else { name },
            "version",
            "--json",
        ]));
        let output =
            match self
                .runner
                .capture(&command, &args, Some(&self.cwd), Some(NETWORK_TIMEOUT_MS))
            {
                Ok(output) => output,
                Err(_) => return true,
            };
        let parsed: Value = match serde_json::from_str(output.trim()) {
            Ok(parsed) => parsed,
            Err(_) => return true,
        };
        let target = if let Some(version) = parsed.as_str() {
            Some(version.to_string())
        } else {
            let requirement = range.and_then(|range| semver::VersionReq::parse(range).ok());
            parsed.as_array().and_then(|versions| {
                versions
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|version| {
                        semver::Version::parse(version).ok().filter(|version| {
                            requirement
                                .as_ref()
                                .is_none_or(|requirement| requirement.matches(version))
                        })
                    })
                    .max()
                    .map(|version| version.to_string())
            })
        };
        target.is_none_or(|target| target != installed_version)
    }

    fn uninstall_npm(&self, name: &str, scope: PackageScope) -> Result<()> {
        let root = self.npm_install_root(scope)?;
        if !root.exists() {
            return Ok(());
        }
        let (command, prefix) = self.npm_command()?;
        let manager = command_name(&command, &prefix);
        let mut args = prefix;
        if manager == "bun" {
            args.extend(strings([
                "uninstall",
                name,
                "--cwd",
                &root.to_string_lossy(),
            ]));
        } else {
            args.extend(strings([
                "uninstall",
                name,
                "--prefix",
                &root.to_string_lossy(),
            ]));
            if manager != "pnpm" {
                args.push("--legacy-peer-deps".into());
            }
        }
        self.runner.run(&command, &args, None)
    }

    fn install_git(&self, parsed: &PackageSource, scope: PackageScope) -> Result<()> {
        let PackageSource::Git {
            repo,
            host,
            path,
            git_ref,
            ..
        } = parsed
        else {
            return Ok(());
        };
        let target = self.git_install_path(host, path, scope)?;
        if target.exists() {
            return self.update_git_checkout(&target, git_ref.as_deref());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(root) = self.git_install_root(scope)? {
            ensure_git_ignore(&root)?;
        }
        self.runner.run(
            "git",
            &strings(["clone", repo, &target.to_string_lossy()]),
            None,
        )?;
        if let Some(git_ref) = git_ref {
            self.runner
                .run("git", &strings(["checkout", git_ref]), Some(&target))?;
        }
        self.install_git_dependencies(&target)?;
        Ok(())
    }

    fn update_git_checkout(&self, target: &Path, git_ref: Option<&str>) -> Result<()> {
        let (fetch_args, reset_ref) = if let Some(git_ref) = git_ref {
            (
                strings(["fetch", "origin", git_ref]),
                "FETCH_HEAD".to_string(),
            )
        } else {
            let upstream = self.runner.capture(
                "git",
                &strings([
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ]),
                Some(target),
                Some(NETWORK_TIMEOUT_MS),
            )?;
            let remote = upstream.split('/').next().unwrap_or("origin");
            (
                strings(["fetch", remote, "--prune"]),
                "@{upstream}".to_string(),
            )
        };
        self.runner.run("git", &fetch_args, Some(target))?;
        let local = self.runner.capture(
            "git",
            &strings(["rev-parse", "HEAD"]),
            Some(target),
            Some(NETWORK_TIMEOUT_MS),
        )?;
        let commit_ref = format!("{reset_ref}^{{commit}}");
        let remote = self.runner.capture(
            "git",
            &strings(["rev-parse", &commit_ref]),
            Some(target),
            Some(NETWORK_TIMEOUT_MS),
        )?;
        if local.trim() != remote.trim() {
            self.runner.run(
                "git",
                &strings(["reset", "--hard", &commit_ref]),
                Some(target),
            )?;
            self.runner
                .run("git", &strings(["clean", "-fdx"]), Some(target))?;
            self.install_git_dependencies(target)?;
        }
        Ok(())
    }

    fn install_git_dependencies(&self, target: &Path) -> Result<()> {
        if !target.join("package.json").exists() {
            return Ok(());
        }
        let (command, mut args) = self.npm_command()?;
        args.extend(self.git_dependency_install_args());
        self.runner.run(&command, &args, Some(target))
    }

    fn git_dependency_install_args(&self) -> Vec<String> {
        match self.settings_manager.get_npm_command() {
            Some(parts) if !parts.is_empty() => strings(["install"]),
            _ => strings(["install", "--omit=dev"]),
        }
    }

    fn npm_command(&self) -> Result<(String, Vec<String>)> {
        match self.settings_manager.get_npm_command() {
            None => Ok(("npm".into(), Vec::new())),
            Some(parts) if parts.is_empty() || parts[0].is_empty() => {
                Err(PackageManagerError::Message(
                    "Invalid npmCommand: first array entry must be a non-empty command".into(),
                ))
            }
            Some(parts) => Ok((parts[0].clone(), parts[1..].to_vec())),
        }
    }

    fn npm_install_root(&self, scope: PackageScope) -> Result<PathBuf> {
        self.assert_project_trusted(scope)?;
        Ok(match scope {
            PackageScope::User => self.agent_dir.join("npm"),
            PackageScope::Project => self.cwd.join(CONFIG_DIR_NAME).join("npm"),
            PackageScope::Temporary => self.agent_dir.join("tmp/extensions/npm"),
        })
    }

    fn git_install_root(&self, scope: PackageScope) -> Result<Option<PathBuf>> {
        self.assert_project_trusted(scope)?;
        Ok(match scope {
            PackageScope::User => Some(self.agent_dir.join("git")),
            PackageScope::Project => Some(self.cwd.join(CONFIG_DIR_NAME).join("git")),
            PackageScope::Temporary => None,
        })
    }

    fn git_install_path(&self, host: &str, path: &str, scope: PackageScope) -> Result<PathBuf> {
        let root = self
            .git_install_root(scope)?
            .ok_or_else(|| PackageManagerError::Message("Missing git install root".into()))?;
        managed_path(&root, &[host, path])
    }

    fn prune_empty_git_parents(&self, target: &Path, scope: PackageScope) -> Result<()> {
        let Some(root) = self.git_install_root(scope)? else {
            return Ok(());
        };
        let mut current = target.parent();
        while let Some(dir) = current {
            if dir == root || !dir.starts_with(&root) {
                break;
            }
            if fs::read_dir(dir)?.next().is_some() {
                break;
            }
            fs::remove_dir(dir)?;
            current = dir.parent();
        }
        Ok(())
    }

    fn package_entries(&self, scope: PackageScope) -> Vec<Value> {
        let settings = match scope {
            PackageScope::Project => self.settings_manager.project_settings(),
            _ => self.settings_manager.global_settings(),
        };
        settings
            .get("packages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn persist_packages(&mut self, scope: PackageScope, entries: Vec<Value>) -> Result<()> {
        match scope {
            PackageScope::Project => self.settings_manager.set_project_packages(entries)?,
            PackageScope::User => self.settings_manager.set_packages(entries),
            PackageScope::Temporary => {
                return Err(PackageManagerError::Message(
                    "Cannot persist temporary package source".into(),
                ));
            }
        }
        Ok(())
    }

    fn normalize_source_for_settings(&self, source: &str, scope: PackageScope) -> String {
        match parse_source(source) {
            PackageSource::Local { path } => {
                let resolved = resolve_local_source(&path, &self.cwd, &self.cwd);
                let relative = relative_path(&self.base_dir(scope), &resolved);
                if relative.as_os_str().is_empty() {
                    ".".into()
                } else {
                    relative.to_string_lossy().into_owned()
                }
            }
            _ => source.to_string(),
        }
    }

    fn source_match_key_input(&self, source: &str) -> String {
        source_identity(&parse_source(source), Some((&self.cwd, &self.cwd)))
    }

    fn source_match_key_settings(&self, source: &str, scope: PackageScope) -> String {
        let base = self.base_dir(scope);
        source_identity(&parse_source(source), Some((&base, &self.cwd)))
    }

    fn base_dir(&self, scope: PackageScope) -> PathBuf {
        match scope {
            PackageScope::Project => self.cwd.join(CONFIG_DIR_NAME),
            PackageScope::User => self.agent_dir.clone(),
            PackageScope::Temporary => self.cwd.clone(),
        }
    }

    fn assert_project_trusted(&self, scope: PackageScope) -> Result<()> {
        if scope == PackageScope::Project && !self.settings_manager.is_project_trusted() {
            Err(PackageManagerError::Message(
                "Project is not trusted; refusing to access project package storage".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn no_matching_message(&self, source: &str) -> String {
        let trimmed = source.trim();
        for scope in [PackageScope::User, PackageScope::Project] {
            for entry in self.package_entries(scope) {
                let configured = package_entry_source(&entry);
                match parse_source(configured) {
                    PackageSource::Npm { spec, name, .. } if trimmed == spec || trimmed == name => {
                        return format!(
                            "No matching package found for {source}. Did you mean {configured}?"
                        );
                    }
                    PackageSource::Git {
                        host,
                        path,
                        git_ref,
                        ..
                    } => {
                        let shorthand = format!("{host}/{path}");
                        if trimmed == shorthand
                            || git_ref
                                .as_ref()
                                .is_some_and(|r| trimmed == format!("{shorthand}@{r}"))
                        {
                            return format!(
                                "No matching package found for {source}. Did you mean {configured}?"
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        format!("No matching package found for {source}")
    }

    fn with_progress<T>(
        &mut self,
        action: ProgressAction,
        source: &str,
        message: String,
        operation: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.emit(ProgressEvent {
            r#type: ProgressType::Start,
            action,
            source: source.into(),
            message: Some(message),
        });
        match operation(self) {
            Ok(value) => {
                self.emit(ProgressEvent {
                    r#type: ProgressType::Complete,
                    action,
                    source: source.into(),
                    message: None,
                });
                Ok(value)
            }
            Err(error) => {
                self.emit(ProgressEvent {
                    r#type: ProgressType::Error,
                    action,
                    source: source.into(),
                    message: Some(error.to_string()),
                });
                Err(error)
            }
        }
    }

    fn emit(&mut self, event: ProgressEvent) {
        if let Some(callback) = &mut self.progress_callback {
            callback(event);
        }
    }
}

pub fn parse_source(source: &str) -> PackageSource {
    let source = source.trim();
    if let Some(spec) = source.strip_prefix("npm:") {
        let spec = spec.trim().to_string();
        let (name, version) = parse_npm_spec(&spec);
        let pinned = version.as_deref().is_some_and(is_exact_version);
        let range = version.as_deref().and_then(valid_range).map(str::to_string);
        return PackageSource::Npm {
            spec,
            name,
            version,
            range,
            pinned,
        };
    }
    if is_local_path(source) {
        return PackageSource::Local {
            path: source.to_string(),
        };
    }
    parse_git_source(source).unwrap_or_else(|| PackageSource::Local {
        path: source.to_string(),
    })
}

fn parse_npm_spec(spec: &str) -> (String, Option<String>) {
    let split = if spec.starts_with('@') {
        spec.rfind('@')
            .filter(|index| *index > spec.find('/').unwrap_or(spec.len()))
    } else {
        spec.rfind('@').filter(|index| *index > 0)
    };
    match split {
        Some(index) => (
            spec[..index].to_string(),
            Some(spec[index + 1..].to_string()),
        ),
        None => (spec.to_string(), None),
    }
}

fn is_exact_version(version: &str) -> bool {
    semver::Version::parse(version).is_ok()
}

fn valid_range(version: &str) -> Option<&str> {
    (semver::Version::parse(version).is_ok() || semver::VersionReq::parse(version).is_ok())
        .then_some(version)
}

fn is_local_path(source: &str) -> bool {
    !["npm:", "git:", "github:", "http:", "https:", "ssh:"]
        .iter()
        .any(|prefix| source.starts_with(prefix))
}

fn parse_git_source(source: &str) -> Option<PackageSource> {
    let has_git_prefix = source.starts_with("git:");
    let mut value = if has_git_prefix {
        source[4..].trim()
    } else {
        source
    };
    if !has_git_prefix
        && !["https://", "http://", "ssh://", "git://"]
            .iter()
            .any(|prefix| value.to_ascii_lowercase().starts_with(prefix))
    {
        return None;
    }
    let (repo, git_ref) = split_git_ref(value);
    value = &repo;
    let (host, path, clone_repo) = if let Some(rest) = value.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        (host.to_string(), path.to_string(), value.to_string())
    } else if let Some(protocol) = value.find("://") {
        let rest = &value[protocol + 3..];
        let rest = rest.rsplit_once('@').map_or(rest, |(_, after)| after);
        let (host_port, path) = rest.split_once('/')?;
        let host = host_port.split(':').next().unwrap_or(host_port);
        (host.to_string(), path.to_string(), value.to_string())
    } else {
        let (host, path) = value.split_once('/')?;
        if !host.contains('.') && host != "localhost" {
            return None;
        }
        (
            host.to_string(),
            path.to_string(),
            format!("https://{value}"),
        )
    };
    let path = path
        .trim_start_matches('/')
        .trim_end_matches(".git")
        .to_string();
    if host.is_empty()
        || path.split('/').count() < 2
        || unsafe_git_part(&host, false)
        || unsafe_git_part(&path, true)
    {
        return None;
    }
    Some(PackageSource::Git {
        repo: clone_repo,
        host,
        path,
        pinned: git_ref.is_some(),
        git_ref,
    })
}

fn split_git_ref(value: &str) -> (String, Option<String>) {
    if let Some((repo, git_ref)) = value.rsplit_once('#')
        && !repo.is_empty()
        && !git_ref.is_empty()
    {
        return (repo.to_string(), Some(git_ref.to_string()));
    }
    let path_start = if let Some(rest) = value.strip_prefix("git@") {
        rest.find(':').map(|i| i + "git@".len() + 1)
    } else if let Some(protocol) = value.find("://") {
        value[protocol + 3..].find('/').map(|i| i + protocol + 4)
    } else {
        value.find('/').map(|i| i + 1)
    };
    if let Some(start) = path_start
        && let Some(offset) = value[start..].find('@')
    {
        let index = start + offset;
        if index > start && index + 1 < value.len() {
            return (
                value[..index].to_string(),
                Some(value[index + 1..].to_string()),
            );
        }
    }
    (value.to_string(), None)
}

fn unsafe_git_part(value: &str, allow_slash: bool) -> bool {
    value.contains('\0')
        || value.contains('\\')
        || value.starts_with('/')
        || (!allow_slash && value.contains('/'))
        || value.split('/').any(|part| part == "..")
}

fn source_identity(source: &PackageSource, local_base: Option<(&Path, &Path)>) -> String {
    match source {
        PackageSource::Npm { name, .. } => format!("npm:{name}"),
        PackageSource::Git { host, path, .. } => format!("git:{host}/{path}"),
        PackageSource::Local { path } => {
            let (base, cwd) = local_base.unwrap_or((Path::new("."), Path::new(".")));
            format!("local:{}", resolve_local_source(path, base, cwd).display())
        }
    }
}

pub fn apply_patterns(
    all_paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> HashSet<PathBuf> {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_includes = Vec::new();
    let mut force_excludes = Vec::new();
    for pattern in patterns {
        if let Some(rest) = pattern.strip_prefix('+') {
            force_includes.push(rest);
        } else if let Some(rest) = pattern.strip_prefix('-') {
            force_excludes.push(rest);
        } else if let Some(rest) = pattern.strip_prefix('!') {
            excludes.push(rest);
        } else {
            includes.push(pattern.as_str());
        }
    }
    let mut result: HashSet<PathBuf> = if includes.is_empty() {
        all_paths.iter().cloned().collect()
    } else {
        all_paths
            .iter()
            .filter(|path| matches_patterns(path, &includes, base_dir, false))
            .cloned()
            .collect()
    };
    result.retain(|path| !matches_patterns(path, &excludes, base_dir, false));
    for path in all_paths {
        if matches_patterns(path, &force_includes, base_dir, true) {
            result.insert(path.clone());
        }
    }
    result.retain(|path| !matches_patterns(path, &force_excludes, base_dir, true));
    result
}

fn apply_delta_patterns(
    all_paths: &[PathBuf],
    patterns: &[String],
    base_dir: &Path,
) -> BTreeMap<PathBuf, bool> {
    let mut result = BTreeMap::new();
    for pattern in patterns {
        let (target, enabled, exact) = if let Some(rest) = pattern.strip_prefix('+') {
            (rest, true, true)
        } else if let Some(rest) = pattern.strip_prefix('-') {
            (rest, false, true)
        } else if let Some(rest) = pattern.strip_prefix('!') {
            (rest, false, false)
        } else {
            (pattern.as_str(), true, false)
        };
        for path in all_paths {
            if matches_patterns(path, &[target], base_dir, exact) {
                result.insert(path.clone(), enabled);
            }
        }
    }
    result
}

fn matches_patterns(path: &Path, patterns: &[&str], base_dir: &Path, exact: bool) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let mut candidates = vec![
        path.strip_prefix(base_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/"),
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_string(),
        path.to_string_lossy().replace('\\', "/"),
    ];
    if path.file_name() == Some(OsStr::new("SKILL.md"))
        && let Some(parent) = path.parent()
    {
        candidates.push(
            parent
                .strip_prefix(base_dir)
                .unwrap_or(parent)
                .to_string_lossy()
                .replace('\\', "/"),
        );
        candidates.push(
            parent
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_string(),
        );
        candidates.push(parent.to_string_lossy().replace('\\', "/"));
    }
    patterns.iter().any(|pattern| {
        let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
        if candidates.iter().any(|candidate| candidate == pattern) {
            return true;
        }
        if exact {
            return false;
        }
        let mut builder = GlobSetBuilder::new();
        let Ok(glob) = Glob::new(pattern) else {
            return false;
        };
        builder.add(glob);
        builder
            .build()
            .is_ok_and(|set| candidates.iter().any(|candidate| set.is_match(candidate)))
    })
}

fn read_pi_manifest(root: &Path) -> Option<Map<String, Value>> {
    let content = fs::read_to_string(root.join("package.json")).ok()?;
    serde_json::from_str::<Value>(&content)
        .ok()?
        .get("pi")?
        .as_object()
        .cloned()
}

fn collect_manifest_files(
    root: &Path,
    resource_type: ResourceType,
    entries: &[String],
) -> Vec<PathBuf> {
    let source_entries: Vec<_> = entries.iter().filter(|entry| !is_override(entry)).collect();
    if source_entries.is_empty() {
        return Vec::new();
    }
    let mut selected = HashSet::new();
    for entry in source_entries {
        if has_glob(entry) {
            selected.extend(collect_glob_resource_files(root, resource_type, entry));
        } else {
            let path = root.join(entry);
            if path.is_dir() {
                selected.extend(collect_resource_files(&path, resource_type));
            } else if path.is_file() && resource_type.accepts(&path) {
                selected.insert(canonical_or(path));
            }
        }
    }
    let mut selected: Vec<_> = selected.into_iter().collect();
    selected.sort();
    selected
}

fn collect_resource_files(root: &Path, resource_type: ResourceType) -> Vec<PathBuf> {
    match resource_type {
        ResourceType::Extensions => collect_extension_entries(root),
        ResourceType::Skills => collect_skill_entries(root),
        ResourceType::Prompts | ResourceType::Themes => {
            collect_recursive_resource_files(root, resource_type)
        }
    }
}

fn collect_extension_entries(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    if root.is_file() {
        return ResourceType::Extensions
            .accepts(root)
            .then(|| canonical_or(root.to_path_buf()))
            .into_iter()
            .collect();
    }
    if let Some(entries) = resolve_extension_entries(root) {
        return entries;
    }

    let mut paths = Vec::new();
    for entry in WalkBuilder::new(root)
        .max_depth(Some(1))
        .hidden(true)
        .git_ignore(true)
        .ignore(true)
        .parents(false)
        .require_git(false)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != OsStr::new("node_modules"))
        .build()
        .filter_map(std::result::Result::ok)
        .skip(1)
    {
        let path = entry.path();
        if path.is_file() && ResourceType::Extensions.accepts(path) {
            paths.push(canonical_or(path.to_path_buf()));
        } else if path.is_dir()
            && let Some(entries) = resolve_extension_entries(path)
        {
            paths.extend(entries);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn resolve_extension_entries(root: &Path) -> Option<Vec<PathBuf>> {
    if let Some(entries) = read_pi_manifest(root)
        .as_ref()
        .and_then(|manifest| string_array(manifest.get(ResourceType::Extensions.key())))
    {
        let resolved = entries
            .into_iter()
            .map(|entry| root.join(entry))
            .filter(|path| path.exists())
            .map(canonical_or)
            .collect::<Vec<_>>();
        if !resolved.is_empty() {
            return Some(resolved);
        }
    }
    for name in ["index.ts", "index.js"] {
        let path = root.join(name);
        if path.is_file() {
            return Some(vec![canonical_or(path)]);
        }
    }
    None
}

fn collect_skill_entries(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    if root.is_file() {
        return (root.file_name() == Some(OsStr::new("SKILL.md"))
            || root.extension() == Some(OsStr::new("md")))
        .then(|| canonical_or(root.to_path_buf()))
        .into_iter()
        .collect();
    }

    let canonical_root = canonical_or(root.to_path_buf());
    let mut root_markdown = Vec::new();
    let mut skill_paths = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .ignore(true)
        .parents(false)
        .require_git(false)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != OsStr::new("node_modules"))
        .build()
        .filter_map(std::result::Result::ok)
        .skip(1)
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let path = canonical_or(path.to_path_buf());
        if entry.depth() == 1 && path.extension() == Some(OsStr::new("md")) {
            root_markdown.push(path.clone());
        }
        if path.file_name() == Some(OsStr::new("SKILL.md")) {
            skill_paths.push(path);
        }
    }

    let root_skill = canonical_root.join("SKILL.md");
    if skill_paths.contains(&root_skill) {
        return vec![root_skill];
    }
    let visible_skills = skill_paths.iter().cloned().collect::<HashSet<_>>();
    for path in skill_paths {
        let shadowed = path
            .parent()
            .and_then(Path::parent)
            .into_iter()
            .flat_map(|ancestor| ancestor.ancestors())
            .take_while(|ancestor| *ancestor != canonical_root)
            .any(|ancestor| visible_skills.contains(&ancestor.join("SKILL.md")));
        if !shadowed {
            root_markdown.push(path);
        }
    }
    root_markdown.sort();
    root_markdown.dedup();
    root_markdown
}

fn collect_glob_resource_files(
    root: &Path,
    resource_type: ResourceType,
    pattern: &str,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .ignore(true)
        .parents(false)
        .require_git(false)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != OsStr::new("node_modules"))
        .build()
        .filter_map(std::result::Result::ok)
        .skip(1)
    {
        let path = entry.path();
        if !matches_patterns(path, &[pattern], root, false) {
            continue;
        }
        if path.is_dir() {
            paths.extend(collect_resource_files(path, resource_type));
        } else if path.is_file() && resource_type.accepts(path) {
            paths.push(canonical_or(path.to_path_buf()));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn collect_recursive_resource_files(root: &Path, resource_type: ResourceType) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    if root.is_file() {
        return resource_type
            .accepts(root)
            .then(|| canonical_or(root.to_path_buf()))
            .into_iter()
            .collect();
    }
    let mut paths = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .ignore(true)
        .parents(false)
        .require_git(false)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != OsStr::new("node_modules"))
        .build()
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        if path.is_file() && resource_type.accepts(path) {
            paths.push(canonical_or(path.to_path_buf()));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn override_patterns(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| is_override(entry))
        .cloned()
        .collect()
}

fn is_override(entry: &str) -> bool {
    entry.starts_with(['!', '+', '-'])
}

fn has_glob(entry: &str) -> bool {
    entry.contains(['*', '?', '[', '{'])
}

fn resolved_len(output: &ResolvedPaths) -> usize {
    output.extensions.len() + output.skills.len() + output.prompts.len() + output.themes.len()
}

fn add_resolved(
    output: &mut ResolvedPaths,
    resource_type: ResourceType,
    resource: ResolvedResource,
) {
    match resource_type {
        ResourceType::Extensions => output.extensions.push(resource),
        ResourceType::Skills => output.skills.push(resource),
        ResourceType::Prompts => output.prompts.push(resource),
        ResourceType::Themes => output.themes.push(resource),
    }
}

fn sort_and_dedupe_resources(output: &mut ResolvedPaths) {
    for resources in [
        &mut output.extensions,
        &mut output.skills,
        &mut output.prompts,
        &mut output.themes,
    ] {
        resources.sort_by_key(|resource| resource_rank(&resource.metadata));
        let mut seen = HashSet::new();
        resources.retain(|resource| seen.insert(canonical_or(resource.path.clone())));
    }
}

fn resource_rank(metadata: &PathMetadata) -> u8 {
    if metadata.origin == ResourceOrigin::Package {
        return 4;
    }
    let base = if metadata.scope == PackageScope::Project {
        0
    } else {
        2
    };
    base + u8::from(metadata.source != "local")
}

fn package_entry_source(entry: &Value) -> &str {
    entry
        .as_str()
        .or_else(|| entry.get("source").and_then(Value::as_str))
        .unwrap_or_default()
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    value?.as_array().map(|array| {
        array
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn offline_mode() -> bool {
    std::env::var("PI_OFFLINE")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn command_name(command: &str, prefix: &[String]) -> String {
    let parts: Vec<_> = std::iter::once(command)
        .chain(prefix.iter().map(String::as_str))
        .collect();
    let selected = parts
        .iter()
        .rposition(|part| *part == "--")
        .and_then(|i| parts.get(i + 1))
        .copied()
        .unwrap_or(command);
    Path::new(selected)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .trim_end_matches(".cmd")
        .trim_end_matches(".exe")
        .to_ascii_lowercase()
}

fn ensure_npm_project(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    ensure_git_ignore(root)?;
    let package_json = root.join("package.json");
    if !package_json.exists() {
        fs::write(
            package_json,
            serde_json::to_string_pretty(&json!({ "name": "pi-extensions", "private": true }))?,
        )?;
    }
    Ok(())
}

fn ensure_git_ignore(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    let path = root.join(".gitignore");
    if !path.exists() {
        fs::write(path, "*\n!.gitignore\n")?;
    }
    Ok(())
}

fn managed_path(root: &Path, parts: &[&str]) -> Result<PathBuf> {
    let root = absolute(root, Path::new("."));
    let mut path = root.clone();
    for part in parts {
        path.push(part);
    }
    let path = normalize_lexically(&path);
    if path != root && !path.starts_with(&root) {
        return Err(PackageManagerError::Message(format!(
            "Refusing to use path outside package install root: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn resolve_local_source(path: &str, base: &Path, cwd: &Path) -> PathBuf {
    let path = if path.starts_with("file:") {
        url::Url::parse(path)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .unwrap_or_else(|| PathBuf::from(path))
    } else if path == "~" || path.starts_with("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| cwd.to_path_buf())
            .join(path.trim_start_matches("~/"))
    } else {
        PathBuf::from(path)
    };
    canonical_or(absolute(&path, base))
}

fn absolute(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_lexically(path)
    } else {
        normalize_lexically(&base.join(path))
    }
}

fn canonical_or(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from = normalize_lexically(from);
    let to = normalize_lexically(to);
    let from_parts: Vec<_> = from.components().collect();
    let to_parts: Vec<_> = to.components().collect();
    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
    let mut result = PathBuf::new();
    for _ in common..from_parts.len() {
        result.push("..");
    }
    for part in &to_parts[common..] {
        result.push(part.as_os_str());
    }
    result
}

fn strings<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.into_iter().map(str::to_string).collect()
}
