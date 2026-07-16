//! System prompt construction and project context loading.
//!
//! Port of `packages/coding-agent/src/core/system-prompt.ts` (`buildSystemPrompt`)
//! and skills formatting from `packages/coding-agent/src/core/skills.ts`
//! (`formatSkillsForPrompt`).

use crate::config::get_package_dir;
use crate::resource_loader::load_project_context_file_paths;
use crate::source_info::SourceInfo;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Skill metadata used when formatting the skills section of the system prompt.
///
/// Port of the fields from `skills.ts` `Skill` that `formatSkillsForPrompt`
/// and the RPC `get_commands` wire surface need.
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    /// Provenance (oracle `Skill.sourceInfo`), set by the loading layer.
    pub source_info: SourceInfo,
    /// When true, the skill is hidden from the model-facing skills list.
    pub disable_model_invocation: bool,
}

impl Default for Skill {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            file_path: PathBuf::new(),
            base_dir: PathBuf::new(),
            source_info: SourceInfo::default(),
            disable_model_invocation: false,
        }
    }
}

/// A loaded project context file (AGENTS.md / CLAUDE.md).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

/// Options for [`build_system_prompt`].
///
/// Port of `BuildSystemPromptOptions` from `system-prompt.ts`.
#[derive(Clone, Debug, Default)]
pub struct BuildSystemPromptOptions {
    /// Custom system prompt (replaces default when non-empty).
    pub custom_prompt: Option<String>,
    /// Tools to include in prompt. Default: `[read, bash, edit, write]`.
    pub selected_tools: Option<Vec<String>>,
    /// Optional one-line tool snippets keyed by tool name.
    /// Visible tool order follows `selected_tools` (not map iteration order).
    pub tool_snippets: HashMap<String, String>,
    /// Additional guideline bullets appended to the default system prompt guidelines.
    pub prompt_guidelines: Vec<String>,
    /// Text to append to system prompt.
    pub append_system_prompt: Option<String>,
    /// Working directory.
    pub cwd: String,
    /// Pre-loaded context files.
    pub context_files: Vec<ContextFile>,
    /// Pre-loaded skills.
    pub skills: Vec<Skill>,
}

/// Path to package `README.md` (`getPackageDir()/README.md`, resolved).
pub fn get_readme_path() -> PathBuf {
    get_package_dir().join("README.md")
}

/// Path to package `docs` directory.
pub fn get_docs_path() -> PathBuf {
    get_package_dir().join("docs")
}

/// Path to package `examples` directory.
pub fn get_examples_path() -> PathBuf {
    get_package_dir().join("examples")
}

/// Escape XML special characters (oracle `escapeXml` in `skills.ts`).
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Format skills for inclusion in a system prompt.
///
/// Byte-compatible with oracle `formatSkillsForPrompt` in `skills.ts`.
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible_skills: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();

    if visible_skills.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];

    for skill in visible_skills {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }

    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn append_project_context(prompt: &mut String, context_files: &[ContextFile]) {
    if context_files.is_empty() {
        return;
    }
    prompt.push_str("\n\n<project_context>\n\n");
    prompt.push_str("Project-specific instructions and guidelines:\n\n");
    for file in context_files {
        prompt.push_str(&format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
            file.path, file.content
        ));
    }
    prompt.push_str("</project_context>\n");
}

