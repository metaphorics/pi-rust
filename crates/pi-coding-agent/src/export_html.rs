//! Standalone, self-contained HTML session export.
//!
//! The document assets are vendored from pi 0.80.7. Session data is JSON encoded
//! and then base64 encoded before insertion, so no session-controlled bytes are
//! interpreted while the HTML document is parsed.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;

use crate::config::{APP_NAME, get_agent_dir};
use crate::modes::rpc::ExportHtmlFn;
use crate::session::{AgentSession, ToolInfo};
use crate::session_types::{SessionEntry, SessionHeader};

const TEMPLATE_HTML: &str = include_str!("../assets/export-html/template.html");
const TEMPLATE_CSS: &str = include_str!("../assets/export-html/template.css");
const TEMPLATE_JS: &str = include_str!("../assets/export-html/template.js");
const MARKED_JS: &str = include_str!("../assets/export-html/vendor/marked.min.js");
const HIGHLIGHT_JS: &str = include_str!("../assets/export-html/vendor/highlight.min.js");
const DARK_THEME: &str = include_str!("../assets/export-html/dark.json");
const LIGHT_THEME: &str = include_str!("../assets/export-html/light.json");

/// Options shared by live-session and file-based export.
#[derive(Clone, Default)]
pub struct ExportHtmlOptions {
    pub output_path: Option<PathBuf>,
    pub theme_name: Option<String>,
    /// Trusted HTML renderer supplied by the extension host. Session data is
    /// never passed through as HTML unless an installed renderer explicitly
    /// returns markup, matching pi's custom-tool contract.
    pub tool_renderer: Option<Arc<dyn ToolHtmlRenderer>>,
}

/// Optional live-session renderer for extension-defined tools.
pub trait ToolHtmlRenderer: Send + Sync {
    fn render_call(&self, tool_call_id: &str, tool_name: &str, args: &Value) -> Option<String>;

