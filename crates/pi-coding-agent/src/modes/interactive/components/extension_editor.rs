//! Multi-line editor dialog for extensions.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use pi_tui::component::{Component, Focusable, RenderStatus};
use pi_tui::components::{Editor, EditorOptions, EditorTheme, EditorTui, Text};
use pi_tui::line::Line;

use super::keybinding_hints::key_hint;
use crate::modes::interactive::theme::{ThemeColor, theme};

pub struct ExtensionEditor<'a> {
    pub title: String,
    editor: Editor<'a>,
    focused: bool,
    external_editor_command: Option<String>,
    before_external_editor: Option<Box<dyn FnMut()>>,
    after_external_editor: Option<Box<dyn FnMut()>>,
    pub on_submit: Option<Box<dyn FnMut(String)>>,
    pub on_cancel: Option<Box<dyn FnMut()>>,
    cached: Vec<Line>,
}

impl<'a> ExtensionEditor<'a> {
    #[must_use]
    pub fn new(
        tui: &'a dyn EditorTui,
        title: impl Into<String>,
        prefill: Option<&str>,
        options: Option<EditorOptions>,
    ) -> Self {
        let mut editor = Editor::with_options(tui, EditorTheme, options.unwrap_or_default());
        if let Some(prefill) = prefill {
            editor.set_text(prefill);
        }
        Self {
            title: title.into(),
            editor,
            focused: false,
            external_editor_command: None,
            before_external_editor: None,
            after_external_editor: None,
            on_submit: None,
            on_cancel: None,
            cached: Vec::new(),
        }
    }

    /// `'static` variant over a shared [`EditorTui`] handle (the mode's
    /// editor-signal seam), for dialogs stored in the component tree.
    #[must_use]
    pub fn with_shared_tui(
        tui: std::rc::Rc<dyn EditorTui>,
        title: impl Into<String>,
        prefill: Option<&str>,
        options: Option<EditorOptions>,
    ) -> ExtensionEditor<'static> {
        let mut editor = Editor::with_shared_tui(tui, EditorTheme, options.unwrap_or_default());
        if let Some(prefill) = prefill {
            editor.set_text(prefill);
        }
        ExtensionEditor {
            title: title.into(),
            editor,
            focused: false,
            external_editor_command: None,
            before_external_editor: None,
            after_external_editor: None,
            on_submit: None,
            on_cancel: None,
            cached: Vec::new(),
        }
    }

    pub fn set_external_editor_command(&mut self, command: Option<String>) {
        self.external_editor_command = command;
    }
    pub fn set_external_editor_lifecycle(
        &mut self,
        before: impl FnMut() + 'static,
        after: impl FnMut() + 'static,
    ) {
        self.before_external_editor = Some(Box::new(before));
        self.after_external_editor = Some(Box::new(after));
    }

    #[must_use]
    pub fn value(&self) -> String {
        self.editor.get_text()
    }

    pub fn set_value(&mut self, value: &str) {
        self.editor.set_text(value);
    }

    fn external_editor_command(&self) -> String {
        self.external_editor_command
            .clone()
            .or_else(|| std::env::var("VISUAL").ok())
            .or_else(|| std::env::var("EDITOR").ok())
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".to_owned()
                } else {
                    "nano".to_owned()
                }
            })
    }

    fn open_external_editor(&mut self) {
        let command = self.external_editor_command();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let path: PathBuf =
            std::env::temp_dir().join(format!("pi-extension-editor-{timestamp}.md"));
        if std::fs::write(&path, self.editor.get_text()).is_err() {
            return;
        }
        let mut parts = command.split_whitespace();
        let Some(program) = parts.next() else {
            let _ = std::fs::remove_file(path);
            return;
        };
        if let Some(before) = &mut self.before_external_editor {
            before();
        }
        let status = std::process::Command::new(program)
            .args(parts)
            .arg(&path)
            .status();
        if status.is_ok_and(|status| status.success())
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            self.editor
                .set_text(content.strip_suffix('\n').unwrap_or(&content));
        }
        let _ = std::fs::remove_file(path);
        if let Some(after) = &mut self.after_external_editor {
            after();
        }
    }
}

fn append_text(lines: &mut Vec<Line>, text: &str, width: u16) {
    let mut component = Text::new(text, 1, 0, None);
    lines.extend_from_slice(component.render(width));
}

impl Component for ExtensionEditor<'_> {
    fn render(&mut self, width: u16) -> &[Line] {
        self.cached.clear();
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &theme().fg(ThemeColor::Accent, &self.title),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.extend_from_slice(self.editor.render(width));
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        append_text(
            &mut self.cached,
            &format!(
                "{}  {}  {}  {}",
                key_hint("tui.select.confirm", "submit"),
                key_hint("tui.input.newLine", "newline"),
                key_hint("tui.select.cancel", "cancel"),
                key_hint("app.editor.external", "external editor")
            ),
            width,
        );
        self.cached
            .push(Line::plain(" ".repeat(usize::from(width))));
        self.cached.push(Line::from_ansi(
            &theme().fg(ThemeColor::Border, &"─".repeat(usize::from(width))),
        ));
        &self.cached
    }

    fn invalidate(&mut self) {
        self.editor.invalidate();
    }

    fn handle_input(&mut self, data: &str) {
        let (cancel, external, confirm) = {
            let keybindings = pi_tui::keybindings::get_keybindings();
            (
                keybindings.matches(data, "tui.select.cancel"),
                keybindings.matches(data, "app.editor.external"),
                keybindings.matches(data, "tui.select.confirm"),
            )
        };
        if cancel {
            if let Some(on_cancel) = &mut self.on_cancel {
                on_cancel();
            }
        } else if external {
            self.open_external_editor();
        } else if confirm {
            if let Some(on_submit) = &mut self.on_submit {
                on_submit(self.editor.get_expanded_text().trim().to_owned());
            }
        } else {
            self.editor.handle_input(data);
        }
    }

    fn last_render_status(&self) -> RenderStatus {
        RenderStatus::Changed
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
}

impl Focusable for ExtensionEditor<'_> {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.editor.set_focused(focused);
    }
}
