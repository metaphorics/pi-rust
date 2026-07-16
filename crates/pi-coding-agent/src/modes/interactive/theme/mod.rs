//! Interactive theme engine.
//!
//! Port of `modes/interactive/theme/theme.ts`: theme JSON schema validation,
//! color resolution (hex / 256-index / var refs), the [`Theme`] class, the
//! global theme instance, terminal background detection, and the pi-tui
//! theme adapters (`getMarkdownTheme` etc.).
//!
//! Builtin `dark.json` / `light.json` are embedded verbatim (the npm package
//! ships them as files; the Rust binary carries them as assets).

pub mod syntax;
pub mod watcher;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use pi_tui::components::markdown::MarkdownTheme;
use pi_tui::components::select_list::SelectListTheme;
use pi_tui::components::settings_list::SettingsListTheme;
use pi_tui::terminal_image::get_capabilities;
use serde_json::Value;

use crate::config::get_custom_themes_dir;

pub const DARK_THEME_JSON: &str = include_str!("dark.json");
pub const LIGHT_THEME_JSON: &str = include_str!("light.json");
pub const THEME_SCHEMA_JSON: &str = include_str!("theme-schema.json");

// ============================================================================
// Types & Schema
// ============================================================================

/// `"truecolor" | "256color"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Truecolor,
    Color256,
}

/// Foreground theme color keys (oracle `ThemeColor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeColor {
    Accent,
    Border,
    BorderAccent,
    BorderMuted,
    Success,
    Error,
    Warning,
    Muted,
    Dim,
    Text,
    ThinkingText,
    UserMessageText,
    CustomMessageText,
    CustomMessageLabel,
    ToolTitle,
    ToolOutput,
    MdHeading,
    MdLink,
    MdLinkUrl,
    MdCode,
    MdCodeBlock,
    MdCodeBlockBorder,
    MdQuote,
    MdQuoteBorder,
    MdHr,
    MdListBullet,
    ToolDiffAdded,
    ToolDiffRemoved,
    ToolDiffContext,
    SyntaxComment,
    SyntaxKeyword,
    SyntaxFunction,
    SyntaxVariable,
    SyntaxString,
    SyntaxNumber,
    SyntaxType,
    SyntaxOperator,
    SyntaxPunctuation,
    ThinkingOff,
    ThinkingMinimal,
    ThinkingLow,
    ThinkingMedium,
    ThinkingHigh,
    ThinkingXhigh,
    ThinkingMax,
    BashMode,
}

impl ThemeColor {
    pub const ALL: [ThemeColor; 46] = [
        ThemeColor::Accent,
        ThemeColor::Border,
        ThemeColor::BorderAccent,
        ThemeColor::BorderMuted,
        ThemeColor::Success,
        ThemeColor::Error,
        ThemeColor::Warning,
        ThemeColor::Muted,
        ThemeColor::Dim,
        ThemeColor::Text,
        ThemeColor::ThinkingText,
        ThemeColor::UserMessageText,
        ThemeColor::CustomMessageText,
        ThemeColor::CustomMessageLabel,
        ThemeColor::ToolTitle,
        ThemeColor::ToolOutput,
        ThemeColor::MdHeading,
        ThemeColor::MdLink,
        ThemeColor::MdLinkUrl,
        ThemeColor::MdCode,
        ThemeColor::MdCodeBlock,
        ThemeColor::MdCodeBlockBorder,
        ThemeColor::MdQuote,
        ThemeColor::MdQuoteBorder,
        ThemeColor::MdHr,
        ThemeColor::MdListBullet,
        ThemeColor::ToolDiffAdded,
        ThemeColor::ToolDiffRemoved,
        ThemeColor::ToolDiffContext,
        ThemeColor::SyntaxComment,
        ThemeColor::SyntaxKeyword,
        ThemeColor::SyntaxFunction,
        ThemeColor::SyntaxVariable,
        ThemeColor::SyntaxString,
        ThemeColor::SyntaxNumber,
        ThemeColor::SyntaxType,
        ThemeColor::SyntaxOperator,
        ThemeColor::SyntaxPunctuation,
        ThemeColor::ThinkingOff,
        ThemeColor::ThinkingMinimal,
        ThemeColor::ThinkingLow,
        ThemeColor::ThinkingMedium,
        ThemeColor::ThinkingHigh,
        ThemeColor::ThinkingXhigh,
        ThemeColor::ThinkingMax,
        ThemeColor::BashMode,
    ];

