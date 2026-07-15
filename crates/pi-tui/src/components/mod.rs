//! Simple pi-tui widgets (port of packages/tui/src/components/*).

pub mod box_widget;
pub mod editor;
pub mod image;
pub mod input;
pub mod loader;
pub mod markdown;
pub mod select_list;
pub mod settings_list;
pub mod spacer;
pub mod text;
pub mod truncated_text;

pub use box_widget::BoxWidget;
pub use editor::{
    Editor, EditorOptions, EditorTheme, EditorTui, TextChunk, word_wrap_line,
    word_wrap_line_atomic, word_wrap_line_with_segments, wordWrapLine,
};
pub use image::{Image, ImageOptions, ImageTheme};
pub use input::{Input, byte_to_utf16, utf16_len, utf16_to_byte};
pub use loader::{Loader, LoaderIndicatorOptions};
pub use markdown::{
    DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme, default_markdown_theme,
    syntect_highlight_code,
};
pub use select_list::{
    SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme,
    SelectListTruncatePrimaryContext,
};
pub use settings_list::{SettingItem, SettingsList, SettingsListOptions, SettingsListTheme};
pub use spacer::Spacer;
pub use text::Text;
pub use truncated_text::TruncatedText;
