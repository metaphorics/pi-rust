//! Interactive-mode extension UI plumbing (Phase 6 C8 / plan flag F2).
//!
//! Two halves:
//! - [`InteractiveUiHost`] — the `Send + Sync` [`ExtensionUiHost`] bound into
//!   the extension binding. Dialog and void-setter calls arrive on tokio
//!   threads; they are channeled to the TUI loop (same pattern as the OAuth
//!   dialog queue) and resolved with oneshot replies. Theme catalog getters
//!   are pure and answered inline (they must work at sidecar boot, before
//!   the loop ever pumps).
//! - Wire-shape helpers the loop consumes: overlay-option parsing
//!   ([`parse_overlay_options`]) and theme-DTO construction
//!   ([`theme_catalog`], [`theme_dto`], [`current_theme_dto`]).

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use pi_agent::CancellationToken;
use pi_ext_protocol::ThemeDto;
use pi_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::extension_bridge::{
    BoxFuture, ExtensionUiHost, NotifyType, ThemeCatalogItem, UiDialogOptions, WidgetPlacement,
};

use super::theme::{
    DARK_THEME_JSON, LIGHT_THEME_JSON, current_theme_name, get_available_themes_with_paths,
};

// ============================================================================
// Loop-bound host requests
// ============================================================================

/// Extension UI traffic queued for the interactive loop (drained per pump
/// tick, like the OAuth queue).
pub enum UiHostRequest {
    Select {
        title: String,
        options: Vec<String>,
        timeout_ms: Option<u64>,
        cancel: Option<CancellationToken>,
        respond: oneshot::Sender<Option<String>>,
    },
    Confirm {
        title: String,
        message: String,
        timeout_ms: Option<u64>,
        cancel: Option<CancellationToken>,
        respond: oneshot::Sender<bool>,
    },
    Input {
        title: String,
        placeholder: Option<String>,
        timeout_ms: Option<u64>,
        cancel: Option<CancellationToken>,
        respond: oneshot::Sender<Option<String>>,
    },
    EditorDialog {
        title: String,
        prefill: Option<String>,
        respond: oneshot::Sender<Option<String>>,
    },
    /// `ui.custom` — host the bridged component under `slot` until the
    /// sidecar reports `ui/done` (or the request is cancelled). The reply
    /// resolves the pending `ui/custom` RPC.
    Custom {
        slot: String,
        overlay: bool,
        overlay_options: Option<Value>,
        cancel: CancellationToken,
        respond: oneshot::Sender<()>,
    },
    Notify {
        message: String,
        level: Option<NotifyType>,
    },
    SetStatus {
        key: String,
        text: Option<String>,
    },
    SetWidget {
        key: String,
        lines: Option<Vec<String>>,
        placement: Option<WidgetPlacement>,
    },
    SetTitle(String),
    SetEditorText(String),
}

// ============================================================================
// ExtensionUiHost implementation
// ============================================================================

/// Interactive [`ExtensionUiHost`]: channels calls into the loop.
pub struct InteractiveUiHost {
    tx: Sender<UiHostRequest>,
}

impl InteractiveUiHost {
    /// Returns the host plus the loop-side receiver
    /// (`InteractiveMode::attach_extensions` consumes the receiver).
    #[must_use]
    pub fn channel() -> (
        std::sync::Arc<Self>,
        std::sync::mpsc::Receiver<UiHostRequest>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        (std::sync::Arc::new(Self { tx }), rx)
    }

    fn send(&self, request: UiHostRequest) {
        // A dropped receiver (mode gone) makes every dialog resolve with its
        // cancel fallback through the dropped oneshot sender.
        let _ = self.tx.send(request);
    }
}

fn await_reply<T: Send + 'static>(
    rx: oneshot::Receiver<T>,
    fallback: impl FnOnce() -> T + Send + 'static,
) -> BoxFuture<'static, T> {
    Box::pin(async move { rx.await.unwrap_or_else(|_| fallback()) })
}

impl ExtensionUiHost for InteractiveUiHost {
    fn select(
        &self,
        title: String,
        options: Vec<String>,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(UiHostRequest::Select {
            title,
            options,
            timeout_ms: opts.timeout_ms,
            cancel: opts.signal,
            respond: tx,
        });
        await_reply(rx, || None)
    }

