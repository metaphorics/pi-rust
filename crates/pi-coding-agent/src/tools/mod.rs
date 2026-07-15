//! Built-in filesystem and shell tools, ported from pi 0.80.7.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use pi_agent::{AgentToolResult, ToolDefinition, ToolExecutionMode};
use pi_ai::{Content, ImageContent, TextContent};
use regex::RegexBuilder;
use serde_json::{Value, json};

const MAX_LINES: usize = 2_000;
const MAX_BYTES: usize = 50 * 1024;
const GREP_MAX_LINE_LENGTH: usize = 500;

fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn text(result: impl Into<String>) -> Result<AgentToolResult, String> {
    Ok(AgentToolResult::text(result))
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Invalid {name}: expected a string"))
}

fn limit_arg(args: &Value, name: &str) -> Option<usize> {
    args.get(name)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn truncate_head(contents: &str, max_lines: usize, max_bytes: usize) -> (String, bool, usize) {
    let total_lines = contents.lines().count();
    let mut used = 0;
    let mut lines = Vec::new();
    let mut truncated = false;
    for (index, line) in contents.split_inclusive('\n').enumerate() {
        if index >= max_lines || used + line.len() > max_bytes {
            truncated = true;
            break;
        }
        used += line.len();
        lines.push(line);
    }
    if !truncated && used < contents.len() {
        truncated = true;
    }
    (lines.concat(), truncated, total_lines)
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// JSON schema copied from `core/tools/read.ts`.
pub fn read_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to read (relative or absolute)"},"offset":{"type":"number","description":"Line number to start reading from (1-indexed)"},"limit":{"type":"number","description":"Maximum number of lines to read"}},"required":["path"]})
}

pub fn create_read_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "read".into(), label: "read".into(),
        description: "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.".into(),
        parameters: read_schema(), execution_mode: None, prepare_arguments: None, renderer: None,
        execute: Arc::new(move |_, args, _, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let raw = string_arg(&args, "path")?;
                let target = resolve(&cwd, raw);
                if let Some(mime_type) = image_mime(&target) {
                    let data = fs::read(&target).map_err(|error| error.to_string())?;
                    return Ok(AgentToolResult {
                        content: vec![
                            Content::Text(TextContent { text: format!("Read image file [{mime_type}]").into(), text_signature: None }),
                            Content::Image(ImageContent { data: BASE64.encode(data), mime_type: mime_type.to_owned() }),
                        ],
                        details: Value::Object(Default::default()),
                        added_tool_names: None,
                        terminate: None,
                    });
                }
                let contents = fs::read_to_string(target).map_err(|error| error.to_string())?;
                let all: Vec<&str> = contents.lines().collect();
                let offset = limit_arg(&args, "offset").unwrap_or(1);
                let start = offset.saturating_sub(1);
                if start >= all.len() { return Err(format!("Offset {offset} is beyond end of file ({} lines total)", all.len())); }
                let requested = limit_arg(&args, "limit").unwrap_or(MAX_LINES);
                let selected = all[start..].iter().take(requested).copied().collect::<Vec<_>>().join("\n");
                let (mut output, truncated, total) = truncate_head(&selected, requested, MAX_BYTES);
                let shown = output.lines().count();
                if truncated || start + shown < total {
                    let next = start + shown + 1;
                    output.push_str(&format!("\n\n[Showing lines {}-{} of {total}. Use offset={next} to continue.]", start + 1, start + shown));
                }
                text(output)
            })
        }),
    }
}

/// JSON schema copied from `core/tools/write.ts`.
pub fn write_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}},"required":["path","content"]})
}

pub fn create_write_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "write".into(), label: "write".into(),
        description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
        parameters: write_schema(), execution_mode: None, prepare_arguments: None, renderer: None,
        execute: Arc::new(move |_, args, _, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = string_arg(&args, "path")?;
                let contents = string_arg(&args, "content")?;
                let target = resolve(&cwd, path);
                if let Some(parent) = target.parent() { fs::create_dir_all(parent).map_err(|error| error.to_string())?; }
                fs::write(target, contents).map_err(|error| error.to_string())?;
                text(format!("Successfully wrote {} bytes to {path}", contents.len()))
            })
        }),
    }
}

