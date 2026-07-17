//! Resource discovery: settings-backed paths, skills, prompts, themes, extensions.
//!
//! Port of discovery semantics from `resource-loader.ts`, `extensions/loader.ts`,
//! and package-manager local dirs. Extension *loading* is deferred to Phase 6;
//! this module only scans paths so the host can decide whether a sidecar is needed.

use crate::config::{CONFIG_DIR_NAME, get_agent_dir, resolve_path};
use crate::extension_bridge::{DiscoveredExtensions, ExtensionBridge, NoopExtensionBridge};
use crate::settings_manager::SettingsManager;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A lightweight resource path with provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourcePath {
    pub path: PathBuf,
    pub source: ResourceSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceSource {
    Project,
    User,
    Cli,
    Configured,
    PackageManifest,
}

/// Snapshot of discovered resources (paths only).
#[derive(Clone, Debug, Default)]
pub struct DiscoveredResources {
    pub extensions: Vec<ResourcePath>,
    pub skills: Vec<ResourcePath>,
    pub prompts: Vec<ResourcePath>,
    pub themes: Vec<ResourcePath>,
    pub agents_files: Vec<ResourcePath>,
}

impl DiscoveredResources {
    pub fn extension_paths(&self) -> Vec<PathBuf> {
        self.extensions.iter().map(|r| r.path.clone()).collect()
    }

    pub fn needs_sidecar(&self) -> bool {
        !self.extensions.is_empty()
    }
}

/// Options for [`DefaultResourceLoader`].
#[derive(Clone, Debug)]
pub struct ResourceLoaderOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub additional_extension_paths: Vec<String>,
    pub additional_skill_paths: Vec<String>,
    pub additional_prompt_paths: Vec<String>,
    pub additional_theme_paths: Vec<String>,
    pub no_extensions: bool,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_themes: bool,
    pub no_context_files: bool,
}

impl ResourceLoaderOptions {
    pub fn new(cwd: impl AsRef<Path>) -> Self {
        Self {
            cwd: resolve_path(&cwd.as_ref().to_string_lossy(), None),
            agent_dir: get_agent_dir(),
            additional_extension_paths: Vec::new(),
            additional_skill_paths: Vec::new(),
            additional_prompt_paths: Vec::new(),
            additional_theme_paths: Vec::new(),
            no_extensions: false,
            no_skills: false,
            no_prompt_templates: false,
            no_themes: false,
            no_context_files: false,
        }
    }
}

/// Resource loader: discovers paths; does not execute extensions.
pub struct DefaultResourceLoader {
    options: ResourceLoaderOptions,
    project_trusted: bool,
    configured_extensions: Vec<String>,
    configured_skills: Vec<String>,
    configured_prompts: Vec<String>,
    configured_themes: Vec<String>,
    discovered: DiscoveredResources,
    extension_bridge: NoopExtensionBridge,
}

impl DefaultResourceLoader {
    pub fn new(options: ResourceLoaderOptions) -> Self {
        let settings = SettingsManager::create(&options.cwd, Some(options.agent_dir.clone()));
        Self::from_settings(options, &settings)
    }

    pub fn from_settings(options: ResourceLoaderOptions, settings: &SettingsManager) -> Self {
        let mut loader = Self {
            options,
            project_trusted: settings.is_project_trusted(),
            configured_extensions: settings.get_extensions(),
            configured_skills: settings.get_skills(),
            configured_prompts: settings.get_prompts(),
            configured_themes: settings.get_themes(),
            discovered: DiscoveredResources::default(),
            extension_bridge: NoopExtensionBridge::default(),
        };
        loader.rediscover();
        loader
    }

    pub fn set_project_trusted(&mut self, trusted: bool) {
        self.project_trusted = trusted;
    }