    fn confirm(
        &self,
        title: String,
        message: String,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, bool> {
        let (tx, rx) = oneshot::channel();
        self.send(UiHostRequest::Confirm {
            title,
            message,
            timeout_ms: opts.timeout_ms,
            cancel: opts.signal,
            respond: tx,
        });
        await_reply(rx, || false)
    }

    fn input(
        &self,
        title: String,
        placeholder: Option<String>,
        opts: UiDialogOptions,
    ) -> BoxFuture<'static, Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(UiHostRequest::Input {
            title,
            placeholder,
            timeout_ms: opts.timeout_ms,
            cancel: opts.signal,
            respond: tx,
        });
        await_reply(rx, || None)
    }

    fn editor(&self, title: String, prefill: Option<String>) -> BoxFuture<'static, Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(UiHostRequest::EditorDialog {
            title,
            prefill,
            respond: tx,
        });
        await_reply(rx, || None)
    }

    fn custom(
        &self,
        slot: String,
        overlay: bool,
        overlay_options: Option<Value>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, ()> {
        let (tx, rx) = oneshot::channel();
        self.send(UiHostRequest::Custom {
            slot,
            overlay,
            overlay_options,
            cancel,
            respond: tx,
        });
        await_reply(rx, || ())
    }

    fn notify(&self, message: String, notify_type: Option<NotifyType>) {
        self.send(UiHostRequest::Notify {
            message,
            level: notify_type,
        });
    }

    fn set_status(&self, key: String, text: Option<String>) {
        self.send(UiHostRequest::SetStatus { key, text });
    }

    fn set_widget(
        &self,
        key: String,
        lines: Option<Vec<String>>,
        placement: Option<WidgetPlacement>,
    ) {
        // Widgets normally arrive as `ui/frame` slots; this path exists for
        // hosts driving the trait directly (RPC parity).
        self.send(UiHostRequest::SetWidget {
            key,
            lines,
            placement,
        });
    }

    fn set_title(&self, title: String) {
        self.send(UiHostRequest::SetTitle(title));
    }

    fn set_editor_text(&self, text: String) {
        self.send(UiHostRequest::SetEditorText(text));
    }

    fn get_all_themes(&self) -> Vec<ThemeCatalogItem> {
        theme_catalog()
    }

    fn get_theme_json(&self, name: &str) -> Option<(String, Value)> {
        theme_dto(name).map(|dto| (dto.name, dto.json))
    }
}

// ============================================================================
// Theme catalog (F8: real theme JSON for init/state + ui/getAllThemes)
// ============================================================================

/// Builtin + registered themes with their source paths (`None` = embedded
/// builtin; the sidecar resolves those from its own npm copy).
#[must_use]
pub fn theme_catalog() -> Vec<ThemeCatalogItem> {
    get_available_themes_with_paths()
        .into_iter()
        .map(|info| ThemeCatalogItem {
            name: info.name,
            path: info.path,
        })
        .collect()
}

/// Resolved theme JSON by name: embedded builtins parse their bundled
/// content; registered themes re-read their source file.
#[must_use]
pub fn theme_dto(name: &str) -> Option<ThemeDto> {
    let json: Value = match name {
        "dark" => serde_json::from_str(DARK_THEME_JSON).ok()?,
        "light" => serde_json::from_str(LIGHT_THEME_JSON).ok()?,
        _ => {
            let info = get_available_themes_with_paths()
                .into_iter()
                .find(|info| info.name == name)?;
            let path: PathBuf = info.path?;
            let content = std::fs::read_to_string(path).ok()?;
            serde_json::from_str(&content).ok()?
        }
    };
    Some(ThemeDto {
        name: name.to_string(),
        json,
    })
}

/// The active theme as a wire DTO (init baseline / `state/update.theme`).
/// Falls back to the embedded dark theme so the sidecar never receives the
/// empty-object placeholder that pi's loader rejects.
#[must_use]
pub fn current_theme_dto() -> ThemeDto {
    let name = current_theme_name().unwrap_or_else(|| "dark".to_owned());
    theme_dto(&name)
        .or_else(|| theme_dto("dark"))
        .expect("embedded dark theme parses")
}

// ============================================================================
// Overlay options (wire JSON → pi-tui OverlayOptions)
// ============================================================================