/// JSON schema copied from `core/tools/edit.ts`.
pub fn edit_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"edits":{"type":"array","items":{"type":"object","properties":{"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}},"required":["oldText","newText"]},"description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."}},"required":["path","edits"]})
}

pub fn create_edit_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "edit".into(), label: "edit".into(),
        description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".into(),
        parameters: edit_schema(), execution_mode: None, prepare_arguments: None, renderer: None,
        execute: Arc::new(move |_, args, _, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let path = string_arg(&args, "path")?;
                let edits = args.get("edits").and_then(Value::as_array).ok_or_else(|| "Edit tool input is invalid. edits must contain at least one replacement.".to_string())?;
                if edits.is_empty() { return Err("Edit tool input is invalid. edits must contain at least one replacement.".into()); }
                let target = resolve(&cwd, path);
                let original = fs::read_to_string(&target).map_err(|error| format!("Could not edit file: {path}. Error: {error}."))?;
                let bom = original.strip_prefix('\u{feff}').map_or("", |_| "\u{feff}");
                let normalized = original.trim_start_matches('\u{feff}').replace("\r\n", "\n");
                let mut matches = Vec::with_capacity(edits.len());
                for (index, edit) in edits.iter().enumerate() {
                    let old = edit.get("oldText").and_then(Value::as_str).ok_or_else(|| format!("edits[{index}].oldText must not be empty in {path}."))?;
                    if old.is_empty() { return Err(if edits.len() == 1 { format!("oldText must not be empty in {path}.") } else { format!("edits[{index}].oldText must not be empty in {path}.") }); }
                    let old = old.replace("\r\n", "\n");
                    let positions: Vec<_> = normalized.match_indices(&old).collect();
                    if positions.is_empty() { return Err(if edits.len() == 1 { format!("Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines.") } else { format!("Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines.") }); }
                    if positions.len() > 1 { return Err(if edits.len() == 1 { format!("Found {} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique.", positions.len()) } else { format!("Found {} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique.", positions.len()) }); }
                    let replacement = edit.get("newText").and_then(Value::as_str).ok_or_else(|| format!("Invalid edits[{index}].newText: expected a string"))?.replace("\r\n", "\n");
                    matches.push((positions[0].0, old.len(), replacement, index));
                }
                matches.sort_by_key(|entry| entry.0);
                for pair in matches.windows(2) { if pair[0].0 + pair[0].1 > pair[1].0 { return Err(format!("edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.", pair[0].3, pair[1].3)); } }
                let mut changed = normalized.clone();
                for (position, length, replacement, _) in matches.iter().rev() { changed.replace_range(*position..position + length, replacement); }
                if changed == normalized { return Err(if edits.len() == 1 { format!("No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.") } else { format!("No changes made to {path}. The replacements produced identical content.") }); }
                let changed = if original.contains("\r\n") { changed.replace('\n', "\r\n") } else { changed };
                fs::write(target, format!("{bom}{changed}")).map_err(|error| error.to_string())?;
                text(format!("Successfully replaced {} block(s) in {path}.", edits.len()))
            })
        }),
    }
}

/// JSON schema copied from `core/tools/bash.ts`.
pub fn bash_schema() -> Value {
    json!({"type":"object","properties":{"command":{"type":"string","description":"Bash command to execute"},"timeout":{"type":"number","description":"Timeout in seconds (optional, no default timeout)"}},"required":["command"]})
}

pub fn create_bash_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "bash".into(), label: "bash".into(),
        description: "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.".into(),
        parameters: bash_schema(), execution_mode: Some(ToolExecutionMode::Sequential), prepare_arguments: None, renderer: None,
        execute: Arc::new(move |_, args, _, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let command = string_arg(&args, "command")?;
                let timeout = args.get("timeout").and_then(Value::as_f64);
                if let Some(seconds) = timeout
                    && (!seconds.is_finite() || seconds <= 0.0)
                {
                    return Err("Invalid timeout: must be a finite number of seconds".into());
                }
                if !cwd.is_dir() { return Err(format!("Working directory does not exist: {}\nCannot execute bash commands.", cwd.display())); }
                let child = tokio::process::Command::new("bash").arg("-c").arg(command).current_dir(&cwd).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|error| error.to_string())?;
                let output = if let Some(seconds) = timeout { match tokio::time::timeout(std::time::Duration::from_secs_f64(seconds), child.wait_with_output()).await { Ok(output) => output.map_err(|error| error.to_string())?, Err(_) => return text(format!("\nCommand timed out after {seconds} seconds")), } } else { child.wait_with_output().await.map_err(|error| error.to_string())? };
                let mut all = String::from_utf8_lossy(&output.stdout).into_owned();
                all.push_str(&String::from_utf8_lossy(&output.stderr));
                let (shown, truncated, _) = truncate_head(&all, MAX_LINES, MAX_BYTES);
                let mut result = shown;
                if !output.status.success() { result.push_str(&format!("\n\nCommand exited with code {}", output.status.code().unwrap_or(-1))); }
                if truncated { result.push_str("\n\n[Output truncated]"); }
                text(result)
            })
        }),
    }
}