    /// JSON key (camelCase, matching theme-schema.json).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ThemeColor::Accent => "accent",
            ThemeColor::Border => "border",
            ThemeColor::BorderAccent => "borderAccent",
            ThemeColor::BorderMuted => "borderMuted",
            ThemeColor::Success => "success",
            ThemeColor::Error => "error",
            ThemeColor::Warning => "warning",
            ThemeColor::Muted => "muted",
            ThemeColor::Dim => "dim",
            ThemeColor::Text => "text",
            ThemeColor::ThinkingText => "thinkingText",
            ThemeColor::UserMessageText => "userMessageText",
            ThemeColor::CustomMessageText => "customMessageText",
            ThemeColor::CustomMessageLabel => "customMessageLabel",
            ThemeColor::ToolTitle => "toolTitle",
            ThemeColor::ToolOutput => "toolOutput",
            ThemeColor::MdHeading => "mdHeading",
            ThemeColor::MdLink => "mdLink",
            ThemeColor::MdLinkUrl => "mdLinkUrl",
            ThemeColor::MdCode => "mdCode",
            ThemeColor::MdCodeBlock => "mdCodeBlock",
            ThemeColor::MdCodeBlockBorder => "mdCodeBlockBorder",
            ThemeColor::MdQuote => "mdQuote",
            ThemeColor::MdQuoteBorder => "mdQuoteBorder",
            ThemeColor::MdHr => "mdHr",
            ThemeColor::MdListBullet => "mdListBullet",
            ThemeColor::ToolDiffAdded => "toolDiffAdded",
            ThemeColor::ToolDiffRemoved => "toolDiffRemoved",
            ThemeColor::ToolDiffContext => "toolDiffContext",
            ThemeColor::SyntaxComment => "syntaxComment",
            ThemeColor::SyntaxKeyword => "syntaxKeyword",
            ThemeColor::SyntaxFunction => "syntaxFunction",
            ThemeColor::SyntaxVariable => "syntaxVariable",
            ThemeColor::SyntaxString => "syntaxString",
            ThemeColor::SyntaxNumber => "syntaxNumber",
            ThemeColor::SyntaxType => "syntaxType",
            ThemeColor::SyntaxOperator => "syntaxOperator",
            ThemeColor::SyntaxPunctuation => "syntaxPunctuation",
            ThemeColor::ThinkingOff => "thinkingOff",
            ThemeColor::ThinkingMinimal => "thinkingMinimal",
            ThemeColor::ThinkingLow => "thinkingLow",
            ThemeColor::ThinkingMedium => "thinkingMedium",
            ThemeColor::ThinkingHigh => "thinkingHigh",
            ThemeColor::ThinkingXhigh => "thinkingXhigh",
            ThemeColor::ThinkingMax => "thinkingMax",
            ThemeColor::BashMode => "bashMode",
        }
    }
}

/// Background theme color keys (oracle `ThemeBg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeBg {
    SelectedBg,
    UserMessageBg,
    CustomMessageBg,
    ToolPendingBg,
    ToolSuccessBg,
    ToolErrorBg,
}

impl ThemeBg {
    pub const ALL: [ThemeBg; 6] = [
        ThemeBg::SelectedBg,
        ThemeBg::UserMessageBg,
        ThemeBg::CustomMessageBg,
        ThemeBg::ToolPendingBg,
        ThemeBg::ToolSuccessBg,
        ThemeBg::ToolErrorBg,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ThemeBg::SelectedBg => "selectedBg",
            ThemeBg::UserMessageBg => "userMessageBg",
            ThemeBg::CustomMessageBg => "customMessageBg",
            ThemeBg::ToolPendingBg => "toolPendingBg",
            ThemeBg::ToolSuccessBg => "toolSuccessBg",
            ThemeBg::ToolErrorBg => "toolErrorBg",
        }
    }
}

/// Every color key required by the schema, in schema declaration order.
/// (`thinkingMax` is optional and therefore not listed.)
pub const REQUIRED_COLOR_KEYS: [&str; 51] = [
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
];

/// A raw color value: hex string, empty string, var ref, or 0-255 index.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorValue {
    Str(String),
    Index(u8),
}