/// Build the system prompt with tools, guidelines, and context.
///
/// Exact port of oracle `buildSystemPrompt`.
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    let prompt_cwd = options.cwd.replace('\\', "/");

    // JS truthiness: empty string / undefined → no append section.
    let append_section = match options.append_system_prompt.as_deref() {
        Some(s) if !s.is_empty() => format!("\n\n{s}"),
        _ => String::new(),
    };

    let context_files = &options.context_files;
    let skills = &options.skills;

    // JS: `if (customPrompt)` — empty string is falsy.
    if let Some(custom_prompt) = options.custom_prompt.as_deref().filter(|s| !s.is_empty()) {
        let mut prompt = custom_prompt.to_string();

        if !append_section.is_empty() {
            prompt.push_str(&append_section);
        }

        append_project_context(&mut prompt, context_files);

        // Append skills section (only if read tool is available)
        let custom_prompt_has_read = match &options.selected_tools {
            None => true,
            Some(tools) => tools.iter().any(|t| t == "read"),
        };
        if custom_prompt_has_read && !skills.is_empty() {
            prompt.push_str(&format_skills_for_prompt(skills));
        }

        prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}"));
        return prompt;
    }

    // Get absolute paths to documentation and examples
    let readme_path = get_readme_path();
    let docs_path = get_docs_path();
    let examples_path = get_examples_path();

    // Build tools list based on selected tools.
    // A tool appears in Available tools only when the caller provides a one-line snippet.
    let default_tools = ["read", "bash", "edit", "write"];
    let tools: Vec<&str> = match &options.selected_tools {
        Some(t) => t.iter().map(String::as_str).collect(),
        None => default_tools.to_vec(),
    };
    let visible_tools: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|name| {
            // JS `!!toolSnippets?.[name]` — missing/empty string are falsy.
            options
                .tool_snippets
                .get(*name)
                .is_some_and(|snippet| !snippet.is_empty())
        })
        .collect();

    let tools_list = if visible_tools.is_empty() {
        "(none)".to_string()
    } else {
        visible_tools
            .iter()
            .map(|name| {
                let snippet = options.tool_snippets.get(*name).expect("filtered");
                format!("- {name}: {snippet}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Build guidelines based on which tools are actually available
    let mut guidelines_list: Vec<String> = Vec::new();
    let mut guidelines_set: HashSet<String> = HashSet::new();
    let mut add_guideline = |guideline: &str| {
        if guidelines_set.contains(guideline) {
            return;
        }
        guidelines_set.insert(guideline.to_string());
        guidelines_list.push(guideline.to_string());
    };

    let has_bash = tools.contains(&"bash");
    let has_grep = tools.contains(&"grep");
    let has_find = tools.contains(&"find");
    let has_ls = tools.contains(&"ls");
    let has_read = tools.contains(&"read");

    // File exploration guidelines
    if has_bash && !has_grep && !has_find && !has_ls {
        add_guideline("Use bash for file operations like ls, rg, find");
    }

    for guideline in &options.prompt_guidelines {
        let normalized = guideline.trim();
        if !normalized.is_empty() {
            add_guideline(normalized);
        }
    }

    // Always include these
    add_guideline("Be concise in your responses");
    add_guideline("Show file paths clearly when working with files");

    let guidelines = guidelines_list
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\
\n\
Available tools:\n\
{tools_list}\n\
\n\
In addition to the tools above, you may have access to other custom tools depending on the project.\n\
\n\
Guidelines:\n\
{guidelines}\n\
\n\
Pi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):\n\
- Main documentation: {readme}\n\
- Additional docs: {docs}\n\
- Examples: {examples} (extensions, custom tools, SDK)\n\
- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n\
- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md)\n\
- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing\n\
- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
        readme = readme_path.display(),
        docs = docs_path.display(),
        examples = examples_path.display(),
    );

    if !append_section.is_empty() {
        prompt.push_str(&append_section);
    }

    append_project_context(&mut prompt, context_files);

    // Append skills section (only if read tool is available)
    if has_read && !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }

    prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}"));
    prompt
}

/// Load AGENTS.md / CLAUDE.md content from agent dir + cwd ancestors.
///
/// Ordering and discovery mirror oracle `loadProjectContextFiles` via
/// [`load_project_context_file_paths`]. Unreadable files are skipped.
pub fn load_project_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let paths = load_project_context_file_paths(cwd, agent_dir);
    let mut out = Vec::new();
    for rp in paths {
        match fs::read_to_string(&rp.path) {
            Ok(content) => {
                out.push(ContextFile {
                    path: rp.path.to_string_lossy().into_owned(),
                    content,
                });
            }
            Err(_) => {
                // Skip unreadable (oracle logs a warning and continues for path discovery;
                // content load failures are skipped here).
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_xml_covers_all_entities() {
        assert_eq!(
            escape_xml(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[test]
    fn format_skills_empty_when_all_disabled() {
        let skills = vec![Skill {
            name: "x".into(),
            description: "y".into(),
            file_path: PathBuf::from("/s/SKILL.md"),
            base_dir: PathBuf::from("/s"),
            source_info: Default::default(),
            disable_model_invocation: true,
        }];
        assert_eq!(format_skills_for_prompt(&skills), "");
    }
}