/// JSON schema copied from `core/tools/grep.ts`.
pub fn grep_schema() -> Value {
    json!({"type":"object","properties":{"pattern":{"type":"string","description":"Search pattern (regex or literal string)"},"path":{"type":"string","description":"Directory or file to search (default: current directory)"},"glob":{"type":"string","description":"Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},"ignoreCase":{"type":"boolean","description":"Case-insensitive search (default: false)"},"literal":{"type":"boolean","description":"Treat pattern as literal string instead of regex (default: false)"},"context":{"type":"number","description":"Number of lines to show before and after each match (default: 0)"},"limit":{"type":"number","description":"Maximum number of matches to return (default: 100)"}},"required":["pattern"]})
}

fn file_set(root: &Path, glob: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let matcher = match glob {
        Some(pattern) => {
            let mut builder = GlobSetBuilder::new();
            builder.add(Glob::new(pattern).map_err(|error| error.to_string())?);
            Some(builder.build().map_err(|error| error.to_string())?)
        }
        None => None,
    };
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
        return Ok(files);
    }
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .build()
    {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && matcher
                .as_ref()
                .is_none_or(|set| set.is_match(entry.path()))
        {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}

pub fn create_grep_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "grep".into(), label: "grep".into(),
        description: "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars.".into(),
        parameters: grep_schema(), execution_mode: None, prepare_arguments: None, renderer: None,
        execute: Arc::new(move |_, args, _, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let pattern = string_arg(&args, "pattern")?;
                let root = resolve(&cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
                if !root.exists() { return Err(format!("Path not found: {}", root.display())); }
                let limit = limit_arg(&args, "limit").unwrap_or(100);
                let literal = args.get("literal").and_then(Value::as_bool).unwrap_or(false);
                let pattern = if literal { regex::escape(pattern) } else { pattern.to_owned() };
                let regex = RegexBuilder::new(&pattern).case_insensitive(args.get("ignoreCase").and_then(Value::as_bool).unwrap_or(false)).build().map_err(|error| error.to_string())?;
                let mut found = Vec::new();
                for file in file_set(&root, args.get("glob").and_then(Value::as_str))? {
                    let Ok(contents) = fs::read_to_string(&file) else { continue };
                    for (line_number, line) in contents.lines().enumerate() {
                        if regex.is_match(line) {
                            let relative = file.strip_prefix(&cwd).unwrap_or(&file).display().to_string().replace('\\', "/");
                            let line = if line.chars().count() > GREP_MAX_LINE_LENGTH { format!("{}...", line.chars().take(GREP_MAX_LINE_LENGTH).collect::<String>()) } else { line.to_string() };
                            found.push(format!("{relative}:{}:{line}", line_number + 1));
                            if found.len() >= limit { break; }
                        }
                    }
                    if found.len() >= limit { break; }
                }
                if found.is_empty() {
                    text("No matches found")
                } else {
                    let (mut value, truncated, _) = truncate_head(&found.join("\n"), usize::MAX, MAX_BYTES);
                    if found.len() >= limit {
                        value.push_str(&format!("\n\n{limit} matches limit reached. Use limit={} for more, or refine pattern.", limit * 2));
                    }
                    if truncated {
                        value.push_str("\n\n50KB limit reached");
                    }
                    text(value)
                }
            })
        }),
    }
}

/// JSON schema copied from `core/tools/find.ts`.
pub fn find_schema() -> Value {
    json!({"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},"path":{"type":"string","description":"Directory to search in (default: current directory)"},"limit":{"type":"number","description":"Maximum number of results (default: 1000)"}},"required":["pattern"]})
}

pub fn create_find_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "find".into(), label: "find".into(),
        description: "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first).".into(),
        parameters: find_schema(), execution_mode: None, prepare_arguments: None, renderer: None,
        execute: Arc::new(move |_, args, _, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let pattern = string_arg(&args, "pattern")?;
                let root = resolve(&cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
                if !root.exists() { return Err(format!("Path not found: {}", root.display())); }
                let limit = limit_arg(&args, "limit").unwrap_or(1000);
                let mut builder = GlobSetBuilder::new(); builder.add(Glob::new(pattern).map_err(|error| error.to_string())?); let matcher = builder.build().map_err(|error| error.to_string())?;
                let mut results = Vec::new();
                for entry in WalkBuilder::new(&root).hidden(false).git_ignore(true).git_global(true).build() {
                    let entry = entry.map_err(|error| error.to_string())?;
                    let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                    if !relative.as_os_str().is_empty() && matcher.is_match(relative) { results.push(relative.display().to_string().replace('\\', "/")); if results.len() >= limit { break; } }
                }
                results.sort();
                if results.is_empty() { return text("No files found matching pattern"); }
                let (mut output, truncated, _) = truncate_head(&results.join("\n"), usize::MAX, MAX_BYTES);
                if results.len() >= limit { output.push_str(&format!("\n\n{limit} results limit reached. Use limit={} for more, or refine pattern.", limit * 2)); }
                if truncated { output.push_str("\n\n50KB limit reached"); }
                text(output)
            })
        }),
    }
}