    fn render_result(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        result: &[Value],
        details: Option<&Value>,
        is_error: bool,
    ) -> Option<RenderedToolResult>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedToolResult {
    pub collapsed: Option<String>,
    pub expanded: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionData {
    header: Option<SessionHeader>,
    entries: Vec<SessionEntry>,
    leaf_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ExportTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rendered_tools: Option<BTreeMap<String, RenderedToolHtml>>,
}

#[derive(Serialize)]
struct ExportTool {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenderedToolHtml {
    #[serde(skip_serializing_if = "Option::is_none")]
    call_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_html_collapsed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_html_expanded: Option<String>,
}

#[derive(serde::Deserialize)]
struct ThemeFile {
    #[serde(default)]
    vars: BTreeMap<String, Value>,
    colors: BTreeMap<String, Value>,
    #[serde(default)]
    export: ThemeExport,
}

#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThemeExport {
    page_bg: Option<Value>,
    card_bg: Option<Value>,
    info_bg: Option<Value>,
}

/// Export the live session used by RPC and interactive hosts.
pub fn export_session_to_html(
    session: &AgentSession,
    options: ExportHtmlOptions,
) -> Result<PathBuf, String> {
    let (session_file, header, entries, leaf_id) = session.with_session_manager(|manager| {
        (
            manager.get_session_file().map(Path::to_path_buf),
            manager.get_header().cloned(),
            manager.get_entries(),
            manager.get_leaf_id().map(str::to_owned),
        )
    });

    let session_file =
        session_file.ok_or_else(|| "Cannot export in-memory session to HTML".to_string())?;
    if !session_file.exists() {
        return Err("Nothing to export yet - start a conversation first".to_string());
    }

    let active: HashSet<String> = session.get_active_tool_names().into_iter().collect();
    let tools = session
        .get_all_tools()
        .into_iter()
        .filter(|tool| active.contains(&tool.name))
        .map(export_tool)
        .collect();
    let data = SessionData {
        header,
        rendered_tools: options
            .tool_renderer
            .as_deref()
            .and_then(|renderer| pre_render_custom_tools(&entries, renderer)),
        entries,
        leaf_id,
        system_prompt: Some(session.system_prompt()),
        tools: Some(tools),
    };
    let html = generate_html(&data, options.theme_name.as_deref())?;
    let output_path = output_path(options.output_path, &session_file);
    fs::write(&output_path, html).map_err(|error| error.to_string())?;
    Ok(output_path)
}

/// Export a pi-written JSONL session without constructing an agent runtime.
pub fn export_session_file_to_html(
    input_path: impl AsRef<Path>,
    options: ExportHtmlOptions,
) -> Result<PathBuf, String> {
    let input_path = resolve_input_path(input_path.as_ref());
    if !input_path.exists() {
        return Err(format!("File not found: {}", input_path.display()));
    }
    let manager =
        crate::SessionManager::open(&input_path, None, None).map_err(|error| error.to_string())?;
    let data = SessionData {
        header: manager.get_header().cloned(),
        entries: manager.get_entries(),
        leaf_id: manager.get_leaf_id().map(str::to_owned),
        system_prompt: None,
        tools: None,
        rendered_tools: None,
    };
    let html = generate_html(&data, options.theme_name.as_deref())?;
    let output_path = output_path(options.output_path, &input_path);
    fs::write(&output_path, html).map_err(|error| error.to_string())?;
    Ok(output_path)
}

/// Concrete handler for the existing `RpcModeOptions::export_html` seam.
///
/// The RPC mode remains agnostic to export mechanics; hosts bind this handler.
pub fn rpc_export_html_handler(theme_name: Option<String>) -> ExportHtmlFn {
    Arc::new(move |session, output_path| {
        let theme_name = theme_name.clone();
        Box::pin(async move {
            export_session_to_html(
                &session,
                ExportHtmlOptions {
                    output_path: output_path.map(PathBuf::from),
                    theme_name,
                    tool_renderer: None,
                },
            )
            .map(|path| path.to_string_lossy().into_owned())
        })
    })
}

fn export_tool(tool: ToolInfo) -> ExportTool {
    ExportTool {
        name: tool.name,
        description: tool.description,
        parameters: tool.parameters,
    }
}

fn pre_render_custom_tools(
    entries: &[SessionEntry],
    renderer: &dyn ToolHtmlRenderer,
) -> Option<BTreeMap<String, RenderedToolHtml>> {
    const TEMPLATE_RENDERED: [&str; 5] = ["bash", "read", "write", "edit", "ls"];
    let mut rendered = BTreeMap::<String, RenderedToolHtml>::new();

    for entry in entries {
        let Ok(Value::Object(entry)) = serde_json::to_value(entry) else {
            continue;
        };
        let Some(Value::Object(message)) = entry.get("message") else {
            continue;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(content) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in content {
                    if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                        continue;
                    }
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    if TEMPLATE_RENDERED.contains(&name) {
                        continue;
                    }
                    let args = block.get("arguments").unwrap_or(&Value::Null);
                    if let Some(call_html) = renderer.render_call(id, name, args) {
                        rendered.insert(
                            id.to_string(),
                            RenderedToolHtml {
                                call_html: Some(call_html),
                                result_html_collapsed: None,
                                result_html_expanded: None,
                            },
                        );
                    }
                }
            }
            Some("toolResult") => {
                let Some(id) = message.get("toolCallId").and_then(Value::as_str) else {
                    continue;
                };
                let name = message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !rendered.contains_key(id) && TEMPLATE_RENDERED.contains(&name) {
                    continue;
                }
                let content = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if let Some(result) = renderer.render_result(
                    id,
                    name,
                    content,
                    message.get("details"),
                    message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ) {
                    let entry = rendered.entry(id.to_string()).or_insert(RenderedToolHtml {
                        call_html: None,
                        result_html_collapsed: None,
                        result_html_expanded: None,
                    });
                    entry.result_html_collapsed = result.collapsed;
                    entry.result_html_expanded = result.expanded;
                }
            }
            _ => {}
        }
    }

    (!rendered.is_empty()).then_some(rendered)
}