impl ColorValue {
    fn from_json(value: &Value) -> Option<ColorValue> {
        match value {
            Value::String(s) => Some(ColorValue::Str(s.clone())),
            Value::Number(n) => {
                let i = n.as_i64()?;
                if n.is_i64() && (0..=255).contains(&i) {
                    Some(ColorValue::Index(i as u8))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Parsed theme JSON (oracle `ThemeJson`).
#[derive(Debug, Clone)]
pub struct ThemeJson {
    pub name: String,
    pub vars: HashMap<String, ColorValue>,
    pub colors: HashMap<String, ColorValue>,
    /// Optional HTML-export colors (`export.pageBg` etc.).
    pub export: HashMap<String, ColorValue>,
}

// ============================================================================
// Color Utilities
// ============================================================================

/// RGB triple from an OSC 11 response or hex color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Oracle `hexToRgb`. Errors on malformed input.
pub fn hex_to_rgb(hex: &str) -> Result<RgbColor, String> {
    let cleaned = hex.trim_start_matches('#');
    if cleaned.len() != 6 {
        return Err(format!("Invalid hex color: {hex}"));
    }
    let parse =
        |s: &str| u8::from_str_radix(s, 16).map_err(|_| format!("Invalid hex color: {hex}"));
    Ok(RgbColor {
        r: parse(&cleaned[0..2])?,
        g: parse(&cleaned[2..4])?,
        b: parse(&cleaned[4..6])?,
    })
}

/// The 6x6x6 color cube channel values (indices 0-5).
const CUBE_VALUES: [i32; 6] = [0, 95, 135, 175, 215, 255];

fn find_closest_cube_index(value: i32) -> usize {
    let mut min_dist = i32::MAX;
    let mut min_idx = 0;
    for (i, &cube) in CUBE_VALUES.iter().enumerate() {
        let dist = (value - cube).abs();
        if dist < min_dist {
            min_dist = dist;
            min_idx = i;
        }
    }
    min_idx
}

fn find_closest_gray_index(gray: i32) -> usize {
    // Grayscale ramp values (indices 232-255, 24 grays from 8 to 238)
    let mut min_dist = i32::MAX;
    let mut min_idx = 0;
    for i in 0..24 {
        let value = 8 + (i as i32) * 10;
        let dist = (gray - value).abs();
        if dist < min_dist {
            min_dist = dist;
            min_idx = i;
        }
    }
    min_idx
}

fn color_distance(r1: i32, g1: i32, b1: i32, r2: i32, g2: i32, b2: i32) -> f64 {
    // Weighted Euclidean distance (human eye is more sensitive to green)
    let dr = f64::from(r1 - r2);
    let dg = f64::from(g1 - g2);
    let db = f64::from(b1 - b2);
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// Oracle `rgbTo256`: closest 256-palette index for an RGB color.
#[must_use]
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let r_idx = find_closest_cube_index(r);
    let g_idx = find_closest_cube_index(g);
    let b_idx = find_closest_cube_index(b);
    let cube_index = 16 + 36 * r_idx + 6 * g_idx + b_idx;
    let cube_dist = color_distance(
        r,
        g,
        b,
        CUBE_VALUES[r_idx],
        CUBE_VALUES[g_idx],
        CUBE_VALUES[b_idx],
    );

    let gray = (0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)).round() as i32;
    let gray_idx = find_closest_gray_index(gray);
    let gray_value = 8 + (gray_idx as i32) * 10;
    let gray_index = 232 + gray_idx;
    let gray_dist = color_distance(r, g, b, gray_value, gray_value, gray_value);

    let spread = r.max(g).max(b) - r.min(g).min(b);
    if spread < 10 && gray_dist < cube_dist {
        return gray_index as u8;
    }
    cube_index as u8
}

fn hex_to_256(hex: &str) -> Result<u8, String> {
    let rgb = hex_to_rgb(hex)?;
    Ok(rgb_to_256(rgb.r, rgb.g, rgb.b))
}

/// A resolved color value: `""`, `"#rrggbb"`, or palette index.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedColor {
    Default,
    Hex(String),
    Index(u8),
}

fn fg_ansi(color: &ResolvedColor, mode: ColorMode) -> Result<String, String> {
    match color {
        ResolvedColor::Default => Ok("\x1b[39m".to_owned()),
        ResolvedColor::Index(i) => Ok(format!("\x1b[38;5;{i}m")),
        ResolvedColor::Hex(hex) => match mode {
            ColorMode::Truecolor => {
                let RgbColor { r, g, b } = hex_to_rgb(hex)?;
                Ok(format!("\x1b[38;2;{r};{g};{b}m"))
            }
            ColorMode::Color256 => {
                let index = hex_to_256(hex)?;
                Ok(format!("\x1b[38;5;{index}m"))
            }
        },
    }
}

fn bg_ansi(color: &ResolvedColor, mode: ColorMode) -> Result<String, String> {
    match color {
        ResolvedColor::Default => Ok("\x1b[49m".to_owned()),
        ResolvedColor::Index(i) => Ok(format!("\x1b[48;5;{i}m")),
        ResolvedColor::Hex(hex) => match mode {
            ColorMode::Truecolor => {
                let RgbColor { r, g, b } = hex_to_rgb(hex)?;
                Ok(format!("\x1b[48;2;{r};{g};{b}m"))
            }
            ColorMode::Color256 => {
                let index = hex_to_256(hex)?;
                Ok(format!("\x1b[48;5;{index}m"))
            }
        },
    }
}

/// Oracle `resolveVarRefs`.
fn resolve_var_refs(
    value: &ColorValue,
    vars: &HashMap<String, ColorValue>,
    visited: &mut Vec<String>,
) -> Result<ResolvedColor, String> {
    match value {
        ColorValue::Index(i) => Ok(ResolvedColor::Index(*i)),
        ColorValue::Str(s) if s.is_empty() => Ok(ResolvedColor::Default),
        ColorValue::Str(s) if s.starts_with('#') => Ok(ResolvedColor::Hex(s.clone())),
        ColorValue::Str(name) => {
            if visited.iter().any(|v| v == name) {
                return Err(format!("Circular variable reference detected: {name}"));
            }
            let Some(next) = vars.get(name) else {
                return Err(format!("Variable reference not found: {name}"));
            };
            visited.push(name.clone());
            resolve_var_refs(next, vars, visited)
        }
    }
}

// ============================================================================
// Theme
// ============================================================================

/// Resolved, render-ready theme (oracle `Theme` class).
#[derive(Debug)]
pub struct Theme {
    pub name: Option<String>,
    pub source_path: Option<PathBuf>,
    fg_colors: HashMap<ThemeColor, String>,
    bg_colors: HashMap<ThemeBg, String>,
    mode: ColorMode,
}

impl Theme {
    /// Build from validated theme JSON (oracle `createTheme`).
    pub fn from_json(
        json: &ThemeJson,
        mode: Option<ColorMode>,
        source_path: Option<PathBuf>,
    ) -> Result<Theme, String> {
        let mode = mode.unwrap_or_else(|| {
            if get_capabilities().true_color {
                ColorMode::Truecolor
            } else {
                ColorMode::Color256
            }
        });
        let mut fg_colors = HashMap::new();
        let mut bg_colors = HashMap::new();
        for key in ThemeColor::ALL {
            // thinkingMax falls back to thinkingXhigh (withThemeColorFallbacks)
            let raw = json.colors.get(key.as_str()).or_else(|| {
                if key == ThemeColor::ThinkingMax {
                    json.colors.get("thinkingXhigh")
                } else {
                    None
                }
            });
            let Some(raw) = raw else {
                return Err(format!("Unknown theme color: {}", key.as_str()));
            };
            let resolved = resolve_var_refs(raw, &json.vars, &mut Vec::new())?;
            fg_colors.insert(key, fg_ansi(&resolved, mode)?);
        }
        for key in ThemeBg::ALL {
            let Some(raw) = json.colors.get(key.as_str()) else {
                return Err(format!("Unknown theme background color: {}", key.as_str()));
            };
            let resolved = resolve_var_refs(raw, &json.vars, &mut Vec::new())?;
            bg_colors.insert(key, bg_ansi(&resolved, mode)?);
        }
        Ok(Theme {
            name: Some(json.name.clone()),
            source_path,
            fg_colors,
            bg_colors,
            mode,
        })
    }

    /// Foreground-color the text; resets only the foreground (SGR 39).
    #[must_use]
    pub fn fg(&self, color: ThemeColor, text: &str) -> String {
        let ansi = &self.fg_colors[&color];
        format!("{ansi}{text}\x1b[39m")
    }

    /// Background-color the text; resets only the background (SGR 49).
    #[must_use]
    pub fn bg(&self, color: ThemeBg, text: &str) -> String {
        let ansi = &self.bg_colors[&color];
        format!("{ansi}{text}\x1b[49m")
    }

    #[must_use]
    pub fn bold(&self, text: &str) -> String {
        format!("\x1b[1m{text}\x1b[22m")
    }

    #[must_use]
    pub fn italic(&self, text: &str) -> String {
        format!("\x1b[3m{text}\x1b[23m")
    }

    #[must_use]
    pub fn underline(&self, text: &str) -> String {
        format!("\x1b[4m{text}\x1b[24m")
    }

    #[must_use]
    pub fn inverse(&self, text: &str) -> String {
        format!("\x1b[7m{text}\x1b[27m")
    }

    #[must_use]
    pub fn strikethrough(&self, text: &str) -> String {
        format!("\x1b[9m{text}\x1b[29m")
    }

    /// Raw foreground SGR prefix for a color.
    #[must_use]
    pub fn get_fg_ansi(&self, color: ThemeColor) -> &str {
        &self.fg_colors[&color]
    }

    /// Raw background SGR prefix for a color.
    #[must_use]
    pub fn get_bg_ansi(&self, color: ThemeBg) -> &str {
        &self.bg_colors[&color]
    }

    #[must_use]
    pub fn get_color_mode(&self) -> ColorMode {
        self.mode
    }

    /// Border color for a thinking level (oracle `getThinkingBorderColor`).
    #[must_use]
    pub fn thinking_border_color(&self, level: &str) -> ThemeColor {
        match level {
            "minimal" => ThemeColor::ThinkingMinimal,
            "low" => ThemeColor::ThinkingLow,
            "medium" => ThemeColor::ThinkingMedium,
            "high" => ThemeColor::ThinkingHigh,
            "xhigh" => ThemeColor::ThinkingXhigh,
            "max" => ThemeColor::ThinkingMax,
            _ => ThemeColor::ThinkingOff,
        }
    }
}

// ============================================================================
// Theme JSON parsing / validation
// ============================================================================

fn color_value_error(path: &str) -> String {
    format!("  - {path}: Expected a hex color, 256-color index, or variable reference")
}

/// Oracle `parseThemeJson`: validate + collect missing-color diagnostics.
pub fn parse_theme_json(label: &str, json: &Value) -> Result<ThemeJson, String> {
    let mut missing_colors: Vec<String> = Vec::new();
    let mut other_errors: Vec<String> = Vec::new();

    let obj = json.as_object();
    let name = obj
        .and_then(|o| o.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if name.is_none() {
        other_errors.push("  - /name: Expected string".to_owned());
    }

    let mut vars = HashMap::new();
    if let Some(raw_vars) = obj.and_then(|o| o.get("vars")) {
        if let Some(map) = raw_vars.as_object() {
            for (k, v) in map {
                match ColorValue::from_json(v) {
                    Some(cv) => {
                        vars.insert(k.clone(), cv);
                    }
                    None => other_errors.push(color_value_error(&format!("/vars/{k}"))),
                }
            }
        } else {
            other_errors.push("  - /vars: Expected object".to_owned());
        }
    }

    let mut colors = HashMap::new();
    match obj.and_then(|o| o.get("colors")) {
        Some(Value::Object(map)) => {
            for (k, v) in map {
                match ColorValue::from_json(v) {
                    Some(cv) => {
                        colors.insert(k.clone(), cv);
                    }
                    None => other_errors.push(color_value_error(&format!("/colors/{k}"))),
                }
            }
            for required in REQUIRED_COLOR_KEYS {
                if !map.contains_key(required) {
                    missing_colors.push(required.to_owned());
                }
            }
        }
        _ => {
            other_errors.push("  - /colors: Expected object".to_owned());
        }
    }

    let mut export = HashMap::new();
    if let Some(Value::Object(map)) = obj.and_then(|o| o.get("export")) {
        for (k, v) in map {
            if let Some(cv) = ColorValue::from_json(v) {
                export.insert(k.clone(), cv);
            }
        }
    }

    if !missing_colors.is_empty() || !other_errors.is_empty() {
        let mut error_message = format!("Invalid theme \"{label}\":\n");
        if !missing_colors.is_empty() {
            missing_colors.sort();
            missing_colors.dedup();
            error_message.push_str("\nMissing required color tokens:\n");
            error_message.push_str(
                &missing_colors
                    .iter()
                    .map(|c| format!("  - {c}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            error_message
                .push_str("\n\nPlease add these colors to your theme's \"colors\" object.");
            error_message.push_str(
                "\nSee the built-in themes (dark.json, light.json) for reference values.",
            );
        }
        if !other_errors.is_empty() {
            error_message.push_str(&format!("\n\nOther errors:\n{}", other_errors.join("\n")));
        }
        return Err(error_message);
    }

    let name = name.expect("checked above");
    assert_theme_name_is_valid(&name)?;
    Ok(ThemeJson {
        name,
        vars,
        colors,
        export,
    })
}

fn assert_theme_name_is_valid(name: &str) -> Result<(), String> {
    if name.contains('/') {
        return Err(format!(
            "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
        ));
    }
    Ok(())
}

/// Oracle `parseThemeJsonContent`.
pub fn parse_theme_json_content(label: &str, content: &str) -> Result<ThemeJson, String> {
    let json: Value = serde_json::from_str(content)
        .map_err(|error| format!("Failed to parse theme {label}: {error}"))?;
    parse_theme_json(label, &json)
}

// ============================================================================
// Theme loading
// ============================================================================

static BUILTIN_THEMES: LazyLock<HashMap<String, ThemeJson>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "dark".to_owned(),
        parse_theme_json_content("dark", DARK_THEME_JSON).expect("embedded dark theme is valid"),
    );
    map.insert(
        "light".to_owned(),
        parse_theme_json_content("light", LIGHT_THEME_JSON).expect("embedded light theme is valid"),
    );
    map
});

/// Theme name + source path (oracle `ThemeInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    /// `None` for embedded builtin themes (the npm package has them on disk).
    pub path: Option<PathBuf>,
}

/// Oracle `getAvailableThemes`.
#[must_use]
pub fn get_available_themes() -> Vec<String> {
    get_available_themes_with_paths()
        .into_iter()
        .map(|t| t.name)
        .collect()
}

/// Oracle `getAvailableThemesWithPaths`: builtin + custom + registered, deduped
/// by name, sorted.
#[must_use]
pub fn get_available_themes_with_paths() -> Vec<ThemeInfo> {
    let mut result: Vec<ThemeInfo> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |result: &mut Vec<ThemeInfo>, info: ThemeInfo| {
        if seen.insert(info.name.clone()) {
            result.push(info);
        }
    };
    for name in ["dark", "light"] {
        add(
            &mut result,
            ThemeInfo {
                name: name.to_owned(),
                path: None,
            },
        );
    }
    for info in get_custom_theme_infos() {
        add(&mut result, info);
    }
    {
        let registered = registered_themes()
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for (name, theme) in registered.iter() {
            add(
                &mut result,
                ThemeInfo {
                    name: name.clone(),
                    path: theme.source_path.clone(),
                },
            );
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

fn get_custom_theme_infos() -> Vec<ThemeInfo> {
    let dir = get_custom_themes_dir();
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return result;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    for path in files {
        // Invalid themes are ignored here; the resource loader reports them.
        if let Ok(theme) = load_theme_from_path(&path, None)
            && let Some(name) = theme.name
        {
            result.push(ThemeInfo {
                name,
                path: Some(path),
            });
        }
    }
    result
}

fn load_theme_json(name: &str) -> Result<ThemeJson, String> {
    if let Some(json) = BUILTIN_THEMES.get(name) {
        return Ok(json.clone());
    }
    {
        let registered = registered_themes()
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(theme) = registered.get(name) {
            if let Some(source_path) = &theme.source_path {
                let content = std::fs::read_to_string(source_path)
                    .map_err(|e| format!("Failed to read theme {}: {e}", source_path.display()))?;
                return parse_theme_json_content(&source_path.display().to_string(), &content);
            }
            return Err(format!(
                "Theme \"{name}\" does not have a source path for export"
            ));
        }
    }
    let theme_path = get_custom_themes_dir().join(format!("{name}.json"));
    if !theme_path.exists() {
        return Err(format!("Theme not found: {name}"));
    }
    let content = std::fs::read_to_string(&theme_path)
        .map_err(|e| format!("Failed to read theme {name}: {e}"))?;
    parse_theme_json_content(name, &content)
}

/// Oracle `loadThemeFromPath`.
pub fn load_theme_from_path(path: &Path, mode: Option<ColorMode>) -> Result<Theme, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read theme {}: {e}", path.display()))?;
    let json = parse_theme_json_content(&path.display().to_string(), &content)?;
    Theme::from_json(&json, mode, Some(path.to_path_buf()))
}

fn load_theme(name: &str, mode: Option<ColorMode>) -> Result<Arc<Theme>, String> {
    {
        let registered = registered_themes()
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(theme) = registered.get(name) {
            return Ok(Arc::clone(theme));
        }
    }
    let json = load_theme_json(name)?;
    Ok(Arc::new(Theme::from_json(&json, mode, None)?))
}

/// Oracle `getThemeByName`.
#[must_use]
pub fn get_theme_by_name(name: &str) -> Option<Arc<Theme>> {
    load_theme(name, None).ok()
}

// ============================================================================
// Light/dark pair setting resolution + terminal background detection
// ============================================================================

/// `"dark" | "light"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalTheme {
    Dark,
    Light,
}

impl TerminalTheme {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TerminalTheme::Dark => "dark",
            TerminalTheme::Light => "light",
        }
    }
}

/// Oracle `parseAutoThemeSetting`: `"light/dark"` pair.
#[must_use]
pub fn parse_auto_theme_setting(theme_setting: Option<&str>) -> Option<(String, String)> {
    let setting = theme_setting?;
    let slash = setting.find('/')?;
    if setting[slash + 1..].contains('/') {
        return None;
    }
    let light = setting[..slash].trim();
    let dark = setting[slash + 1..].trim();
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light.to_owned(), dark.to_owned()))
}