/// JSON schema copied from `core/tools/ls.ts`.
pub fn ls_schema() -> Value {
    json!({"type":"object","properties":{"path":{"type":"string","description":"Directory to list (default: current directory)"},"limit":{"type":"number","description":"Maximum number of entries to return (default: 500)"}}})
}

pub fn create_ls_tool(cwd: impl Into<PathBuf>) -> ToolDefinition {
    let cwd = cwd.into();
    ToolDefinition {
        name: "ls".into(), label: "ls".into(),
        description: "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first).".into(),
        parameters: ls_schema(), execution_mode: None, prepare_arguments: None, renderer: None,
        execute: Arc::new(move |_, args, _, _| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let target = resolve(&cwd, args.get("path").and_then(Value::as_str).unwrap_or("."));
                if !target.exists() { return Err(format!("Path not found: {}", target.display())); }
                if !target.is_dir() { return Err(format!("Not a directory: {}", target.display())); }
                let limit = limit_arg(&args, "limit").unwrap_or(500);
                let mut entries = Vec::new();
                for entry in fs::read_dir(&target).map_err(|error| format!("Cannot read directory: {error}"))? {
                    let entry = entry.map_err(|error| error.to_string())?;
                    let mut name = entry.file_name().to_string_lossy().into_owned();
                    if entry.file_type().map_err(|error| error.to_string())?.is_dir() { name.push('/'); }
                    entries.push(name);
                }
                entries.sort();
                if entries.is_empty() { return text("(empty directory)"); }
                let limited = entries.len() > limit; entries.truncate(limit);
                let (mut output, bytes_limited, _) = truncate_head(&entries.join("\n"), usize::MAX, MAX_BYTES);
                if limited { output.push_str(&format!("\n\n{limit} entries limit reached. Use limit={} for more.", limit * 2)); }
                if bytes_limited { output.push_str("\n\n50KB limit reached"); }
                text(output)
            })
        }),
    }
}

