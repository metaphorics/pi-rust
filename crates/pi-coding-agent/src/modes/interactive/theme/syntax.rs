//! Syntax highlighting on the theme's flat scope namespace.
//!
//! Port of `theme.ts` `highlightCode`/`getLanguageFromPath` +
//! `buildCliHighlightTheme`.

use std::sync::LazyLock;

use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use super::{Theme, ThemeColor, theme};

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// The sole bridge between syntect scopes and pi's cli-highlight theme keys.
/// Earlier entries are more specific and therefore win over broad scopes.
const SCOPE_COLOR_TABLE: &[(&str, ThemeColor)] = &[
    ("keyword.operator", ThemeColor::SyntaxOperator),
    ("entity.name.function", ThemeColor::SyntaxFunction),
    ("support.function", ThemeColor::SyntaxType),
    ("meta.function", ThemeColor::SyntaxFunction),
    ("entity.name.class", ThemeColor::SyntaxType),
    ("entity.name.type", ThemeColor::SyntaxType),
    ("storage.type", ThemeColor::SyntaxType),
    ("support.type", ThemeColor::SyntaxType),
    ("constant.numeric", ThemeColor::SyntaxNumber),
    ("constant.language", ThemeColor::SyntaxNumber),
    ("constant", ThemeColor::SyntaxNumber),
    ("string.regexp", ThemeColor::SyntaxString),
    ("string", ThemeColor::SyntaxString),
    ("comment", ThemeColor::SyntaxComment),
    ("keyword", ThemeColor::SyntaxKeyword),
    ("storage", ThemeColor::SyntaxKeyword),
    ("variable", ThemeColor::SyntaxVariable),
    ("entity.other.attribute-name", ThemeColor::SyntaxVariable),
    ("operator", ThemeColor::SyntaxOperator),
    ("punctuation", ThemeColor::SyntaxPunctuation),
    ("entity.name.tag", ThemeColor::SyntaxPunctuation),
    ("entity.name", ThemeColor::SyntaxKeyword),
    ("meta", ThemeColor::Muted),
    ("markup.inserted", ThemeColor::ToolDiffAdded),
    ("markup.deleted", ThemeColor::ToolDiffRemoved),
];

fn color_for_scopes(scopes: &ScopeStack) -> Option<ThemeColor> {
    scopes.as_slice().iter().rev().find_map(|scope| {
        let name = scope.to_string();
        SCOPE_COLOR_TABLE
            .iter()
            .find_map(|(selector, color)| name.contains(selector).then_some(*color))
    })
}

fn style_for_scopes(scopes: &ScopeStack, text: &str, theme: &Theme) -> String {
    let names: Vec<String> = scopes.as_slice().iter().map(ToString::to_string).collect();
    let mut styled = match color_for_scopes(scopes) {
        Some(color) => theme.fg(color, text),
        None => text.to_owned(),
    };
    if names.iter().any(|name| name.contains("markup.italic")) {
        styled = theme.italic(&styled);
    }
    if names.iter().any(|name| name.contains("markup.bold")) {
        styled = theme.bold(&styled);
    }
    if names
        .iter()
        .any(|name| name.contains("markup.underline") || name.contains("markup.link"))
    {
        styled = theme.underline(&styled);
    }
    styled
}

fn highlight_line(
    line: &str,
    state: &mut ParseState,
    scopes: &mut ScopeStack,
    theme: &Theme,
) -> Option<String> {
    let source = format!("{line}\n");
    let operations = state.parse_line(&source, &SYNTAX_SET).ok()?;
    let mut output = String::with_capacity(line.len());
    let mut start = 0;
    for (offset, operation) in operations {
        let end = offset.min(line.len());
        output.push_str(&style_for_scopes(scopes, &line[start..end], theme));
        scopes.apply(&operation).ok()?;
        start = end;
    }
    output.push_str(&style_for_scopes(scopes, &line[start..], theme));
    Some(output)
}

/// Oracle `highlightCode`: styled lines for a code block.
///
/// No valid language → every line colored `mdCodeBlock`; auto-detection is
/// deliberately not attempted. Syntect parse failures use that same fallback.
#[must_use]
pub fn highlight_code(code: &str, lang: Option<&str>) -> Vec<String> {
    let t = theme();
    let lines: Vec<&str> = if code.is_empty() {
        vec![""]
    } else {
        code.split_terminator('\n').collect()
    };
    let fallback = || {
        lines
            .iter()
            .map(|line| t.fg(ThemeColor::MdCodeBlock, line))
            .collect()
    };
    let Some(language) = lang.filter(|language| !language.is_empty()) else {
        return fallback();
    };
    let Some(syntax) = SYNTAX_SET.find_syntax_by_token(language) else {
        return fallback();
    };

    let mut state = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut highlighted = Vec::with_capacity(lines.len());
    for line in &lines {
        let Some(rendered) = highlight_line(line, &mut state, &mut scopes, &t) else {
            return fallback();
        };
        highlighted.push(rendered);
    }
    highlighted
}

/// Oracle `getLanguageFromPath`: language identifier from a file extension.
#[must_use]
pub fn get_language_from_path(file_path: &str) -> Option<&'static str> {
    let ext = file_path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    })
}