fn generate_html(data: &SessionData, theme_name: Option<&str>) -> Result<String, String> {
    let theme = load_theme(theme_name.unwrap_or("dark"))?;
    let colors = resolve_colors(&theme, theme_name == Some("light"));
    let base = colors
        .get("userMessageBg")
        .map(String::as_str)
        .unwrap_or("#343541");
    let derived = derive_export_colors(base);
    let page_bg = resolve_optional(theme.export.page_bg.as_ref(), &theme.vars)
        .unwrap_or_else(|| derived.0.clone());
    let card_bg = resolve_optional(theme.export.card_bg.as_ref(), &theme.vars)
        .unwrap_or_else(|| derived.1.clone());
    let info_bg = resolve_optional(theme.export.info_bg.as_ref(), &theme.vars).unwrap_or(derived.2);

    let mut theme_vars = String::new();
    for (index, (key, value)) in colors.iter().enumerate() {
        if index > 0 {
            theme_vars.push_str("\n      ");
        }
        theme_vars.push_str("--");
        theme_vars.push_str(key);
        theme_vars.push_str(": ");
        theme_vars.push_str(value);
        theme_vars.push(';');
    }
    for (name, value) in [
        ("exportPageBg", &page_bg),
        ("exportCardBg", &card_bg),
        ("exportInfoBg", &info_bg),
    ] {
        theme_vars.push_str("\n      --");
        theme_vars.push_str(name);
        theme_vars.push_str(": ");
        theme_vars.push_str(value);
        theme_vars.push(';');
    }

    let css = TEMPLATE_CSS
        .replacen("{{THEME_VARS}}", &theme_vars, 1)
        .replacen("{{BODY_BG}}", &page_bg, 1)
        .replacen("{{CONTAINER_BG}}", &card_bg, 1)
        .replacen("{{INFO_BG}}", &info_bg, 1);
    let json = serde_json::to_vec(data).map_err(|error| error.to_string())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(json);
    Ok(TEMPLATE_HTML
        .replacen("{{CSS}}", &css, 1)
        .replacen("{{JS}}", TEMPLATE_JS, 1)
        .replacen("{{SESSION_DATA}}", &encoded, 1)
        .replacen("{{MARKED_JS}}", MARKED_JS, 1)
        .replacen("{{HIGHLIGHT_JS}}", HIGHLIGHT_JS, 1))
}

fn load_theme(name: &str) -> Result<ThemeFile, String> {
    let bundled = match name {
        "dark" => Some(DARK_THEME),
        "light" => Some(LIGHT_THEME),
        _ => None,
    };
    let source = match bundled {
        Some(source) => source.to_owned(),
        None => {
            let path = get_agent_dir().join("themes").join(format!("{name}.json"));
            if !path.exists() {
                return Err(format!("Theme not found: {name}"));
            }
            fs::read_to_string(path).map_err(|error| error.to_string())?
        }
    };
    serde_json::from_str(&source).map_err(|error| error.to_string())
}

fn resolve_colors(theme: &ThemeFile, light: bool) -> BTreeMap<String, String> {
    theme
        .colors
        .iter()
        .filter_map(|(key, value)| {
            resolve_value(value, &theme.vars, &mut HashSet::new()).map(|mut value| {
                if value.is_empty() {
                    value = if light { "#000000" } else { "#e5e5e7" }.to_string();
                }
                (key.clone(), value)
            })
        })
        .collect()
}

fn resolve_optional(value: Option<&Value>, vars: &BTreeMap<String, Value>) -> Option<String> {
    let resolved = resolve_value(value?, vars, &mut HashSet::new())?;
    (!resolved.is_empty()).then_some(resolved)
}

fn resolve_value(
    value: &Value,
    vars: &BTreeMap<String, Value>,
    seen: &mut HashSet<String>,
) -> Option<String> {
    match value {
        Value::String(text) => {
            if let Some(next) = vars.get(text) {
                if !seen.insert(text.clone()) {
                    return None;
                }
                resolve_value(next, vars, seen)
            } else {
                Some(text.clone())
            }
        }
        Value::Number(number) => number.as_u64().map(ansi256_to_hex),
        _ => None,
    }
}