/// Oracle `resolveThemeSetting`.
#[must_use]
pub fn resolve_theme_setting(
    theme_setting: Option<&str>,
    terminal_theme: TerminalTheme,
) -> Option<String> {
    if let Some((light, dark)) = parse_auto_theme_setting(theme_setting) {
        return Some(match terminal_theme {
            TerminalTheme::Light => light,
            TerminalTheme::Dark => dark,
        });
    }
    match theme_setting {
        Some(s) if s.contains('/') => None,
        Some(s) => Some(s.to_owned()),
        None => None,
    }
}

/// Oracle `TerminalThemeDetection`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalThemeDetection {
    pub theme: TerminalTheme,
    /// `"terminal background" | "COLORFGBG" | "fallback"`.
    pub source: &'static str,
    pub detail: String,
    /// `"high" | "low"`.
    pub confidence: &'static str,
}

fn get_colorfgbg_background_index(colorfgbg: &str) -> Option<u8> {
    for part in colorfgbg.split(';').rev() {
        if let Ok(bg) = part.trim().parse::<i64>()
            && (0..=255).contains(&bg)
        {
            return Some(bg as u8);
        }
    }
    None
}

fn rgb_luminance(rgb: RgbColor) -> f64 {
    let to_linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(rgb.r) + 0.7152 * to_linear(rgb.g) + 0.0722 * to_linear(rgb.b)
}