    /// Re-scan the filesystem for resources.
    pub fn rediscover(&mut self) {
        let mut discovered = DiscoveredResources::default();
        let cwd = &self.options.cwd;
        let agent_dir = &self.options.agent_dir;
        let project_base = cwd.join(CONFIG_DIR_NAME);

        // Extensions: project-local, global, configured, CLI additional
        if !self.options.no_extensions {
            if self.project_trusted {
                for p in discover_extensions_in_dir(&project_base.join("extensions")) {
                    discovered.extensions.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Project,
                    });
                }
            }
            for p in discover_extensions_in_dir(&agent_dir.join("extensions")) {
                discovered.extensions.push(ResourcePath {
                    path: p,
                    source: ResourceSource::User,
                });
            }
            for raw in &self.configured_extensions {
                for p in resolve_configured_extension(raw, cwd) {
                    discovered.extensions.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Configured,
                    });
                }
            }
        }
        for raw in &self.options.additional_extension_paths {
            for p in resolve_configured_extension(raw, cwd) {
                discovered.extensions.push(ResourcePath {
                    path: p,
                    source: ResourceSource::Cli,
                });
            }
        }

        // Skills
        if !self.options.no_skills {
            if self.project_trusted {
                for p in collect_skill_entries(&project_base.join("skills")) {
                    discovered.skills.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Project,
                    });
                }
            }
            for p in collect_skill_entries(&agent_dir.join("skills")) {
                discovered.skills.push(ResourcePath {
                    path: p,
                    source: ResourceSource::User,
                });
            }
            // ~/.agents/skills and ancestor .agents/skills (project)
            if let Some(home) = dirs::home_dir() {
                for p in collect_skill_entries(&home.join(".agents").join("skills")) {
                    discovered.skills.push(ResourcePath {
                        path: p,
                        source: ResourceSource::User,
                    });
                }
            }
            if self.project_trusted {
                for dir in collect_ancestor_agents_skill_dirs(cwd) {
                    for p in collect_skill_entries(&dir) {
                        discovered.skills.push(ResourcePath {
                            path: p,
                            source: ResourceSource::Project,
                        });
                    }
                }
            }
            for raw in self
                .configured_skills
                .iter()
                .chain(self.options.additional_skill_paths.iter())
            {
                let p = resolve_path(raw, Some(cwd));
                if p.exists() {
                    discovered.skills.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Configured,
                    });
                }
            }
        }

        // Prompt templates
        if !self.options.no_prompt_templates {
            if self.project_trusted {
                for p in collect_files_with_ext(&project_base.join("prompts"), "md") {
                    discovered.prompts.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Project,
                    });
                }
            }
            for p in collect_files_with_ext(&agent_dir.join("prompts"), "md") {
                discovered.prompts.push(ResourcePath {
                    path: p,
                    source: ResourceSource::User,
                });
            }
            for raw in self
                .configured_prompts
                .iter()
                .chain(self.options.additional_prompt_paths.iter())
            {
                let p = resolve_path(raw, Some(cwd));
                if p.exists() {
                    discovered.prompts.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Configured,
                    });
                }
            }
        }

        // Themes
        if !self.options.no_themes {
            if self.project_trusted {
                for p in collect_files_with_ext(&project_base.join("themes"), "json") {
                    discovered.themes.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Project,
                    });
                }
            }
            for p in collect_files_with_ext(&agent_dir.join("themes"), "json") {
                discovered.themes.push(ResourcePath {
                    path: p,
                    source: ResourceSource::User,
                });
            }
            for raw in self
                .configured_themes
                .iter()
                .chain(self.options.additional_theme_paths.iter())
            {
                let p = resolve_path(raw, Some(cwd));
                if p.exists() {
                    discovered.themes.push(ResourcePath {
                        path: p,
                        source: ResourceSource::Configured,
                    });
                }
            }
        }

        // AGENTS.md / CLAUDE.md context files
        if !self.options.no_context_files {
            discovered.agents_files = load_project_context_file_paths(cwd, agent_dir);
        }

        // Dedupe extension paths (first wins)
        discovered.extensions = dedupe_paths(discovered.extensions);
        discovered.skills = dedupe_paths(discovered.skills);
        discovered.prompts = dedupe_paths(discovered.prompts);
        discovered.themes = dedupe_paths(discovered.themes);

        let ext_paths = discovered.extension_paths();
        self.extension_bridge = NoopExtensionBridge::new(ext_paths);
        self.discovered = discovered;
    }

    pub fn discovered(&self) -> &DiscoveredResources {
        &self.discovered
    }

    pub fn needs_sidecar(&self) -> bool {
        self.extension_bridge.needs_sidecar()
    }

    pub fn extension_bridge(&self) -> &dyn ExtensionBridge {
        &self.extension_bridge
    }

    pub fn discovered_extensions(&self) -> DiscoveredExtensions {
        DiscoveredExtensions {
            paths: self.discovered.extension_paths(),
        }
    }
}