/// Parsed `ui/overlay` options: layout plus the sidecar-mirrored handle
/// state (`hidden`, `focused`) that rides the same JSON object.
pub struct ParsedOverlay {
    pub options: OverlayOptions,
    pub hidden: bool,
    /// `None` = never specified (initial non-handle overlays).
    pub focused: Option<bool>,
}

fn size_value(value: &Value) -> Option<SizeValue> {
    match value {
        Value::Number(n) => n.as_f64().map(|v| SizeValue::Abs(v.max(0.0) as usize)),
        Value::String(s) => {
            let s = s.trim();
            let percent = s.strip_suffix('%')?;
            percent.trim().parse::<f64>().ok().map(SizeValue::Percent)
        }
        _ => None,
    }
}

fn anchor(value: &Value) -> OverlayAnchor {
    match value.as_str().unwrap_or("center") {
        "top-left" => OverlayAnchor::TopLeft,
        "top-right" => OverlayAnchor::TopRight,
        "bottom-left" => OverlayAnchor::BottomLeft,
        "bottom-right" => OverlayAnchor::BottomRight,
        "top-center" => OverlayAnchor::TopCenter,
        "bottom-center" => OverlayAnchor::BottomCenter,
        "left-center" => OverlayAnchor::LeftCenter,
        "right-center" => OverlayAnchor::RightCenter,
        _ => OverlayAnchor::Center,
    }
}

/// Parse wire overlay options. Unknown fields are ignored; a `visible()`
/// gate function cannot cross the wire (the sidecar's handle path re-ships
/// `hidden` on every observable change instead).
#[must_use]
pub fn parse_overlay_options(value: &Value) -> ParsedOverlay {
    let get = |key: &str| value.get(key);
    let mut options = OverlayOptions::default();
    if let Some(v) = get("width") {
        options.width = size_value(v);
    }
    if let Some(v) = get("minWidth").and_then(Value::as_u64) {
        options.min_width = Some(v as usize);
    }
    if let Some(v) = get("maxHeight") {
        options.max_height = size_value(v);
    }
    if let Some(v) = get("anchor") {
        options.anchor = anchor(v);
    }
    if let Some(v) = get("offsetX").and_then(Value::as_i64) {
        options.offset_x = v as i32;
    }
    if let Some(v) = get("offsetY").and_then(Value::as_i64) {
        options.offset_y = v as i32;
    }
    if let Some(v) = get("row") {
        options.row = size_value(v);
    }
    if let Some(v) = get("col") {
        options.col = size_value(v);
    }
    if let Some(v) = get("nonCapturing").and_then(Value::as_bool) {
        options.non_capturing = v;
    }
    ParsedOverlay {
        options,
        hidden: get("hidden").and_then(Value::as_bool).unwrap_or(false),
        focused: get("focused").and_then(Value::as_bool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn overlay_options_parse_sizes_anchors_and_state() {
        let parsed = parse_overlay_options(&json!({
            "width": "50%",
            "minWidth": 20,
            "maxHeight": 12,
            "anchor": "top-right",
            "offsetX": -2,
            "offsetY": 1,
            "hidden": true,
            "focused": false,
        }));
        assert_eq!(parsed.options.width, Some(SizeValue::Percent(50.0)));
        assert_eq!(parsed.options.min_width, Some(20));
        assert_eq!(parsed.options.max_height, Some(SizeValue::Abs(12)));
        assert_eq!(parsed.options.anchor, OverlayAnchor::TopRight);
        assert_eq!(parsed.options.offset_x, -2);
        assert_eq!(parsed.options.offset_y, 1);
        assert!(parsed.hidden);
        assert_eq!(parsed.focused, Some(false));

        let default = parse_overlay_options(&json!({}));
        assert_eq!(default.options.anchor, OverlayAnchor::Center);
        assert!(!default.hidden);
        assert_eq!(default.focused, None);
    }

    #[test]
    fn builtin_theme_dtos_are_real_json() {
        let dark = theme_dto("dark").expect("dark parses");
        assert_eq!(dark.name, "dark");
        assert!(dark.json.get("colors").is_some());
        let current = current_theme_dto();
        assert!(current.json.get("colors").is_some());
        assert!(theme_catalog().iter().any(|item| item.name == "dark"));
    }
}