fn ansi256_to_hex(index: u64) -> String {
    const BASIC: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    if index < 16 {
        return BASIC[index as usize].to_string();
    }
    if index < 232 {
        let cube = index - 16;
        let component = |value: u64| if value == 0 { 0 } else { 55 + value * 40 };
        return format!(
            "#{:02x}{:02x}{:02x}",
            component(cube / 36),
            component((cube % 36) / 6),
            component(cube % 6)
        );
    }
    let gray = 8 + (index.min(255) - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

fn derive_export_colors(base: &str) -> (String, String, String) {
    let Some((r, g, b)) = parse_color(base) else {
        return (
            "rgb(24, 24, 30)".to_string(),
            "rgb(30, 30, 36)".to_string(),
            "rgb(60, 55, 40)".to_string(),
        );
    };
    let linear = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let light = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b) > 0.5;
    let adjust = |factor: f64| {
        format!(
            "rgb({}, {}, {})",
            (f64::from(r) * factor).round().clamp(0.0, 255.0) as u8,
            (f64::from(g) * factor).round().clamp(0.0, 255.0) as u8,
            (f64::from(b) * factor).round().clamp(0.0, 255.0) as u8
        )
    };
    if light {
        (
            adjust(0.96),
            base.to_string(),
            format!(
                "rgb({}, {}, {})",
                r.saturating_add(10),
                g.saturating_add(5),
                b.saturating_sub(20)
            ),
        )
    } else {
        (
            adjust(0.7),
            adjust(0.85),
            format!(
                "rgb({}, {}, {})",
                r.saturating_add(20),
                g.saturating_add(15),
                b
            ),
        )
    }
}

fn parse_color(value: &str) -> Option<(u8, u8, u8)> {
    if value.len() == 7 && value.starts_with('#') {
        return Some((
            u8::from_str_radix(&value[1..3], 16).ok()?,
            u8::from_str_radix(&value[3..5], 16).ok()?,
            u8::from_str_radix(&value[5..7], 16).ok()?,
        ));
    }
    let inner = value.strip_prefix("rgb(")?.strip_suffix(')')?;
    let mut parts = inner.split(',').map(str::trim);
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn output_path(explicit: Option<PathBuf>, session_file: &Path) -> PathBuf {
    explicit
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(
            || {
                let filename = session_file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("session");
                let basename = filename.strip_suffix(".jsonl").unwrap_or(filename);
                PathBuf::from(format!("{APP_NAME}-session-{basename}.html"))
            },
            |path| normalize_path(&path),
        )
}

fn normalize_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Ok(url) = url::Url::parse(&text)
        && url.scheme() == "file"
        && let Ok(path) = url.to_file_path()
    {
        return path;
    }
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn resolve_input_path(path: &Path) -> PathBuf {
    let normalized = normalize_path(path);
    let absolute = if normalized.is_absolute() {
        normalized
    } else {
        std::env::current_dir().unwrap_or_default().join(normalized)
    };
    lexical_normalize(&absolute)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Renderer;

    impl ToolHtmlRenderer for Renderer {
        fn render_call(&self, id: &str, name: &str, args: &Value) -> Option<String> {
            assert_eq!((id, name), ("call-1", "custom"));
            assert_eq!(args["value"], 7);
            Some("<b>call</b>".to_string())
        }

        fn render_result(
            &self,
            id: &str,
            name: &str,
            result: &[Value],
            details: Option<&Value>,
            is_error: bool,
        ) -> Option<RenderedToolResult> {
            assert_eq!((id, name), ("call-1", "custom"));
            assert_eq!(result[0]["text"], "done");
            assert_eq!(
                details.and_then(|value| value.get("count")),
                Some(&Value::from(1))
            );
            assert!(!is_error);
            Some(RenderedToolResult {
                collapsed: Some("<i>collapsed</i>".to_string()),
                expanded: Some("<i>expanded</i>".to_string()),
            })
        }
    }

    #[test]
    fn custom_tool_renderer_matches_oracle_rendered_tools_shape() {
        let entries: Vec<SessionEntry> = vec![
            serde_json::from_value(serde_json::json!({
                "type": "message",
                "id": "assistant",
                "parentId": null,
                "timestamp": "2026-01-01T00:00:00Z",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "toolCall",
                        "id": "call-1",
                        "name": "custom",
                        "arguments": {"value": 7}
                    }]
                }
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "type": "message",
                "id": "result",
                "parentId": "assistant",
                "timestamp": "2026-01-01T00:00:01Z",
                "message": {
                    "role": "toolResult",
                    "toolCallId": "call-1",
                    "toolName": "custom",
                    "content": [{"type": "text", "text": "done"}],
                    "details": {"count": 1},
                    "isError": false
                }
            }))
            .unwrap(),
        ];

        let rendered = pre_render_custom_tools(&entries, &Renderer).expect("rendered tool");
        assert_eq!(
            serde_json::to_value(rendered).unwrap(),
            serde_json::json!({
                "call-1": {
                    "callHtml": "<b>call</b>",
                    "resultHtmlCollapsed": "<i>collapsed</i>",
                    "resultHtmlExpanded": "<i>expanded</i>"
                }
            })
        );
    }
}