fn dedupe_paths(items: Vec<ResourcePath>) -> Vec<ResourcePath> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let key = item.path.to_string_lossy().to_string();
        if seen.insert(key) {
            out.push(item);
        }
    }
    out
}

fn is_extension_file(name: &str) -> bool {
    name.ends_with(".ts") || name.ends_with(".js")
}

/// Discover extensions in a directory (loader.ts `discoverExtensionsInDir`).
pub fn discover_extensions_in_dir(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut discovered = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        let file_type = ent.file_type().ok();
        let is_file = file_type.as_ref().is_some_and(|t| t.is_file());
        let is_dir = file_type.as_ref().is_some_and(|t| t.is_dir());
        // treat symlink as either
        let is_symlink = file_type.as_ref().is_some_and(|t| t.is_symlink());
        if (is_file || is_symlink) && is_extension_file(&name) && path.is_file() {
            discovered.push(path);
            continue;
        }
        if (is_dir || is_symlink)
            && let Some(entries) = resolve_extension_entries(&path)
        {
            discovered.extend(entries);
        }
    }
    discovered
}

/// Resolve extension entry points for a package directory.
pub fn resolve_extension_entries(dir: &Path) -> Option<Vec<PathBuf>> {
    let package_json = dir.join("package.json");
    if package_json.exists()
        && let Ok(text) = fs::read_to_string(&package_json)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(pi) = value.get("pi")
    {
        // pi.extensions array
        if let Some(arr) = pi
            .get("extensions")
            .and_then(|e| e.as_array())
            .or_else(|| pi.as_array())
        {
            let mut out = Vec::new();
            for item in arr {
                if let Some(s) = item.as_str() {
                    let p = dir.join(s);
                    if p.exists() {
                        out.push(p);
                    }
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
        // pi as object with extensions field already handled; if pi is true-ish empty, fall through
    }
    for index in ["index.ts", "index.js"] {
        let p = dir.join(index);
        if p.exists() {
            return Some(vec![p]);
        }
    }
    None
}

fn resolve_configured_extension(raw: &str, cwd: &Path) -> Vec<PathBuf> {
    let resolved = resolve_path(raw, Some(cwd));
    if resolved.is_dir() {
        if let Some(entries) = resolve_extension_entries(&resolved) {
            return entries;
        }
        return discover_extensions_in_dir(&resolved);
    }
    vec![resolved]
}

/// Collect SKILL.md entries under a skills root (one level of package dirs + nested).
pub fn collect_skill_entries(dir: &Path) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // direct SKILL.md
    let direct = dir.join("SKILL.md");
    if direct.is_file() {
        out.push(direct);
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if path.is_dir() {
            let skill = path.join("SKILL.md");
            if skill.is_file() {
                out.push(skill);
            } else {
                // one more level (nested/child-skill)
                if let Ok(inner) = fs::read_dir(&path) {
                    for child in inner.flatten() {
                        let cp = child.path();
                        if cp.is_dir() {
                            let nested = cp.join("SKILL.md");
                            if nested.is_file() {
                                out.push(nested);
                            }
                        } else if child.file_name() == "SKILL.md" {
                            out.push(cp);
                        }
                    }
                }
            }
        } else if ent.file_name() == "SKILL.md" {
            out.push(path);
        }
    }
    out
}

fn collect_files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            out.push(path);
        } else if path.is_dir() {
            out.extend(collect_files_with_ext(&path, ext));
        }
    }
    out
}
// ============================================================================
// Content loading layer (oracle core/skills.ts loadSkillFromFile +
// core/prompt-templates.ts loadTemplateFromFile; frontmatter is real YAML
// via serde_yaml, matching the oracle's `yaml` package)
// ============================================================================

