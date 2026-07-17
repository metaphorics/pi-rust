//! Utilities for formatting keybinding hints in the UI.

use pi_tui::keybindings::get_keybindings;

use crate::modes::interactive::theme::{ThemeColor, theme};

pub fn format_key_text(key: &str, capitalize: bool) -> String {
    key.split('/')
        .map(|alternative| {
            alternative
                .split('+')
                .map(|part| {
                    let part = if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
                        "option"
                    } else {
                        part
                    };
                    if capitalize {
                        let mut chars = part.chars();
                        chars
                            .next()
                            .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                            .unwrap_or_default()
                    } else {
                        part.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}
pub fn key_text(keybinding: &str) -> String {
    format_key_text(&get_keybindings().get_keys(keybinding).join("/"), false)
}
pub fn key_display_text(keybinding: &str) -> String {
    format_key_text(&get_keybindings().get_keys(keybinding).join("/"), true)
}
pub fn key_hint(keybinding: &str, description: &str) -> String {
    format!(
        "{}{}",
        theme().fg(ThemeColor::Dim, &key_text(keybinding)),
        theme().fg(ThemeColor::Muted, &format!(" {description}"))
    )
}

pub fn raw_key_hint(key: &str, description: &str) -> String {
    format!(
        "{}{}",
        theme().fg(ThemeColor::Dim, &format_key_text(key, false)),
        theme().fg(ThemeColor::Muted, &format!(" {description}"))
    )
}

#[cfg(test)]
mod tests {
    use super::format_key_text;
    #[test]
    fn formats_alternatives_and_capitalization() {
        assert_eq!(format_key_text("ctrl+c/alt+x", true), "Ctrl+C/Alt+X");
    }
}
