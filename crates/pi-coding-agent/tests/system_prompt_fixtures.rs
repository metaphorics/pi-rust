//! Fixture / golden tests for `build_system_prompt` and `format_skills_for_prompt`.
//!
//! Goldens are hand-traced from oracle
//! `packages/coding-agent/src/core/system-prompt.ts` and
//! `packages/coding-agent/src/core/skills.ts`.

use pi_coding_agent::{
    BuildSystemPromptOptions, ContextFile, Skill, build_system_prompt, format_skills_for_prompt,
    get_docs_path, get_examples_path, get_readme_path,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn default_snippets() -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("read".into(), "Read file contents".into());
    m.insert("bash".into(), "Execute shell commands".into());
    m.insert("edit".into(), "Edit existing files".into());
    m.insert("write".into(), "Write new files".into());
    m
}

fn sample_skill() -> Skill {
    Skill {
        name: "demo-skill".into(),
        description: "A demo skill for fixtures".into(),
        file_path: PathBuf::from("/tmp/skills/demo-skill/SKILL.md"),
        base_dir: PathBuf::from("/tmp/skills/demo-skill"),
        disable_model_invocation: false,
    }
}

/// (1) default prompt with tools read/bash/edit/write + snippets.
#[test]
fn default_prompt_with_core_tools() {
    let tools = vec![
        "read".into(),
        "bash".into(),
        "edit".into(),
        "write".into(),
    ];
    let opts = BuildSystemPromptOptions {
        selected_tools: Some(tools),
        tool_snippets: default_snippets(),
        cwd: r"C:\work\proj".into(), // backslash → slash
        ..Default::default()
    };

    let prompt = build_system_prompt(&opts);

    let readme = get_readme_path().display().to_string();
    let docs = get_docs_path().display().to_string();
    let examples = get_examples_path().display().to_string();

    let expected = format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\
\n\
Available tools:\n\
- read: Read file contents\n\
- bash: Execute shell commands\n\
- edit: Edit existing files\n\
- write: Write new files\n\
\n\
In addition to the tools above, you may have access to other custom tools depending on the project.\n\
\n\
Guidelines:\n\
- Use bash for file operations like ls, rg, find\n\
- Be concise in your responses\n\
- Show file paths clearly when working with files\n\
\n\
Pi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):\n\
- Main documentation: {readme}\n\
- Additional docs: {docs}\n\
- Examples: {examples} (extensions, custom tools, SDK)\n\
- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n\
- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md)\n\
- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing\n\
- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)\n\
Current working directory: C:/work/proj"
    );

    assert_eq!(prompt, expected);
    assert!(prompt.contains("You are an expert coding assistant operating inside pi, a coding agent harness."));
    assert!(prompt.contains("- Be concise in your responses"));
    assert!(prompt.contains("- Show file paths clearly when working with files"));
}

/// (2) bash-only tool set includes the file-ops guideline; grep/find/ls suppress it.
#[test]
fn bash_file_ops_guideline_conditional() {
    let mut snippets = HashMap::new();
    snippets.insert("bash".into(), "Execute shell commands".into());

    let bash_only = BuildSystemPromptOptions {
        selected_tools: Some(vec!["bash".into()]),
        tool_snippets: snippets.clone(),
        cwd: "/tmp".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&bash_only);
    assert!(
        p.contains("- Use bash for file operations like ls, rg, find"),
        "bash-only must include file-ops guideline:\n{p}"
    );

    // Presence of grep suppresses (even without snippet — guideline uses `tools.includes`).
    let with_grep = BuildSystemPromptOptions {
        selected_tools: Some(vec!["bash".into(), "grep".into()]),
        tool_snippets: snippets.clone(),
        cwd: "/tmp".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&with_grep);
    assert!(
        !p.contains("Use bash for file operations like ls, rg, find"),
        "grep presence must suppress file-ops guideline:\n{p}"
    );

    let with_find = BuildSystemPromptOptions {
        selected_tools: Some(vec!["bash".into(), "find".into()]),
        tool_snippets: snippets.clone(),
        cwd: "/tmp".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&with_find);
    assert!(!p.contains("Use bash for file operations like ls, rg, find"));

    let with_ls = BuildSystemPromptOptions {
        selected_tools: Some(vec!["bash".into(), "ls".into()]),
        tool_snippets: snippets,
        cwd: "/tmp".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&with_ls);
    assert!(!p.contains("Use bash for file operations like ls, rg, find"));
}