/// Creates the seven first-party tool definitions.
pub fn builtin_tools(cwd: impl Into<PathBuf>) -> Vec<ToolDefinition> {
    let cwd = cwd.into();
    vec![
        create_read_tool(cwd.clone()),
        create_bash_tool(cwd.clone()),
        create_edit_tool(cwd.clone()),
        create_write_tool(cwd.clone()),
        create_grep_tool(cwd.clone()),
        create_find_tool(cwd.clone()),
        create_ls_tool(cwd),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pi-tools-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("tempdir");
        dir
    }
    async fn run(tool: &ToolDefinition, args: Value) -> Result<String, String> {
        let result = (tool.execute)("test".into(), args, None, None).await?;
        let pi_ai::Content::Text(content) = &result.content[0] else {
            panic!("expected text")
        };
        Ok(content.text.to_string())
    }
    #[test]
    fn schemas_equal_pi_fixtures() {
        let expected = json!({
            "read": {"type":"object","properties":{"path":{"type":"string","description":"Path to the file to read (relative or absolute)"},"offset":{"type":"number","description":"Line number to start reading from (1-indexed)"},"limit":{"type":"number","description":"Maximum number of lines to read"}},"required":["path"]},
            "write": {"type":"object","properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}},"required":["path","content"]},
            "bash": {"type":"object","properties":{"command":{"type":"string","description":"Bash command to execute"},"timeout":{"type":"number","description":"Timeout in seconds (optional, no default timeout)"}},"required":["command"]},
            "grep": {"type":"object","properties":{"pattern":{"type":"string","description":"Search pattern (regex or literal string)"},"path":{"type":"string","description":"Directory or file to search (default: current directory)"},"glob":{"type":"string","description":"Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'"},"ignoreCase":{"type":"boolean","description":"Case-insensitive search (default: false)"},"literal":{"type":"boolean","description":"Treat pattern as literal string instead of regex (default: false)"},"context":{"type":"number","description":"Number of lines to show before and after each match (default: 0)"},"limit":{"type":"number","description":"Maximum number of matches to return (default: 100)"}},"required":["pattern"]},
            "find": {"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},"path":{"type":"string","description":"Directory to search in (default: current directory)"},"limit":{"type":"number","description":"Maximum number of results (default: 1000)"}},"required":["pattern"]},
            "ls": {"type":"object","properties":{"path":{"type":"string","description":"Directory to list (default: current directory)"},"limit":{"type":"number","description":"Maximum number of entries to return (default: 500)"}}}
        });
        assert_eq!(read_schema(), expected["read"]);
        assert_eq!(write_schema(), expected["write"]);
        assert_eq!(bash_schema(), expected["bash"]);
        assert_eq!(grep_schema(), expected["grep"]);
        assert_eq!(find_schema(), expected["find"]);
        assert_eq!(ls_schema(), expected["ls"]);
        assert_eq!(
            edit_schema(),
            json!({"type":"object","properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"edits":{"type":"array","items":{"type":"object","properties":{"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}},"required":["oldText","newText"]},"description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."}},"required":["path","edits"]})
        );
    }
    #[tokio::test]
    async fn tools_execute_on_tempdir() {
        let dir = tempdir();
        fs::write(dir.join("input.txt"), "alpha\nbeta\ngamma\n").expect("fixture");
        fs::create_dir(dir.join("nested")).expect("nested");
        fs::write(dir.join("nested/item.rs"), "fn main() {}\n").expect("fixture");
        assert_eq!(
            run(
                &create_read_tool(&dir),
                json!({"path":"input.txt","offset":2,"limit":1})
            )
            .await
            .expect("read"),
            "beta"
        );
        fs::write(dir.join("picture.png"), [137, 80, 78, 71]).expect("image fixture");
        let image = (create_read_tool(&dir).execute)(
            "test".into(),
            json!({"path":"picture.png"}),
            None,
            None,
        )
        .await
        .expect("image read");
        assert!(matches!(
            image.content.get(1),
            Some(pi_ai::Content::Image(_))
        ));
        assert!(
            run(
                &create_write_tool(&dir),
                json!({"path":"made/out.txt","content":"created"})
            )
            .await
            .expect("write")
            .contains("Successfully wrote 7 bytes")
        );
        assert!(
            run(
                &create_edit_tool(&dir),
                json!({"path":"input.txt","edits":[{"oldText":"beta","newText":"delta"}]})
            )
            .await
            .expect("edit")
            .contains("Successfully replaced 1")
        );
        assert!(
            run(&create_bash_tool(&dir), json!({"command":"printf shell"}))
                .await
                .expect("bash")
                .contains("shell")
        );
        assert!(
            run(&create_grep_tool(&dir), json!({"pattern":"delta"}))
                .await
                .expect("grep")
                .contains("input.txt:2:delta")
        );
        assert!(
            run(&create_find_tool(&dir), json!({"pattern":"**/*.rs"}))
                .await
                .expect("find")
                .contains("nested/item.rs")
        );
        assert!(
            run(&create_ls_tool(&dir), json!({}))
                .await
                .expect("ls")
                .contains("nested/")
        );
        fs::remove_dir_all(dir).expect("cleanup");
    }
}
