//! TUI component for managing package resources (enable/disable).
//!
//! Port of `modes/interactive/components/config-selector.ts`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use pi_tui::components::Input;
use pi_tui::keybindings::get_keybindings;
use pi_tui::keys::matches_key;
use pi_tui::util::{truncate_to_width, visible_width};
use pi_tui::{Component, Focusable, Line, RenderStatus};

use super::dynamic_border::DynamicBorder;
use super::keybinding_hints::{key_hint, raw_key_hint};
use crate::config::{CONFIG_DIR_NAME, resolve_path};
use crate::modes::interactive::theme::{ThemeColor, theme};
use crate::package_manager::{PackageScope, PathMetadata, ResolvedPaths, ResourceOrigin};
use crate::settings_manager::SettingsManager;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ResourceType {
    const ALL: [ResourceType; 4] = [
        ResourceType::Extensions,
        ResourceType::Skills,
        ResourceType::Prompts,
        ResourceType::Themes,
    ];

    fn as_str(self) -> &'static str {
        match self {
            ResourceType::Extensions => "extensions",
            ResourceType::Skills => "skills",
            ResourceType::Prompts => "prompts",
            ResourceType::Themes => "themes",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ResourceType::Extensions => "Extensions",
            ResourceType::Skills => "Skills",
            ResourceType::Prompts => "Prompts",
            ResourceType::Themes => "Themes",
        }
    }

    fn order(self) -> usize {
        match self {
            ResourceType::Extensions => 0,
            ResourceType::Skills => 1,
            ResourceType::Prompts => 2,
            ResourceType::Themes => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigWriteScope {
    Global,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsScope {
    User,
    Project,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectOverrideState {
    Inherit,
    Load,
    Unload,
}

/// Oracle `ScopedResolvedPaths` (`Record<ConfigWriteScope, ResolvedPaths>`).
pub struct ScopedResolvedPaths {
    pub global: ResolvedPaths,
    pub project: ResolvedPaths,
}

fn scope_str(scope: PackageScope) -> &'static str {
    match scope {
        PackageScope::User => "user",
        PackageScope::Project => "project",
        PackageScope::Temporary => "temporary",
    }
}

fn origin_str(origin: ResourceOrigin) -> &'static str {
    match origin {
        ResourceOrigin::Package => "package",
        ResourceOrigin::TopLevel => "top-level",
    }
}

/// One resource row.
#[derive(Clone, Debug)]
pub struct ResourceItem {
    pub path: String,
    pub enabled: bool,
    pub metadata: PathMetadata,
    pub resource_type: ResourceType,
    pub display_name: String,
    pub group_key: String,
    pub subgroup_key: String,
}

#[derive(Clone, Debug)]
pub struct ResourceSubgroup {
    pub resource_type: ResourceType,
    pub label: &'static str,
    pub items: Vec<ResourceItem>,
}

#[derive(Clone, Debug)]
pub struct ResourceGroup {
    pub key: String,
    pub label: String,
    pub scope: PackageScope,
    pub origin: ResourceOrigin,
    pub source: String,
    pub subgroups: Vec<ResourceSubgroup>,
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn dirname(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(0) => "/",
        Some(idx) => &path[..idx],
        None => ".",
    }
}

/// Node `path.relative(from, to)` for already-absolute inputs (lexical).
fn relative_string(from: &Path, to: &Path) -> String {
    use std::path::Component;
    let normalize = |p: &Path| -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        for component in p.components() {
            match component {
                Component::ParentDir => {
                    parts.pop();
                }
                Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
                Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            }
        }
        parts
    };
    let from_parts = normalize(from);
    let to_parts = normalize(to);
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut result: Vec<String> = Vec::new();
    for _ in common..from_parts.len() {
        result.push("..".to_owned());
    }
    for part in &to_parts[common..] {
        result.push(part.clone());
    }
    result.join("/")
}

/// Oracle `canonicalizePath` (realpath with fallback to input).
fn canonicalize_path(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// Oracle `isLocalPath` (utils/paths.ts).
fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !["npm:", "git:", "github:", "http:", "https:", "ssh:"]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn format_base_dir(base_dir: &Path) -> String {
    let home_dir = dirs::home_dir().unwrap_or_default();
    let base = base_dir.to_string_lossy();
    let home = home_dir.to_string_lossy();
    let display_path = if !home.is_empty() && base == home {
        "~".to_owned()
    } else if !home.is_empty() && base.starts_with(home.as_ref()) {
        let rest = &base[home.len()..];
        format!("~{}", rest.replace('\\', "/"))
    } else {
        base.replace('\\', "/")
    };
    if display_path.ends_with('/') {
        display_path
    } else {
        format!("{display_path}/")
    }
}

fn get_group_label(metadata: &PathMetadata, agent_dir: &Path) -> String {
    if metadata.origin == ResourceOrigin::Package {
        return format!("{} ({})", metadata.source, scope_str(metadata.scope));
    }
    // Top-level resources
    if metadata.source == "auto" {
        if let Some(base_dir) = &metadata.base_dir {
            return if metadata.scope == PackageScope::User {
                format!("User ({})", format_base_dir(base_dir))
            } else {
                format!("Project ({})", format_base_dir(base_dir))
            };
        }
        return if metadata.scope == PackageScope::User {
            format!("User ({})", format_base_dir(agent_dir))
        } else {
            format!("Project ({CONFIG_DIR_NAME}/)")
        };
    }
    if metadata.scope == PackageScope::User {
        "User settings".to_owned()
    } else {
        "Project settings".to_owned()
    }
}

/// Oracle `buildGroups`.
fn build_groups(resolved: &ResolvedPaths, agent_dir: &Path) -> Vec<ResourceGroup> {
    let mut groups: Vec<ResourceGroup> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();

    let add_to_group = |groups: &mut Vec<ResourceGroup>,
                        group_index: &mut HashMap<String, usize>,
                        resources: &[crate::package_manager::ResolvedResource],
                        resource_type: ResourceType| {
        for res in resources {
            let path = res.path.to_string_lossy().into_owned();
            let metadata = &res.metadata;
            let group_key = format!(
                "{}:{}:{}:{}",
                origin_str(metadata.origin),
                scope_str(metadata.scope),
                metadata.source,
                metadata
                    .base_dir
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            );

            let group_idx = *group_index.entry(group_key.clone()).or_insert_with(|| {
                groups.push(ResourceGroup {
                    key: group_key.clone(),
                    label: get_group_label(metadata, agent_dir),
                    scope: metadata.scope,
                    origin: metadata.origin,
                    source: metadata.source.clone(),
                    subgroups: Vec::new(),
                });
                groups.len() - 1
            });

            let group = &mut groups[group_idx];
            let subgroup_key = format!("{group_key}:{}", resource_type.as_str());

            let subgroup_idx = match group
                .subgroups
                .iter()
                .position(|sg| sg.resource_type == resource_type)
            {
                Some(idx) => idx,
                None => {
                    group.subgroups.push(ResourceSubgroup {
                        resource_type,
                        label: resource_type.label(),
                        items: Vec::new(),
                    });
                    group.subgroups.len() - 1
                }
            };

            let file_name = basename(&path);
            let parent_folder = basename(dirname(&path));
            let display_name =
                if resource_type == ResourceType::Extensions && parent_folder != "extensions" {
                    format!("{parent_folder}/{file_name}")
                } else if resource_type == ResourceType::Skills && file_name == "SKILL.md" {
                    parent_folder.to_owned()
                } else {
                    file_name.to_owned()
                };
            group.subgroups[subgroup_idx].items.push(ResourceItem {
                path: path.clone(),
                enabled: res.enabled,
                metadata: metadata.clone(),
                resource_type,
                display_name,
                group_key: group_key.clone(),
                subgroup_key,
            });
        }
    };

    add_to_group(
        &mut groups,
        &mut group_index,
        &resolved.extensions,
        ResourceType::Extensions,
    );
    add_to_group(
        &mut groups,
        &mut group_index,
        &resolved.skills,
        ResourceType::Skills,
    );
    add_to_group(
        &mut groups,
        &mut group_index,
        &resolved.prompts,
        ResourceType::Prompts,
    );
    add_to_group(
        &mut groups,
        &mut group_index,
        &resolved.themes,
        ResourceType::Themes,
    );

    // Sort groups: packages first, then top-level; user before project.
    groups.sort_by(|a, b| {
        if a.origin != b.origin {
            return if a.origin == ResourceOrigin::Package {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        if a.scope != b.scope {
            return if a.scope == PackageScope::User {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        a.source.cmp(&b.source)
    });

    // Sort subgroups within each group by type order, and items by name.
    for group in &mut groups {
        group.subgroups.sort_by_key(|sg| sg.resource_type.order());
        for subgroup in &mut group.subgroups {
            subgroup
                .items
                .sort_by(|a, b| a.display_name.cmp(&b.display_name));
        }
    }

    groups
}

/// Flattened list entry: indices into the current scope's groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlatEntry {
    Group(usize),
    Subgroup(usize, usize),
    Item(usize, usize, usize),
}

/// Header showing scope title + key hints (oracle `ConfigSelectorHeader`).
struct ConfigSelectorHeader {
    write_scope: ConfigWriteScope,
    project_mode_available: bool,
    cached: Vec<Line>,
}

impl ConfigSelectorHeader {
    fn new(write_scope: ConfigWriteScope, project_mode_available: bool) -> Self {
        Self {
            write_scope,
            project_mode_available,
            cached: Vec::new(),
        }
    }

    fn set_write_scope(&mut self, write_scope: ConfigWriteScope) {
        self.write_scope = write_scope;
    }
}

impl Component for ConfigSelectorHeader {
    fn render(&mut self, width: u16) -> &[Line] {
        let width = width as usize;
        let title = theme().bold(if self.write_scope == ConfigWriteScope::Project {
            "Project Local Resources"
        } else {
            "Global Resources"
        });
        let sep = theme().fg(ThemeColor::Muted, " · ");
        let switch_hint = if self.project_mode_available {
            format!("{}{sep}", key_hint("tui.input.tab", "switch mode"))
        } else {
            String::new()
        };
        let action_hint = if self.write_scope == ConfigWriteScope::Project {
            raw_key_hint("space", "cycle inherit/+/-")
        } else {
            raw_key_hint("space", "toggle")
        };
        let hint = format!(
            "{switch_hint}{action_hint}{sep}{}",
            raw_key_hint("esc", "close")
        );
        let spacing = (width as i64 - visible_width(&title) as i64 - visible_width(&hint) as i64)
            .max(1) as usize;
        let scope_hint = if self.write_scope == ConfigWriteScope::Project {
            theme().fg(
                ThemeColor::Muted,
                &format!("{CONFIG_DIR_NAME}/settings.json · inherited global resources are dimmed"),
            )
        } else {
            theme().fg(
                ThemeColor::Muted,
                &format!("~/{CONFIG_DIR_NAME}/agent/settings.json"),
            )
        };

        self.cached = vec![
            Line::from_ansi(&truncate_to_width(
                &format!("{title}{}{hint}", " ".repeat(spacing)),
                width,
            )),
            Line::from_ansi(&truncate_to_width(&scope_hint, width)),
        ];
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }
}

/// `(item, new_enabled)` toggle callback (clippy `type_complexity`).
pub type ToggleCallback = Box<dyn FnMut(&ResourceItem, bool)>;

/// Searchable, scrollable resource list (oracle `ResourceList`).
pub struct ResourceList {
    groups_global: Vec<ResourceGroup>,
    groups_project: Vec<ResourceGroup>,
    flat_items: Vec<FlatEntry>,
    filtered_items: Vec<FlatEntry>,
    selected_index: usize,
    search_input: Input,
    max_visible: usize,
    settings_manager: Arc<Mutex<SettingsManager>>,
    cwd: PathBuf,
    agent_dir: PathBuf,
    write_scope: ConfigWriteScope,
    inherited_enabled_by_key: HashMap<String, bool>,

    pub on_cancel: Option<Box<dyn FnMut()>>,
    pub on_exit: Option<Box<dyn FnMut()>>,
    pub on_toggle: Option<ToggleCallback>,
    pub on_switch_mode: Option<Box<dyn FnMut()>>,

    focused: bool,
    cached: Vec<Line>,
}

impl ResourceList {
    fn new(
        groups_global: Vec<ResourceGroup>,
        groups_project: Vec<ResourceGroup>,
        settings_manager: Arc<Mutex<SettingsManager>>,
        cwd: PathBuf,
        agent_dir: PathBuf,
        terminal_height: Option<u16>,
        write_scope: ConfigWriteScope,
    ) -> Self {
        let inherited_enabled_by_key = Self::build_inherited_enabled_map(&groups_global);
        // 8 lines of chrome: top spacer + top border + spacer + header (2 lines)
        // + spacer + bottom spacer + bottom border
        let chrome = 8usize;
        let max_visible = (usize::from(terminal_height.unwrap_or(24)))
            .saturating_sub(chrome)
            .max(5);
        let mut list = Self {
            groups_global,
            groups_project,
            flat_items: Vec::new(),
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::new(),
            max_visible,
            settings_manager,
            cwd,
            agent_dir,
            write_scope,
            inherited_enabled_by_key,
            on_cancel: None,
            on_exit: None,
            on_toggle: None,
            on_switch_mode: None,
            focused: false,
            cached: Vec::new(),
        };
        list.build_flat_list();
        list.filtered_items = list.flat_items.clone();
        list
    }

    pub fn set_write_scope(&mut self, write_scope: ConfigWriteScope) {
        self.write_scope = write_scope;
        self.build_flat_list();
        let query = self.search_input.get_value().to_owned();
        self.filter_items(&query);
    }

    fn groups(&self) -> &[ResourceGroup] {
        match self.write_scope {
            ConfigWriteScope::Global => &self.groups_global,
            ConfigWriteScope::Project => &self.groups_project,
        }
    }

    fn groups_mut(&mut self) -> &mut [ResourceGroup] {
        match self.write_scope {
            ConfigWriteScope::Global => &mut self.groups_global,
            ConfigWriteScope::Project => &mut self.groups_project,
        }
    }

    fn item(&self, entry: FlatEntry) -> Option<&ResourceItem> {
        match entry {
            FlatEntry::Item(g, s, i) => self.groups().get(g)?.subgroups.get(s)?.items.get(i),
            _ => None,
        }
    }

    fn build_inherited_enabled_map(groups: &[ResourceGroup]) -> HashMap<String, bool> {
        let mut result = HashMap::new();
        for group in groups {
            for subgroup in &group.subgroups {
                for item in &subgroup.items {
                    result.insert(Self::resource_item_key(item), item.enabled);
                }
            }
        }
        result
    }

    fn build_flat_list(&mut self) {
        let mut flat = Vec::new();
        for (g, group) in self.groups().iter().enumerate() {
            flat.push(FlatEntry::Group(g));
            for (s, subgroup) in group.subgroups.iter().enumerate() {
                flat.push(FlatEntry::Subgroup(g, s));
                for i in 0..subgroup.items.len() {
                    flat.push(FlatEntry::Item(g, s, i));
                }
            }
        }
        self.flat_items = flat;
        // Start selection on first item (not header).
        self.selected_index = self
            .flat_items
            .iter()
            .position(|e| matches!(e, FlatEntry::Item(..)))
            .unwrap_or(0);
    }

    fn find_next_item(&self, from_index: usize, direction: i64) -> usize {
        let mut idx = from_index as i64 + direction;
        while idx >= 0 && (idx as usize) < self.filtered_items.len() {
            if matches!(self.filtered_items[idx as usize], FlatEntry::Item(..)) {
                return idx as usize;
            }
            idx += direction;
        }
        from_index // Stay at current if no item found
    }

    fn filter_items(&mut self, query: &str) {
        if query.trim().is_empty() {
            self.filtered_items = self.flat_items.clone();
            self.select_first_item();
            return;
        }

        let lower_query = query.to_lowercase();
        let mut matching_items: HashSet<(usize, usize, usize)> = HashSet::new();
        let mut matching_subgroups: HashSet<(usize, usize)> = HashSet::new();
        let mut matching_groups: HashSet<usize> = HashSet::new();

        for entry in &self.flat_items {
            if let FlatEntry::Item(g, s, i) = *entry {
                let item = &self.groups()[g].subgroups[s].items[i];
                if item.display_name.to_lowercase().contains(&lower_query)
                    || item
                        .resource_type
                        .as_str()
                        .to_lowercase()
                        .contains(&lower_query)
                    || item.path.to_lowercase().contains(&lower_query)
                {
                    matching_items.insert((g, s, i));
                    matching_subgroups.insert((g, s));
                    matching_groups.insert(g);
                }
            }
        }

        self.filtered_items = self
            .flat_items
            .iter()
            .filter(|entry| match **entry {
                FlatEntry::Group(g) => matching_groups.contains(&g),
                FlatEntry::Subgroup(g, s) => matching_subgroups.contains(&(g, s)),
                FlatEntry::Item(g, s, i) => matching_items.contains(&(g, s, i)),
            })
            .copied()
            .collect();

        self.select_first_item();
    }

    fn select_first_item(&mut self) {
        self.selected_index = self
            .filtered_items
            .iter()
            .position(|e| matches!(e, FlatEntry::Item(..)))
            .unwrap_or(0);
    }

    fn update_item(&mut self, entry: FlatEntry, enabled: bool) {
        let FlatEntry::Item(g, s, i) = entry else {
            return;
        };
        let (path, resource_type) = {
            let item = &self.groups()[g].subgroups[s].items[i];
            (item.path.clone(), item.resource_type)
        };
        // Update every copy of this resource in the current scope's groups.
        for group in self.groups_mut() {
            for subgroup in &mut group.subgroups {
                if let Some(found) = subgroup
                    .items
                    .iter_mut()
                    .find(|it| it.path == path && it.resource_type == resource_type)
                {
                    found.enabled = enabled;
                    return;
                }
            }
        }
    }

    fn toggle_resource(&mut self, item: &ResourceItem) -> Option<bool> {
        if self.write_scope == ConfigWriteScope::Project {
            let state = self.next_override_state(item);
            if !self.set_project_resource_override(item, state) {
                return None;
            }
            return Some(match state {
                ProjectOverrideState::Inherit => self.inherited_enabled(item),
                ProjectOverrideState::Load => true,
                ProjectOverrideState::Unload => false,
            });
        }

        let enabled = !item.enabled;
        if item.metadata.origin == ResourceOrigin::TopLevel {
            self.toggle_top_level_resource(item, enabled);
        } else {
            self.toggle_package_resource(item, enabled);
        }
        Some(enabled)
    }

    fn toggle_top_level_resource(&mut self, item: &ResourceItem, enabled: bool) {
        let scope = Self::item_scope(item);
        let array_key = item.resource_type;
        let current: Vec<String> = {
            let sm = self.settings_manager.lock();
            let settings = if scope == SettingsScope::Project {
                sm.project_settings()
            } else {
                sm.global_settings()
            };
            string_array(settings.0.get(array_key.as_str()))
        };

        // Generate pattern for this resource
        let pattern = self.resource_pattern(item);
        let disable_pattern = format!("-{pattern}");
        let enable_pattern = format!("+{pattern}");

        // Filter out existing patterns for this resource
        let mut updated: Vec<String> = current
            .into_iter()
            .filter(|p| pattern_entry_target(p) != pattern)
            .collect();

        if enabled {
            updated.push(enable_pattern);
        } else {
            updated.push(disable_pattern);
        }

        let mut sm = self.settings_manager.lock();
        if scope == SettingsScope::Project {
            let _ = match array_key {
                ResourceType::Extensions => sm.set_project_extension_paths(updated),
                ResourceType::Skills => sm.set_project_skill_paths(updated),
                ResourceType::Prompts => sm.set_project_prompt_template_paths(updated),
                ResourceType::Themes => sm.set_project_theme_paths(updated),
            };
        } else {
            match array_key {
                ResourceType::Extensions => sm.set_extension_paths(updated),
                ResourceType::Skills => sm.set_skill_paths(updated),
                ResourceType::Prompts => sm.set_prompt_template_paths(updated),
                ResourceType::Themes => sm.set_theme_paths(updated),
            }
        }
    }

    fn toggle_package_resource(&mut self, item: &ResourceItem, enabled: bool) {
        let scope = Self::item_scope(item);
        let mut packages: Vec<Value> = {
            let sm = self.settings_manager.lock();
            let settings = if scope == SettingsScope::Project {
                sm.project_settings()
            } else {
                sm.global_settings()
            };
            settings
                .0
                .get("packages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };

        let Some(pkg_index) = packages
            .iter()
            .position(|pkg| package_source_str(pkg) == Some(item.metadata.source.as_str()))
        else {
            return;
        };

        // Convert string to object form if needed
        if packages[pkg_index].is_string() {
            let source = packages[pkg_index].clone();
            let mut obj = serde_json::Map::new();
            obj.insert("source".to_owned(), source);
            packages[pkg_index] = Value::Object(obj);
        }

        let array_key = item.resource_type.as_str();
        let current = string_array(packages[pkg_index].get(array_key));

        // Generate pattern relative to package root
        let pattern = self.package_resource_pattern(item);
        let disable_pattern = format!("-{pattern}");
        let enable_pattern = format!("+{pattern}");

        // Filter out existing patterns for this resource
        let mut updated: Vec<String> = current
            .into_iter()
            .filter(|p| pattern_entry_target(p) != pattern)
            .collect();

        if enabled {
            updated.push(enable_pattern);
        } else {
            updated.push(disable_pattern);
        }

        let pkg = packages[pkg_index]
            .as_object_mut()
            .expect("converted to object above");
        if updated.is_empty() {
            pkg.remove(array_key);
        } else {
            pkg.insert(
                array_key.to_owned(),
                Value::Array(updated.into_iter().map(Value::String).collect()),
            );
        }

        // Clean up empty filter object
        let has_filters = ResourceType::ALL
            .iter()
            .any(|k| pkg.contains_key(k.as_str()));
        if !has_filters {
            let source = pkg.get("source").cloned().unwrap_or(Value::Null);
            packages[pkg_index] = source;
        }

        let mut sm = self.settings_manager.lock();
        if scope == SettingsScope::Project {
            let _ = sm.set_project_packages(packages);
        } else {
            sm.set_packages(packages);
        }
    }

    fn render_checkbox(&self, item: &ResourceItem) -> String {
        if self.write_scope == ConfigWriteScope::Project {
            let state = self.project_override_state(item);
            if state == ProjectOverrideState::Load {
                return theme().fg(ThemeColor::Success, "[+]");
            }
            if state == ProjectOverrideState::Unload {
                return theme().fg(ThemeColor::Warning, "[-]");
            }
            return theme().fg(ThemeColor::Dim, if item.enabled { "[x]" } else { "[ ]" });
        }
        if item.enabled {
            theme().fg(ThemeColor::Success, "[x]")
        } else {
            theme().fg(ThemeColor::Dim, "[ ]")
        }
    }

    fn item_suffix(&self, item: &ResourceItem) -> String {
        if self.write_scope != ConfigWriteScope::Project {
            return String::new();
        }
        let state = self.project_override_state(item);
        if state == ProjectOverrideState::Load {
            return theme().fg(ThemeColor::Muted, "  project load");
        }
        if state == ProjectOverrideState::Unload {
            return theme().fg(ThemeColor::Muted, "  project unload");
        }
        if self.is_inherited_global_item(item) {
            theme().fg(ThemeColor::Dim, "  inherited global")
        } else {
            String::new()
        }
    }

    fn is_dimmed_item(&self, item: &ResourceItem) -> bool {
        self.write_scope == ConfigWriteScope::Project
            && self.is_inherited_global_item(item)
            && self.project_override_state(item) == ProjectOverrideState::Inherit
    }

    fn set_project_resource_override(
        &mut self,
        item: &ResourceItem,
        state: ProjectOverrideState,
    ) -> bool {
        if item.metadata.origin == ResourceOrigin::TopLevel {
            self.set_project_top_level_override(item, state)
        } else {
            self.set_project_package_override(item, state)
        }
    }

    fn set_project_top_level_override(
        &mut self,
        item: &ResourceItem,
        state: ProjectOverrideState,
    ) -> bool {
        let current: Vec<String> = {
            let sm = self.settings_manager.lock();
            string_array(sm.project_settings().0.get(item.resource_type.as_str()))
        };
        let pattern = if self.is_inherited_global_item(item) {
            item.path.clone()
        } else {
            self.resource_pattern_for_scope(item, SettingsScope::Project)
        };
        let patterns = self.top_level_override_patterns(item, SettingsScope::Project);
        let is_inherited = self.is_inherited_global_item(item);
        let mut updated: Vec<String> = current
            .into_iter()
            .filter(|entry| {
                let target = pattern_entry_target(entry);
                if (entry.starts_with('!') || entry.starts_with('+') || entry.starts_with('-'))
                    && patterns.contains(target)
                {
                    return false;
                }
                !(state == ProjectOverrideState::Inherit && is_inherited && target == pattern)
            })
            .collect();
        if state != ProjectOverrideState::Inherit {
            if is_inherited && !updated.iter().any(|p| p == &pattern) {
                updated.push(pattern.clone());
            }
            updated.push(format!(
                "{}{pattern}",
                if state == ProjectOverrideState::Load {
                    "+"
                } else {
                    "-"
                }
            ));
        }
        self.set_project_top_level_paths(item.resource_type, updated);
        true
    }

    fn set_project_top_level_paths(&mut self, key: ResourceType, paths: Vec<String>) {
        let mut sm = self.settings_manager.lock();
        let _ = match key {
            ResourceType::Extensions => sm.set_project_extension_paths(paths),
            ResourceType::Skills => sm.set_project_skill_paths(paths),
            ResourceType::Prompts => sm.set_project_prompt_template_paths(paths),
            ResourceType::Themes => sm.set_project_theme_paths(paths),
        };
    }

    fn set_project_package_override(
        &mut self,
        item: &ResourceItem,
        state: ProjectOverrideState,
    ) -> bool {
        let mut packages: Vec<Value> = {
            let sm = self.settings_manager.lock();
            sm.project_settings()
                .0
                .get("packages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let item_scope = Self::item_scope(item);
        let mut pkg_index = packages.iter().position(|pkg| {
            package_source_str(pkg).is_some_and(|source| {
                self.package_source_string_matches(
                    &item.metadata.source,
                    item_scope,
                    source,
                    SettingsScope::Project,
                )
            })
        });
        if pkg_index.is_none() {
            if state == ProjectOverrideState::Inherit {
                return false;
            }
            packages.push(self.create_package_override_source(item));
            pkg_index = Some(packages.len() - 1);
        }
        let pkg_index = pkg_index.expect("set above");
        if packages[pkg_index].is_string() {
            let source = packages[pkg_index].clone();
            let mut obj = serde_json::Map::new();
            obj.insert("source".to_owned(), source);
            packages[pkg_index] = Value::Object(obj);
        }
        let pattern = self.package_resource_pattern(item);
        let array_key = item.resource_type.as_str();
        let updated: Vec<String> = string_array(packages[pkg_index].get(array_key))
            .into_iter()
            .filter(|entry| pattern_entry_target(entry) != pattern)
            .chain((state != ProjectOverrideState::Inherit).then(|| {
                format!(
                    "{}{pattern}",
                    if state == ProjectOverrideState::Load {
                        "+"
                    } else {
                        "-"
                    }
                )
            }))
            .collect();
        let pkg = packages[pkg_index]
            .as_object_mut()
            .expect("converted to object above");
        if updated.is_empty() {
            pkg.remove(array_key);
        } else {
            pkg.insert(
                array_key.to_owned(),
                Value::Array(updated.into_iter().map(Value::String).collect()),
            );
        }
        if !ResourceType::ALL
            .iter()
            .any(|key| pkg.contains_key(key.as_str()))
        {
            let autoload_false = pkg.get("autoload") == Some(&Value::Bool(false));
            if autoload_false {
                packages.remove(pkg_index);
            } else {
                let source = pkg.get("source").cloned().unwrap_or(Value::Null);
                packages[pkg_index] = source;
            }
        }
        let mut sm = self.settings_manager.lock();
        let _ = sm.set_project_packages(packages);
        true
    }

    fn next_override_state(&self, item: &ResourceItem) -> ProjectOverrideState {
        let state = self.project_override_state(item);
        let inherited_enabled = self.inherited_enabled(item);
        match state {
            ProjectOverrideState::Inherit => {
                if inherited_enabled {
                    ProjectOverrideState::Unload
                } else {
                    ProjectOverrideState::Load
                }
            }
            ProjectOverrideState::Unload => {
                if inherited_enabled {
                    ProjectOverrideState::Load
                } else {
                    ProjectOverrideState::Inherit
                }
            }
            ProjectOverrideState::Load => {
                if inherited_enabled {
                    ProjectOverrideState::Inherit
                } else {
                    ProjectOverrideState::Unload
                }
            }
        }
    }

    fn project_override_state(&self, item: &ResourceItem) -> ProjectOverrideState {
        if self.write_scope != ConfigWriteScope::Project {
            return ProjectOverrideState::Inherit;
        }
        if item.metadata.origin == ResourceOrigin::TopLevel {
            let entries: Vec<String> = {
                let sm = self.settings_manager.lock();
                string_array(sm.project_settings().0.get(item.resource_type.as_str()))
            };
            return override_state_from_entries(
                &entries,
                &self.top_level_override_patterns(item, SettingsScope::Project),
                false,
            );
        }
        let Some(pkg) = self.find_matching_package_source(item, SettingsScope::Project) else {
            return ProjectOverrideState::Inherit;
        };
        let Value::Object(pkg) = pkg else {
            return ProjectOverrideState::Inherit;
        };
        let Some(entries_value) = pkg.get(item.resource_type.as_str()) else {
            return ProjectOverrideState::Inherit;
        };
        let entries = string_array(Some(entries_value));
        let mut patterns = HashSet::new();
        patterns.insert(self.package_resource_pattern(item));
        override_state_from_entries(
            &entries,
            &patterns,
            pkg.get("autoload") != Some(&Value::Bool(false)),
        )
    }

    fn inherited_enabled(&self, item: &ResourceItem) -> bool {
        self.inherited_enabled_by_key
            .get(&Self::resource_item_key(item))
            .copied()
            .unwrap_or_else(|| {
                if Self::item_scope(item) == SettingsScope::User {
                    item.enabled
                } else {
                    true
                }
            })
    }

    fn is_inherited_global_item(&self, item: &ResourceItem) -> bool {
        Self::item_scope(item) == SettingsScope::User
            || self
                .inherited_enabled_by_key
                .contains_key(&Self::resource_item_key(item))
    }

    fn top_level_override_patterns(
        &self,
        item: &ResourceItem,
        scope: SettingsScope,
    ) -> HashSet<String> {
        let base_dir = self.top_level_base_dir(scope);
        let mut patterns = HashSet::new();
        patterns.insert(self.resource_pattern_for_scope(item, scope));
        patterns.insert(item.path.clone());
        patterns.insert(relative_string(&base_dir, Path::new(&item.path)));
        if let Some(meta_base) = &item.metadata.base_dir {
            patterns.insert(relative_string(meta_base, Path::new(&item.path)));
        }
        patterns
    }

    fn resource_pattern_for_scope(&self, item: &ResourceItem, scope: SettingsScope) -> String {
        let source_scope = Self::item_scope(item);
        if scope != source_scope {
            return item.path.clone();
        }
        let base_dir = item
            .metadata
            .base_dir
            .clone()
            .unwrap_or_else(|| self.top_level_base_dir(source_scope));
        relative_string(&base_dir, Path::new(&item.path))
    }

    fn create_package_override_source(&self, item: &ResourceItem) -> Value {
        let source = &item.metadata.source;
        let mut obj = serde_json::Map::new();
        if !is_local_path(source) {
            obj.insert("source".to_owned(), Value::String(source.clone()));
            obj.insert("autoload".to_owned(), Value::Bool(false));
            return Value::Object(obj);
        }
        let source_path = resolve_path(
            source.trim(),
            Some(&self.top_level_base_dir(Self::item_scope(item))),
        );
        let relative = relative_string(
            &self.top_level_base_dir(SettingsScope::Project),
            &source_path,
        );
        let relative = if relative.is_empty() {
            ".".to_owned()
        } else {
            relative
        };
        obj.insert("source".to_owned(), Value::String(relative));
        obj.insert("autoload".to_owned(), Value::Bool(false));
        Value::Object(obj)
    }

    fn package_source_string_matches(
        &self,
        left_source: &str,
        left_scope: SettingsScope,
        right_source: &str,
        right_scope: SettingsScope,
    ) -> bool {
        if left_source == right_source {
            return true;
        }
        if !is_local_path(left_source) || !is_local_path(right_source) {
            return false;
        }
        let left = resolve_path(
            left_source.trim(),
            Some(&self.top_level_base_dir(left_scope)),
        );
        let right = resolve_path(
            right_source.trim(),
            Some(&self.top_level_base_dir(right_scope)),
        );
        left == right
    }

    fn find_matching_package_source(
        &self,
        item: &ResourceItem,
        target_scope: SettingsScope,
    ) -> Option<Value> {
        let sm = self.settings_manager.lock();
        let settings = if target_scope == SettingsScope::Project {
            sm.project_settings()
        } else {
            sm.global_settings()
        };
        let item_scope = Self::item_scope(item);
        settings
            .0
            .get("packages")
            .and_then(Value::as_array)
            .and_then(|packages| {
                packages
                    .iter()
                    .find(|pkg| {
                        package_source_str(pkg).is_some_and(|source| {
                            self.package_source_string_matches(
                                &item.metadata.source,
                                item_scope,
                                source,
                                target_scope,
                            )
                        })
                    })
                    .cloned()
            })
    }

    fn resource_item_key(item: &ResourceItem) -> String {
        format!(
            "{}:{}",
            item.resource_type.as_str(),
            canonicalize_path(&item.path)
        )
    }

    fn item_scope(item: &ResourceItem) -> SettingsScope {
        if item.metadata.scope == PackageScope::Project {
            SettingsScope::Project
        } else {
            SettingsScope::User
        }
    }

    fn top_level_base_dir(&self, scope: SettingsScope) -> PathBuf {
        match scope {
            SettingsScope::Project => self.cwd.join(CONFIG_DIR_NAME),
            SettingsScope::User => self.agent_dir.clone(),
        }
    }

    fn resource_pattern(&self, item: &ResourceItem) -> String {
        let scope = Self::item_scope(item);
        let base_dir = item
            .metadata
            .base_dir
            .clone()
            .unwrap_or_else(|| self.top_level_base_dir(scope));
        relative_string(&base_dir, Path::new(&item.path))
    }

    fn package_resource_pattern(&self, item: &ResourceItem) -> String {
        let base_dir = item
            .metadata
            .base_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(dirname(&item.path)));
        relative_string(&base_dir, Path::new(&item.path))
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn package_source_str(pkg: &Value) -> Option<&str> {
    match pkg {
        Value::String(s) => Some(s),
        Value::Object(obj) => obj.get("source").and_then(Value::as_str),
        _ => None,
    }
}

fn pattern_entry_target(entry: &str) -> &str {
    if entry.starts_with('!') || entry.starts_with('+') || entry.starts_with('-') {
        &entry[1..]
    } else {
        entry
    }
}

fn override_state_from_entries(
    entries: &[String],
    patterns: &HashSet<String>,
    empty_array_is_unload: bool,
) -> ProjectOverrideState {
    if entries.is_empty() && empty_array_is_unload {
        return ProjectOverrideState::Unload;
    }
    let mut state = ProjectOverrideState::Inherit;
    for entry in entries {
        if !patterns.contains(pattern_entry_target(entry)) {
            continue;
        }
        if entry.starts_with('!') || entry.starts_with('-') {
            state = ProjectOverrideState::Unload;
        } else {
            state = ProjectOverrideState::Load;
        }
    }
    state
}

/// Truncate with `"..."` suffix (TS `truncateToWidth(text, width, "...")`).
fn truncate_ellipsis(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(text) <= max_width {
        return text.to_owned();
    }
    if max_width <= 3 {
        let clipped = truncate_to_width("...", max_width);
        return format!("\x1b[0m{clipped}");
    }
    let prefix = truncate_to_width(text, max_width - 3);
    format!("{prefix}...\x1b[0m")
}

impl Component for ResourceList {
    fn render(&mut self, width: u16) -> &[Line] {
        let w = width as usize;
        let mut lines: Vec<Line> = Vec::new();

        // Search input
        lines.extend_from_slice(self.search_input.render(width));
        lines.push(Line::empty());

        if self.filtered_items.is_empty() {
            lines.push(Line::from_ansi(
                &theme().fg(ThemeColor::Muted, "  No resources found"),
            ));
            self.cached = lines;
            return &self.cached;
        }

        // Calculate visible range
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.filtered_items.len().saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(self.filtered_items.len());

        for i in start_index..end_index {
            let entry = self.filtered_items[i];
            let is_selected = i == self.selected_index;

            match entry {
                FlatEntry::Group(g) => {
                    // Main group header (no cursor)
                    let group = &self.groups()[g];
                    let inherited = self.write_scope == ConfigWriteScope::Project
                        && group.scope == PackageScope::User;
                    let label = theme().bold(&format!(
                        "{}{}",
                        group.label,
                        if inherited {
                            " · inherited global"
                        } else {
                            ""
                        }
                    ));
                    let group_line = theme().fg(
                        if inherited {
                            ThemeColor::Dim
                        } else {
                            ThemeColor::Accent
                        },
                        &label,
                    );
                    lines.push(Line::from_ansi(&truncate_to_width(
                        &format!("  {group_line}"),
                        w,
                    )));
                }
                FlatEntry::Subgroup(g, s) => {
                    // Subgroup header (indented, no cursor)
                    let group = &self.groups()[g];
                    let color = if self.write_scope == ConfigWriteScope::Project
                        && group.scope == PackageScope::User
                    {
                        ThemeColor::Dim
                    } else {
                        ThemeColor::Muted
                    };
                    let subgroup_line = theme().fg(color, group.subgroups[s].label);
                    lines.push(Line::from_ansi(&truncate_to_width(
                        &format!("    {subgroup_line}"),
                        w,
                    )));
                }
                FlatEntry::Item(g, s, i) => {
                    // Resource item (cursor only on items)
                    let item = self.groups()[g].subgroups[s].items[i].clone();
                    let cursor = if is_selected { "> " } else { "  " };
                    let dimmed = self.is_dimmed_item(&item);
                    let name_text = if is_selected && !dimmed {
                        theme().bold(&item.display_name)
                    } else {
                        item.display_name.clone()
                    };
                    let name = if dimmed {
                        theme().fg(ThemeColor::Dim, &name_text)
                    } else {
                        name_text
                    };
                    lines.push(Line::from_ansi(&truncate_ellipsis(
                        &format!(
                            "{cursor}    {} {name}{}",
                            self.render_checkbox(&item),
                            self.item_suffix(&item)
                        ),
                        w,
                    )));
                }
            }
        }

        // Scroll indicator
        if start_index > 0 || end_index < self.filtered_items.len() {
            let item_count = self
                .filtered_items
                .iter()
                .filter(|e| matches!(e, FlatEntry::Item(..)))
                .count();
            let current_item_index = self.filtered_items[..self.selected_index]
                .iter()
                .filter(|e| matches!(e, FlatEntry::Item(..)))
                .count()
                + 1;
            lines.push(Line::from_ansi(&theme().fg(
                ThemeColor::Dim,
                &format!("  ({current_item_index}/{item_count})"),
            )));
        }

        self.cached = lines;
        &self.cached
    }

    fn invalidate(&mut self) {}

    fn handle_input(&mut self, data: &str) {
        let (up, down, page_up, page_down, cancel, tab, confirm) = {
            let kb = get_keybindings();
            (
                kb.matches(data, "tui.select.up"),
                kb.matches(data, "tui.select.down"),
                kb.matches(data, "tui.select.pageUp"),
                kb.matches(data, "tui.select.pageDown"),
                kb.matches(data, "tui.select.cancel"),
                kb.matches(data, "tui.input.tab"),
                kb.matches(data, "tui.select.confirm"),
            )
        };

        if up {
            self.selected_index = self.find_next_item(self.selected_index, -1);
            return;
        }
        if down {
            self.selected_index = self.find_next_item(self.selected_index, 1);
            return;
        }
        if page_up {
            // Jump up by maxVisible, then find nearest item
            let mut target = self.selected_index.saturating_sub(self.max_visible);
            while target < self.filtered_items.len()
                && !matches!(self.filtered_items[target], FlatEntry::Item(..))
            {
                target += 1;
            }
            if target < self.filtered_items.len() {
                self.selected_index = target;
            }
            return;
        }
        if page_down {
            // Jump down by maxVisible, then find nearest item
            let mut target = (self.selected_index + self.max_visible)
                .min(self.filtered_items.len().saturating_sub(1))
                as i64;
            while target >= 0
                && !matches!(self.filtered_items[target as usize], FlatEntry::Item(..))
            {
                target -= 1;
            }
            if target >= 0 {
                self.selected_index = target as usize;
            }
            return;
        }
        if cancel {
            if let Some(cb) = &mut self.on_cancel {
                cb();
            }
            return;
        }
        if matches_key(data, "ctrl+c") {
            if let Some(cb) = &mut self.on_exit {
                cb();
            }
            return;
        }
        if tab {
            if let Some(cb) = &mut self.on_switch_mode {
                cb();
            }
            return;
        }
        if data == " " || confirm {
            let Some(entry) = self.filtered_items.get(self.selected_index).copied() else {
                return;
            };
            let Some(item) = self.item(entry).cloned() else {
                return;
            };
            if (self.write_scope == ConfigWriteScope::Project
                || Self::item_scope(&item) == SettingsScope::User)
                && let Some(new_enabled) = self.toggle_resource(&item)
            {
                self.update_item(entry, new_enabled);
                if let Some(cb) = &mut self.on_toggle {
                    cb(&item, new_enabled);
                }
            }
            return;
        }

        // Pass to search input
        self.search_input.handle_input(data);
        let query = self.search_input.get_value().to_owned();
        self.filter_items(&query);
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ResourceList {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}

/// Full config selector overlay (oracle `ConfigSelectorComponent`).
pub struct ConfigSelectorComponent {
    header: ConfigSelectorHeader,
    resource_list: ResourceList,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,
    write_scope: ConfigWriteScope,
    project_mode_available: bool,
    request_render: Box<dyn Fn()>,
    /// Set by the resource-list `on_switch_mode` closure; drained after input.
    switch_requested: std::rc::Rc<std::cell::Cell<bool>>,
    /// Set by the resource-list `on_toggle` closure; drained after input.
    render_requested: std::rc::Rc<std::cell::Cell<bool>>,
    focused: bool,
    cached: Vec<Line>,
}

impl ConfigSelectorComponent {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        resolved_paths: &ScopedResolvedPaths,
        settings_manager: Arc<Mutex<SettingsManager>>,
        cwd: &Path,
        agent_dir: &Path,
        on_close: Box<dyn FnMut()>,
        on_exit: Box<dyn FnMut()>,
        request_render: Box<dyn Fn()>,
        terminal_height: Option<u16>,
        write_scope: ConfigWriteScope,
        project_mode_available: bool,
    ) -> Self {
        let groups_global = build_groups(&resolved_paths.global, agent_dir);
        let groups_project = build_groups(&resolved_paths.project, agent_dir);

        let mut resource_list = ResourceList::new(
            groups_global,
            groups_project,
            settings_manager,
            cwd.to_path_buf(),
            agent_dir.to_path_buf(),
            terminal_height,
            write_scope,
        );
        resource_list.on_cancel = Some(on_close);
        resource_list.on_exit = Some(on_exit);

        let render_requested = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let flag = std::rc::Rc::clone(&render_requested);
            resource_list.on_toggle = Some(Box::new(move |_, _| flag.set(true)));
        }
        let switch_requested = std::rc::Rc::new(std::cell::Cell::new(false));
        if project_mode_available {
            let flag = std::rc::Rc::clone(&switch_requested);
            resource_list.on_switch_mode = Some(Box::new(move || flag.set(true)));
        }

        Self {
            header: ConfigSelectorHeader::new(write_scope, project_mode_available),
            resource_list,
            top_border: DynamicBorder::default(),
            bottom_border: DynamicBorder::default(),
            write_scope,
            project_mode_available,
            request_render,
            switch_requested,
            render_requested,
            focused: false,
            cached: Vec::new(),
        }
    }

    fn switch_write_scope(&mut self) {
        self.write_scope = if self.write_scope == ConfigWriteScope::Global {
            ConfigWriteScope::Project
        } else {
            ConfigWriteScope::Global
        };
        self.header.set_write_scope(self.write_scope);
        self.resource_list.set_write_scope(self.write_scope);
    }

    pub fn get_resource_list(&mut self) -> &mut ResourceList {
        &mut self.resource_list
    }

    #[must_use]
    pub fn write_scope(&self) -> ConfigWriteScope {
        self.write_scope
    }

    #[must_use]
    pub fn project_mode_available(&self) -> bool {
        self.project_mode_available
    }
}

impl Component for ConfigSelectorComponent {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.push(Line::empty());
        self.cached.extend_from_slice(self.top_border.render(width));
        self.cached.push(Line::empty());
        self.cached.extend_from_slice(self.header.render(width));
        self.cached.push(Line::empty());
        self.cached
            .extend_from_slice(self.resource_list.render(width));
        self.cached.push(Line::empty());
        self.cached
            .extend_from_slice(self.bottom_border.render(width));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.top_border.invalidate();
        self.header.invalidate();
        self.resource_list.invalidate();
        self.bottom_border.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        self.resource_list.handle_input(data);
        let mut render = self.render_requested.replace(false);
        if self.switch_requested.replace(false) {
            self.switch_write_scope();
            render = true;
        }
        if render {
            (self.request_render)();
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ConfigSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.resource_list.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_manager::ResolvedResource;
    use crate::settings_manager::Settings;

    fn resource(path: &str, scope: PackageScope, origin: ResourceOrigin) -> ResolvedResource {
        ResolvedResource {
            path: PathBuf::from(path),
            enabled: true,
            metadata: PathMetadata {
                source: "auto".to_owned(),
                scope,
                origin,
                base_dir: None,
            },
        }
    }

    fn user_extension_paths() -> ScopedResolvedPaths {
        let global = ResolvedPaths {
            extensions: vec![resource(
                "/agent/extensions/foo.ts",
                PackageScope::User,
                ResourceOrigin::TopLevel,
            )],
            skills: vec![resource(
                "/agent/skills/writer/SKILL.md",
                PackageScope::User,
                ResourceOrigin::TopLevel,
            )],
            ..Default::default()
        };
        ScopedResolvedPaths {
            project: global.clone(),
            global,
        }
    }

    fn make_list(write_scope: ConfigWriteScope) -> (ResourceList, Arc<Mutex<SettingsManager>>) {
        let paths = user_extension_paths();
        let sm = Arc::new(Mutex::new(SettingsManager::in_memory(
            Settings::default(),
            true,
        )));
        let list = ResourceList::new(
            build_groups(&paths.global, Path::new("/agent")),
            build_groups(&paths.project, Path::new("/agent")),
            Arc::clone(&sm),
            PathBuf::from("/proj"),
            PathBuf::from("/agent"),
            Some(24),
            write_scope,
        );
        (list, sm)
    }

    #[test]
    fn relative_string_matches_node_semantics() {
        assert_eq!(
            relative_string(Path::new("/agent"), Path::new("/agent/extensions/foo.ts")),
            "extensions/foo.ts"
        );
        assert_eq!(
            relative_string(Path::new("/a/b"), Path::new("/a/c/d.ts")),
            "../c/d.ts"
        );
        assert_eq!(relative_string(Path::new("/a"), Path::new("/a")), "");
    }

    #[test]
    fn pattern_targets_strip_override_prefixes() {
        assert_eq!(pattern_entry_target("+foo"), "foo");
        assert_eq!(pattern_entry_target("-foo"), "foo");
        assert_eq!(pattern_entry_target("!foo"), "foo");
        assert_eq!(pattern_entry_target("foo"), "foo");
    }

    #[test]
    fn override_state_resolution() {
        let patterns: HashSet<String> = ["foo.ts".to_owned()].into_iter().collect();
        assert_eq!(
            override_state_from_entries(&[], &patterns, true),
            ProjectOverrideState::Unload
        );
        assert_eq!(
            override_state_from_entries(&[], &patterns, false),
            ProjectOverrideState::Inherit
        );
        assert_eq!(
            override_state_from_entries(&["+foo.ts".to_owned()], &patterns, false),
            ProjectOverrideState::Load
        );
        assert_eq!(
            override_state_from_entries(&["-foo.ts".to_owned()], &patterns, false),
            ProjectOverrideState::Unload
        );
        assert_eq!(
            override_state_from_entries(&["other".to_owned()], &patterns, false),
            ProjectOverrideState::Inherit
        );
    }

    #[test]
    fn groups_use_skill_folder_names_and_sorted_subgroups() {
        let paths = user_extension_paths();
        let groups = build_groups(&paths.global, Path::new("/agent"));
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.subgroups.len(), 2);
        assert_eq!(group.subgroups[0].resource_type, ResourceType::Extensions);
        assert_eq!(group.subgroups[1].resource_type, ResourceType::Skills);
        // SKILL.md items display the parent folder name.
        assert_eq!(group.subgroups[1].items[0].display_name, "writer");
        assert_eq!(group.subgroups[0].items[0].display_name, "foo.ts");
    }

    #[test]
    fn global_toggle_writes_disable_pattern() {
        let (mut list, sm) = make_list(ConfigWriteScope::Global);
        // Space toggles the first item (extensions/foo.ts, enabled -> disabled).
        list.handle_input(" ");
        let settings = sm.lock();
        let extensions = string_array(settings.global_settings().0.get("extensions"));
        assert_eq!(extensions, vec!["-extensions/foo.ts".to_owned()]);
    }

    #[test]
    fn project_override_cycles_from_inherit_to_unload() {
        let (mut list, sm) = make_list(ConfigWriteScope::Project);
        list.handle_input(" ");
        let settings = sm.lock();
        let extensions = string_array(settings.project_settings().0.get("extensions"));
        // Inherited-global item: absolute path pinned, then unload override.
        assert_eq!(
            extensions,
            vec![
                "/agent/extensions/foo.ts".to_owned(),
                "-/agent/extensions/foo.ts".to_owned(),
            ]
        );
    }

    #[test]
    fn search_filters_items() {
        let (mut list, _sm) = make_list(ConfigWriteScope::Global);
        assert_eq!(list.filtered_items.len(), 5); // group + 2 subgroups + 2 items
        for ch in "writer".chars() {
            list.handle_input(&ch.to_string());
        }
        // Only the skills subtree remains: group + subgroup + item.
        assert_eq!(list.filtered_items.len(), 3);
        assert!(matches!(list.filtered_items[2], FlatEntry::Item(0, _, 0)));
    }
}