/// (3) custom prompt assembly order: base→append→project_context→skills→cwd footer;
/// skills omitted when selected_tools lacks `read`.
#[test]
fn custom_prompt_assembly_order_and_skills_gate() {
    let skill = sample_skill();
    let ctx = ContextFile {
        path: "/proj/AGENTS.md".into(),
        content: "Always use tabs.".into(),
    };

    // With read: skills included.
    let with_read = BuildSystemPromptOptions {
        custom_prompt: Some("CUSTOM BASE".into()),
        append_system_prompt: Some("APPEND TEXT".into()),
        selected_tools: Some(vec!["read".into(), "bash".into()]),
        context_files: vec![ctx.clone()],
        skills: vec![skill.clone()],
        cwd: "/home/u/work".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&with_read);

    let skills_section = format_skills_for_prompt(std::slice::from_ref(&skill));
    let expected = format!(
        "CUSTOM BASE\n\
\n\
APPEND TEXT\n\
\n\
<project_context>\n\
\n\
Project-specific instructions and guidelines:\n\
\n\
<project_instructions path=\"/proj/AGENTS.md\">\n\
Always use tabs.\n\
</project_instructions>\n\
\n\
</project_context>\n\
{skills_section}\n\
Current working directory: /home/u/work"
    );
    assert_eq!(p, expected);

    // Without read: skills omitted.
    let no_read = BuildSystemPromptOptions {
        custom_prompt: Some("CUSTOM BASE".into()),
        append_system_prompt: Some("APPEND TEXT".into()),
        selected_tools: Some(vec!["bash".into()]),
        context_files: vec![ctx],
        skills: vec![skill],
        cwd: "/home/u/work".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&no_read);
    assert!(
        !p.contains("<available_skills>"),
        "skills must be omitted when read is not selected:\n{p}"
    );
    assert!(p.contains("<project_context>"));
    assert!(p.ends_with("Current working directory: /home/u/work"));
    // Order: base, append, project_context, cwd (no skills)
    let base_i = p.find("CUSTOM BASE").unwrap();
    let append_i = p.find("APPEND TEXT").unwrap();
    let ctx_i = p.find("<project_context>").unwrap();
    let cwd_i = p.find("Current working directory:").unwrap();
    assert!(base_i < append_i && append_i < ctx_i && ctx_i < cwd_i);
}

/// (4) context files render as exact project_instructions blocks inside project_context.
#[test]
fn context_files_render_exact_xml() {
    let opts = BuildSystemPromptOptions {
        selected_tools: Some(vec!["read".into()]),
        tool_snippets: {
            let mut m = HashMap::new();
            m.insert("read".into(), "Read file contents".into());
            m
        },
        context_files: vec![
            ContextFile {
                path: "/a/AGENTS.md".into(),
                content: "line1\nline2".into(),
            },
            ContextFile {
                path: "/b/CLAUDE.md".into(),
                content: "other".into(),
            },
        ],
        cwd: "/cwd".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&opts);

    let block = "\n\n<project_context>\n\n\
Project-specific instructions and guidelines:\n\n\
<project_instructions path=\"/a/AGENTS.md\">\n\
line1\nline2\n\
</project_instructions>\n\n\
<project_instructions path=\"/b/CLAUDE.md\">\n\
other\n\
</project_instructions>\n\n\
</project_context>\n";
    assert!(
        p.contains(block),
        "missing exact project_context block.\n---prompt---\n{p}\n---expected block---\n{block}"
    );
}

/// (5) skills section matches formatSkillsForPrompt oracle text for one sample skill.
#[test]
fn skills_section_matches_oracle_format() {
    let skill = sample_skill();
    let section = format_skills_for_prompt(&[skill]);
    let expected = [
        "\n\nThe following skills provide specialized instructions for specific tasks.",
        "Use the read tool to load a skill's file when the task matches its description.",
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
        "",
        "<available_skills>",
        "  <skill>",
        "    <name>demo-skill</name>",
        "    <description>A demo skill for fixtures</description>",
        "    <location>/tmp/skills/demo-skill/SKILL.md</location>",
        "  </skill>",
        "</available_skills>",
    ]
    .join("\n");
    assert_eq!(section, expected);

    // Also appears in the default prompt when read is selected.
    let opts = BuildSystemPromptOptions {
        selected_tools: Some(vec!["read".into()]),
        tool_snippets: {
            let mut m = HashMap::new();
            m.insert("read".into(), "Read file contents".into());
            m
        },
        skills: vec![sample_skill()],
        cwd: "/cwd".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&opts);
    assert!(p.contains(&expected));
    assert!(p.ends_with("\nCurrent working directory: /cwd"));
}

#[test]
fn empty_tool_snippets_yields_none() {
    let opts = BuildSystemPromptOptions {
        selected_tools: Some(vec!["read".into(), "bash".into()]),
        tool_snippets: HashMap::new(),
        cwd: "/x".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&opts);
    assert!(p.contains("Available tools:\n(none)\n"));
}

#[test]
fn prompt_guidelines_dedupe_and_trim() {
    let opts = BuildSystemPromptOptions {
        selected_tools: Some(vec!["bash".into()]),
        tool_snippets: {
            let mut m = HashMap::new();
            m.insert("bash".into(), "Execute shell commands".into());
            m
        },
        prompt_guidelines: vec![
            "  Be concise in your responses  ".into(), // duplicate of always-guideline after trim
            "Prefer small diffs".into(),
            "Prefer small diffs".into(), // exact dup
            "   ".into(),                // empty after trim
        ],
        cwd: "/x".into(),
        ..Default::default()
    };
    let p = build_system_prompt(&opts);
    // Always-guidelines appear once; custom once.
    assert_eq!(
        p.matches("- Be concise in your responses").count(),
        1,
        "{p}"
    );
    assert_eq!(p.matches("- Prefer small diffs").count(), 1, "{p}");
    // Order: bash file-ops, then prompt_guidelines (Be concise first → Prefer small diffs),
    // then remaining always-guidelines (Be concise already present so skipped).
    let g_start = p.find("Guidelines:\n").unwrap() + "Guidelines:\n".len();
    let g_end = p.find("\n\nPi documentation").unwrap();
    let guidelines = &p[g_start..g_end];
    assert_eq!(
        guidelines,
        "- Use bash for file operations like ls, rg, find\n\
- Be concise in your responses\n\
- Prefer small diffs\n\
- Show file paths clearly when working with files"
    );
}