/// Parse a `---` frontmatter block into (key → stringified scalar, body).
/// Mirrors utils/frontmatter.ts: the body is trimmed; a missing or
/// unparseable block yields an empty map with the normalized content.
fn parse_frontmatter(content: &str) -> (std::collections::HashMap<String, String>, String) {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let mut map = std::collections::HashMap::new();
    if !normalized.starts_with("---") {
        return (map, normalized);
    }
    let Some(end) = normalized[3..].find("\n---") else {
        return (map, normalized);
    };
    let yaml_block = &normalized[4.min(3 + end)..3 + end];
    let body = normalized[3 + end + 4..].trim().to_string();
    if let Ok(serde_yaml::Value::Mapping(mapping)) =
        serde_yaml::from_str::<serde_yaml::Value>(yaml_block)
    {
        for (key, value) in mapping {
            let serde_yaml::Value::String(key) = key else {
                continue;
            };
            let value = match value {
                serde_yaml::Value::String(s) => s,
                serde_yaml::Value::Bool(b) => b.to_string(),
                serde_yaml::Value::Number(n) => n.to_string(),
                _ => continue,
            };
            map.insert(key, value);
        }
    }
    (map, body)
}

fn source_scope(source: ResourceSource) -> crate::source_info::SourceScope {
    match source {
        ResourceSource::Project => crate::source_info::SourceScope::Project,
        ResourceSource::User => crate::source_info::SourceScope::User,
        _ => crate::source_info::SourceScope::Temporary,
    }
}

/// Expand discovered resource paths that point at directories (configured/
/// CLI skill roots) into their SKILL.md entries; file paths pass through.
fn expand_skill_paths(discovered: &[ResourcePath]) -> Vec<ResourcePath> {
    let mut out = Vec::new();
    for resource in discovered {
        if resource.path.is_dir() {
            for path in collect_skill_entries(&resource.path) {
                out.push(ResourcePath {
                    path,
                    source: resource.source,
                });
            }
        } else {
            out.push(resource.clone());
        }
    }
    dedupe_paths(out)
}

/// Expand discovered prompt-template paths (directories → contained .md).
fn expand_prompt_paths(discovered: &[ResourcePath]) -> Vec<ResourcePath> {
    let mut out = Vec::new();
    for resource in discovered {
        if resource.path.is_dir() {
            for path in collect_files_with_ext(&resource.path, "md") {
                out.push(ResourcePath {
                    path,
                    source: resource.source,
                });
            }
        } else {
            out.push(resource.clone());
        }
    }
    dedupe_paths(out)
}

/// Load [`Skill`]s from discovered SKILL.md paths. Skills without a
/// description are skipped (oracle skills.ts:305-307); unreadable files are
/// skipped.
pub fn load_skills(discovered: &[ResourcePath]) -> Vec<crate::system_prompt::Skill> {
    let mut skills = Vec::new();
    for resource in &expand_skill_paths(discovered) {
        let Ok(content) = fs::read_to_string(&resource.path) else {
            continue;
        };
        let (frontmatter, _body) = parse_frontmatter(&content);
        let Some(description) = frontmatter
            .get("description")
            .map(|d| d.trim())
            .filter(|d| !d.is_empty())
        else {
            continue;
        };
        let skill_dir = resource
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let parent_dir_name = skill_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = frontmatter
            .get("name")
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or(parent_dir_name);
        skills.push(crate::system_prompt::Skill {
            name,
            description: description.to_string(),
            file_path: resource.path.clone(),
            base_dir: skill_dir.clone(),
            source_info: crate::source_info::SourceInfo::synthetic(
                resource.path.to_string_lossy(),
                "local",
                Some(source_scope(resource.source)),
                None,
                Some(skill_dir.to_string_lossy().into_owned()),
            ),
            disable_model_invocation: frontmatter
                .get("disable-model-invocation")
                .is_some_and(|v| v == "true"),
        });
    }
    skills
}