/// Oracle `getThemeForRgbColor`.
#[must_use]
pub fn get_theme_for_rgb_color(rgb: RgbColor) -> TerminalTheme {
    if rgb_luminance(rgb) >= 0.5 {
        TerminalTheme::Light
    } else {
        TerminalTheme::Dark
    }
}

/// Oracle `ansi256ToHex`.
#[must_use]
pub fn ansi_256_to_hex(index: u8) -> String {
    const BASE16: [&str; 16] = [
        "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
        "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    let index = usize::from(index);
    if index < 16 {
        return BASE16[index].to_owned();
    }
    if index < 232 {
        let i = index - 16;
        let r = CUBE_VALUES[i / 36];
        let g = CUBE_VALUES[(i / 6) % 6];
        let b = CUBE_VALUES[i % 6];
        return format!("#{r:02x}{g:02x}{b:02x}");
    }
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

fn get_ansi_color_luminance(index: u8) -> f64 {
    rgb_luminance(hex_to_rgb(&ansi_256_to_hex(index)).expect("generated hex is valid"))
}

/// Oracle `detectTerminalBackgroundFromEnv` (COLORFGBG heuristic).
#[must_use]
pub fn detect_terminal_background_from_env(env_colorfgbg: Option<&str>) -> TerminalThemeDetection {
    let owned;
    let colorfgbg = match env_colorfgbg {
        Some(v) => v,
        None => {
            owned = std::env::var("COLORFGBG").unwrap_or_default();
            &owned
        }
    };
    if let Some(bg) = get_colorfgbg_background_index(colorfgbg) {
        return TerminalThemeDetection {
            theme: if get_ansi_color_luminance(bg) >= 0.5 {
                TerminalTheme::Light
            } else {
                TerminalTheme::Dark
            },
            source: "COLORFGBG",
            detail: format!("background color index {bg}"),
            confidence: "high",
        };
    }
    TerminalThemeDetection {
        theme: TerminalTheme::Dark,
        source: "fallback",
        detail: "no terminal background hint found".to_owned(),
        confidence: "low",
    }
}

/// Oracle `detectTerminalBackgroundTheme`: OSC 11 first, env fallback.
///
/// `query_background` is the OSC 11 probe (pi-tui `query_background_color`);
/// `None` (unsupported/timeout) falls back to environment detection.
pub fn detect_terminal_background_theme(
    query_background: impl FnOnce() -> Option<RgbColor>,
    env_colorfgbg: Option<&str>,
) -> TerminalThemeDetection {
    if let Some(rgb) = query_background() {
        return TerminalThemeDetection {
            theme: get_theme_for_rgb_color(rgb),
            source: "terminal background",
            detail: format!("OSC 11 background rgb({}, {}, {})", rgb.r, rgb.g, rgb.b),
            confidence: "high",
        };
    }
    detect_terminal_background_from_env(env_colorfgbg)
}

/// Detect the terminal background through pi-tui's OSC 11 probe, then
/// `COLORFGBG` when the terminal does not answer.
#[must_use]
pub fn detect_terminal_background_theme_via(
    terminal: &mut pi_tui::terminal::ProcessTerminal,
    timeout_ms: u64,
) -> TerminalThemeDetection {
    detect_terminal_background_theme(
        || {
            terminal
                .query_background_color(timeout_ms)
                .map(|(r, g, b)| RgbColor { r, g, b })
        },
        std::env::var("COLORFGBG").ok().as_deref(),
    )
}

/// Oracle `getDefaultTheme` ("dark" or "light").
#[must_use]
pub fn get_default_theme() -> String {
    detect_terminal_background_from_env(None)
        .theme
        .as_str()
        .to_owned()
}

// ============================================================================
// Global theme instance
// ============================================================================

type ThemeChangeCallback = Arc<dyn Fn() + Send + Sync>;

struct GlobalThemeState {
    current: Arc<Theme>,
    current_name: Option<String>,
}

static GLOBAL_THEME: LazyLock<RwLock<GlobalThemeState>> = LazyLock::new(|| {
    let json = BUILTIN_THEMES.get("dark").expect("builtin dark exists");
    RwLock::new(GlobalThemeState {
        current: Arc::new(Theme::from_json(json, None, None).expect("builtin dark theme is valid")),
        current_name: None,
    })
});

static REGISTERED_THEMES: LazyLock<RwLock<HashMap<String, Arc<Theme>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

static ON_THEME_CHANGE: LazyLock<RwLock<Option<ThemeChangeCallback>>> =
    LazyLock::new(|| RwLock::new(None));

fn registered_themes() -> &'static RwLock<HashMap<String, Arc<Theme>>> {
    &REGISTERED_THEMES
}

/// Current global theme (oracle `theme` proxy). Cheap Arc clone.
#[must_use]
pub fn theme() -> Arc<Theme> {
    Arc::clone(
        &GLOBAL_THEME
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .current,
    )
}

/// Currently active theme name (None before `init_theme`).
#[must_use]
pub fn current_theme_name() -> Option<String> {
    GLOBAL_THEME
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .current_name
        .clone()
}

fn set_global_theme(theme: Arc<Theme>, name: Option<String>) {
    let mut state = GLOBAL_THEME.write().unwrap_or_else(|e| e.into_inner());
    state.current = theme;
    state.current_name = name;
}

fn fire_theme_change() {
    // Clone under the lock, invoke outside it: a callback that re-registers
    // itself via `on_theme_change` must not deadlock.
    let cb = ON_THEME_CHANGE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(cb) = cb {
        cb();
    }
}

/// Oracle `setRegisteredThemes` (resource-loader supplied themes).
pub fn set_registered_themes(themes: Vec<Theme>) {
    let mut registered = registered_themes()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    registered.clear();
    for theme in themes {
        if let Some(name) = theme.name.clone() {
            registered.insert(name, Arc::new(theme));
        }
    }
}

/// Oracle `initTheme`: load by name (default = terminal background), fall
/// back to dark silently on error.
pub fn init_theme(theme_name: Option<&str>, enable_watcher: bool) {
    let name = theme_name.map_or_else(get_default_theme, str::to_owned);
    match load_theme(&name, None) {
        Ok(theme) => {
            set_global_theme(theme, Some(name));
            if enable_watcher {
                watcher::start_theme_watcher();
            }
        }
        Err(_) => {
            let dark = load_theme("dark", None).expect("builtin dark theme loads");
            set_global_theme(dark, Some("dark".to_owned()));
        }
    }
}

/// Oracle `setTheme`: switch theme; on error fall back to dark and report.
pub fn set_theme(name: &str, enable_watcher: bool) -> Result<(), String> {
    match load_theme(name, None) {
        Ok(theme) => {
            set_global_theme(theme, Some(name.to_owned()));
            if enable_watcher {
                watcher::start_theme_watcher();
            }
            fire_theme_change();
            Ok(())
        }
        Err(error) => {
            let dark = load_theme("dark", None).expect("builtin dark theme loads");
            set_global_theme(dark, Some("dark".to_owned()));
            Err(error)
        }
    }
}

/// Oracle `setThemeInstance` (in-memory theme, no watcher).
pub fn set_theme_instance(theme: Theme) {
    set_global_theme(Arc::new(theme), Some("<in-memory>".to_owned()));
    watcher::stop_theme_watcher();
    fire_theme_change();
}

/// Oracle `onThemeChange`.
pub fn on_theme_change(callback: impl Fn() + Send + Sync + 'static) {
    let mut cb = ON_THEME_CHANGE.write().unwrap_or_else(|e| e.into_inner());
    *cb = Some(Arc::new(callback));
}

/// Watcher reload hook: reload the named custom theme from disk and make it
/// current (keeps registry cache fresh). Used by [`watcher`].
pub(crate) fn reload_watched_theme(name: &str, path: &Path) {
    let Ok(theme) = load_theme_from_path(path, None) else {
        return; // file might be mid-edit; keep last good theme
    };
    let theme = Arc::new(theme);
    {
        let mut registered = registered_themes()
            .write()
            .unwrap_or_else(|e| e.into_inner());
        registered.insert(name.to_owned(), Arc::clone(&theme));
    }
    set_global_theme(theme, Some(name.to_owned()));
    fire_theme_change();
}

// ============================================================================
// pi-tui theme adapters (oracle getMarkdownTheme / getSelectListTheme / ...)
// ============================================================================

/// Oracle `getMarkdownTheme`. Closures resolve the live global theme at call
/// time, so a theme switch only needs component invalidation.
#[must_use]
pub fn get_markdown_theme() -> MarkdownTheme {
    let fg = |color: ThemeColor| {
        Arc::new(move |text: &str| theme().fg(color, text)) as pi_tui::components::markdown::StyleFn
    };
    MarkdownTheme {
        heading: fg(ThemeColor::MdHeading),
        link: fg(ThemeColor::MdLink),
        link_url: fg(ThemeColor::MdLinkUrl),
        code: fg(ThemeColor::MdCode),
        code_block: fg(ThemeColor::MdCodeBlock),
        code_block_border: fg(ThemeColor::MdCodeBlockBorder),
        quote: fg(ThemeColor::MdQuote),
        quote_border: fg(ThemeColor::MdQuoteBorder),
        hr: fg(ThemeColor::MdHr),
        list_bullet: fg(ThemeColor::MdListBullet),
        bold: Arc::new(|text: &str| theme().bold(text)),
        italic: Arc::new(|text: &str| theme().italic(text)),
        underline: Arc::new(|text: &str| theme().underline(text)),
        strikethrough: Arc::new(|text: &str| theme().strikethrough(text)),
        highlight_code: Some(Arc::new(|code: &str, lang: Option<&str>| {
            syntax::highlight_code(code, lang)
        })),
        code_block_indent: "  ".to_owned(),
    }
}

/// Oracle `getSelectListTheme`.
#[must_use]
pub fn get_select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Box::new(|text: &str| theme().fg(ThemeColor::Accent, text)),
        selected_text: Box::new(|text: &str| theme().fg(ThemeColor::Accent, text)),
        description: Box::new(|text: &str| theme().fg(ThemeColor::Muted, text)),
        scroll_info: Box::new(|text: &str| theme().fg(ThemeColor::Muted, text)),
        no_match: Box::new(|text: &str| theme().fg(ThemeColor::Muted, text)),
    }
}