/// Load [`crate::session::PromptTemplate`]s from discovered prompt paths
/// (oracle prompt-templates.ts `loadTemplateFromFile`).
pub fn load_prompt_templates(discovered: &[ResourcePath]) -> Vec<crate::session::PromptTemplate> {
    let mut templates = Vec::new();
    for resource in &expand_prompt_paths(discovered) {
        let Ok(content) = fs::read_to_string(&resource.path) else {
            continue;
        };
        let (frontmatter, body) = parse_frontmatter(&content);
        let name = resource
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .trim_end_matches(".md")
            .to_string();
        let mut description = frontmatter.get("description").cloned().unwrap_or_default();
        if description.is_empty()
            && let Some(first_line) = body.lines().find(|line| !line.trim().is_empty())
        {
            description = first_line.chars().take(60).collect();
            if first_line.chars().count() > 60 {
                description.push_str("...");
            }
        }
        templates.push(crate::session::PromptTemplate {
            name,
            description,
            argument_hint: frontmatter
                .get("argument-hint")
                .filter(|hint| !hint.is_empty())
                .cloned(),
            content: body,
            file_path: resource.path.clone(),
            source_info: crate::source_info::SourceInfo::synthetic(
                resource.path.to_string_lossy(),
                "local",
                Some(source_scope(resource.source)),
                None,
                resource
                    .path
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned()),
            ),
        });
    }
    templates
}

fn collect_ancestor_agents_skill_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        let candidate = current.join(".agents").join("skills");
        if candidate.exists() {
            dirs.push(candidate);
        }
        if !current.pop() {
            break;
        }
    }
    dirs
}

fn load_context_file_from_dir(dir: &Path) -> Option<PathBuf> {
    for name in ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Discover AGENTS.md / CLAUDE.md from agent dir + cwd ancestors.
pub fn load_project_context_file_paths(cwd: &Path, agent_dir: &Path) -> Vec<ResourcePath> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(p) = load_context_file_from_dir(agent_dir) {
        seen.insert(p.to_string_lossy().to_string());
        out.push(ResourcePath {
            path: p,
            source: ResourceSource::User,
        });
    }
    let mut ancestors = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        if let Some(p) = load_context_file_from_dir(&current) {
            let key = p.to_string_lossy().to_string();
            if !seen.contains(&key) {
                seen.insert(key);
                ancestors.push(ResourcePath {
                    path: p,
                    source: ResourceSource::Project,
                });
            }
        }
        if !current.pop() {
            break;
        }
    }
    ancestors.reverse();
    out.extend(ancestors);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn discovers_project_and_user_extensions() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        let agent = tmp.path().join("agent");
        fs::create_dir_all(cwd.join(".pi/extensions")).unwrap();
        fs::create_dir_all(agent.join("extensions")).unwrap();
        fs::write(cwd.join(".pi/extensions/local.ts"), "export default {}").unwrap();
        fs::write(agent.join("extensions/global.js"), "export default {}").unwrap();

        let mut opts = ResourceLoaderOptions::new(&cwd);
        opts.agent_dir = agent;
        let loader = DefaultResourceLoader::new(opts);
        let paths = loader.discovered().extension_paths();
        assert!(paths.iter().any(|p| p.ends_with("local.ts")));
        assert!(paths.iter().any(|p| p.ends_with("global.js")));
        assert!(loader.needs_sidecar());
    }

    #[test]
    fn discovers_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let skill_dir = agent.join("skills/demo");
        fs::create_dir_all(&skill_dir).unwrap();
        let mut f = fs::File::create(skill_dir.join("SKILL.md")).unwrap();
        writeln!(f, "---\nname: demo\ndescription: d\n---\n").unwrap();
        let mut opts = ResourceLoaderOptions::new(tmp.path());
        opts.agent_dir = agent;
        let loader = DefaultResourceLoader::new(opts);
        assert!(
            loader
                .discovered()
                .skills
                .iter()
                .any(|s| s.path.ends_with("SKILL.md"))
        );
    }
}