/// Oracle `getSettingsListTheme`.
#[must_use]
pub fn get_settings_list_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Box::new(|text: &str, selected: bool| {
            if selected {
                theme().fg(ThemeColor::Accent, text)
            } else {
                text.to_owned()
            }
        }),
        value: Box::new(|text: &str, selected: bool| {
            if selected {
                theme().fg(ThemeColor::Accent, text)
            } else {
                theme().fg(ThemeColor::Muted, text)
            }
        }),
        description: Box::new(|text: &str| theme().fg(ThemeColor::Dim, text)),
        cursor: theme().fg(ThemeColor::Accent, "→ "),
        hint: Box::new(|text: &str| theme().fg(ThemeColor::Dim, text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_themes_parse_and_resolve() {
        for (name, json) in BUILTIN_THEMES.iter() {
            let theme = Theme::from_json(json, Some(ColorMode::Truecolor), None)
                .unwrap_or_else(|e| panic!("builtin {name} failed: {e}"));
            assert_eq!(theme.name.as_deref(), Some(name.as_str()));
            // every fg/bg key resolves
            for key in ThemeColor::ALL {
                let _ = theme.get_fg_ansi(key);
            }
            for key in ThemeBg::ALL {
                let _ = theme.get_bg_ansi(key);
            }
        }
    }

    #[test]
    fn fg_resets_only_foreground() {
        let json = BUILTIN_THEMES.get("dark").unwrap();
        let theme = Theme::from_json(json, Some(ColorMode::Truecolor), None).unwrap();
        let styled = theme.fg(ThemeColor::Accent, "x");
        assert!(styled.ends_with("x\x1b[39m"), "{styled:?}");
        let bg = theme.bg(ThemeBg::SelectedBg, "x");
        assert!(bg.ends_with("x\x1b[49m"), "{bg:?}");
    }

    #[test]
    fn missing_colors_error_lists_tokens() {
        let err = parse_theme_json_content("t", r#"{"name":"t","colors":{}}"#).unwrap_err();
        assert!(err.starts_with("Invalid theme \"t\":\n"), "{err}");
        assert!(err.contains("Missing required color tokens:"), "{err}");
        assert!(err.contains("  - accent"), "{err}");
        assert!(
            err.contains("Please add these colors to your theme's \"colors\" object."),
            "{err}"
        );
    }

    #[test]
    fn theme_name_slash_rejected() {
        let mut json: Value = serde_json::from_str(DARK_THEME_JSON).unwrap();
        json["name"] = Value::String("a/b".to_owned());
        let err = parse_theme_json("x", &json).unwrap_err();
        assert!(err.contains("theme names cannot contain \"/\""), "{err}");
    }

    #[test]
    fn circular_var_refs_detected() {
        let content = r#"{"name":"t","vars":{"a":"b","b":"a"},"colors":{}}"#;
        // colors empty -> missing tokens error first; craft full colors via dark then poison
        let mut json: Value = serde_json::from_str(DARK_THEME_JSON).unwrap();
        json["vars"]["cyan"] = Value::String("blue2".to_owned());
        json["vars"]["blue2"] = Value::String("cyan".to_owned());
        let parsed = parse_theme_json("dark", &json).unwrap();
        let err = Theme::from_json(&parsed, Some(ColorMode::Truecolor), None).unwrap_err();
        assert!(
            err.contains("Circular variable reference detected"),
            "{err}"
        );
        let _ = content;
    }

    #[test]
    fn rgb_to_256_matches_oracle_semantics() {
        // pure gray prefers grayscale ramp
        let gray = rgb_to_256(0x80, 0x80, 0x80);
        assert!((232..=255).contains(&gray), "{gray}");
        // saturated color prefers cube
        let red = rgb_to_256(0xff, 0x00, 0x00);
        assert_eq!(red, 196);
    }

    #[test]
    fn ansi_256_to_hex_ramps() {
        assert_eq!(ansi_256_to_hex(196), "#ff0000");
        assert_eq!(ansi_256_to_hex(232), "#080808");
        assert_eq!(ansi_256_to_hex(255), "#eeeeee");
        assert_eq!(ansi_256_to_hex(0), "#000000");
    }

    #[test]
    fn auto_theme_setting_parses_pairs() {
        assert_eq!(
            parse_auto_theme_setting(Some("light/dark")),
            Some(("light".to_owned(), "dark".to_owned()))
        );
        assert_eq!(parse_auto_theme_setting(Some("a/b/c")), None);
        assert_eq!(parse_auto_theme_setting(Some("plain")), None);
        assert_eq!(parse_auto_theme_setting(Some(" / ")), None);
        assert_eq!(
            resolve_theme_setting(Some("l/d"), TerminalTheme::Light),
            Some("l".to_owned())
        );
        assert_eq!(
            resolve_theme_setting(Some("l/d"), TerminalTheme::Dark),
            Some("d".to_owned())
        );
        assert_eq!(
            resolve_theme_setting(Some("a/b/c"), TerminalTheme::Dark),
            None
        );
        assert_eq!(
            resolve_theme_setting(Some("solar"), TerminalTheme::Dark),
            Some("solar".to_owned())
        );
    }

    #[test]
    fn colorfgbg_detection() {
        let d = detect_terminal_background_from_env(Some("15;0"));
        assert_eq!(d.theme, TerminalTheme::Dark);
        assert_eq!(d.source, "COLORFGBG");
        let l = detect_terminal_background_from_env(Some("0;15"));
        assert_eq!(l.theme, TerminalTheme::Light);
        let f = detect_terminal_background_from_env(Some(""));
        assert_eq!(f.source, "fallback");
        assert_eq!(f.theme, TerminalTheme::Dark);
        assert_eq!(f.confidence, "low");
    }

    #[test]
    fn osc11_detection_prefers_query() {
        let d = detect_terminal_background_theme(
            || {
                Some(RgbColor {
                    r: 255,
                    g: 255,
                    b: 255,
                })
            },
            Some("15;0"),
        );
        assert_eq!(d.theme, TerminalTheme::Light);
        assert_eq!(d.source, "terminal background");
        assert_eq!(d.detail, "OSC 11 background rgb(255, 255, 255)");
        let f = detect_terminal_background_theme(|| None, Some("15;0"));
        assert_eq!(f.source, "COLORFGBG");
    }
}
