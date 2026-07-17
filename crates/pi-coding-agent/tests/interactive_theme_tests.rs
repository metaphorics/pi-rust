use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use pi_coding_agent::modes::interactive::theme::syntax::{get_language_from_path, highlight_code};
use pi_coding_agent::modes::interactive::theme::{
    ColorMode, DARK_THEME_JSON, LIGHT_THEME_JSON, Theme, ThemeColor, current_theme_name,
    init_theme, on_theme_change, parse_theme_json_content, theme,
};

static THEME_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[test]
fn shipped_themes_emit_sgr_in_both_color_modes() {
    for (name, json) in [("dark", DARK_THEME_JSON), ("light", LIGHT_THEME_JSON)] {
        let parsed = parse_theme_json_content(name, json).expect("shipped theme is valid");
        for mode in [ColorMode::Truecolor, ColorMode::Color256] {
            let loaded = Theme::from_json(&parsed, Some(mode), None).expect("theme loads");
            for color in [
                ThemeColor::Accent,
                ThemeColor::MdCodeBlock,
                ThemeColor::SyntaxKeyword,
                ThemeColor::ToolDiffAdded,
            ] {
                assert!(loaded.fg(color, "sample").starts_with("\x1b["));
            }
        }
    }
}

#[test]
fn highlighter_uses_syntax_keyword_and_unknown_uses_code_block() {
    let _guard = THEME_TEST_LOCK.lock().expect("theme test lock");
    init_theme(Some("dark"), false);
    let current = theme();
    let keyword = current.get_fg_ansi(ThemeColor::SyntaxKeyword).to_owned();
    let highlighted = highlight_code("if (answer) {}", Some("javascript"));
    assert!(highlighted.iter().any(|line| line.contains(&keyword)));

    let unknown = highlight_code("one\ntwo", Some("not-a-language"));
    assert_eq!(unknown.len(), 2);
    assert_eq!(unknown[0], current.fg(ThemeColor::MdCodeBlock, "one"));
    assert_eq!(unknown[1], current.fg(ThemeColor::MdCodeBlock, "two"));
}

#[test]
fn language_extensions_match_oracle_table() {
    for (extension, language) in [
        ("ts", "typescript"),
        ("tsx", "typescript"),
        ("js", "javascript"),
        ("jsx", "javascript"),
        ("mjs", "javascript"),
        ("cjs", "javascript"),
        ("py", "python"),
        ("rb", "ruby"),
        ("rs", "rust"),
        ("go", "go"),
        ("java", "java"),
        ("kt", "kotlin"),
        ("swift", "swift"),
        ("c", "c"),
        ("h", "c"),
        ("cpp", "cpp"),
        ("cc", "cpp"),
        ("cxx", "cpp"),
        ("hpp", "cpp"),
        ("cs", "csharp"),
        ("php", "php"),
        ("sh", "bash"),
        ("bash", "bash"),
        ("zsh", "bash"),
        ("fish", "fish"),
        ("ps1", "powershell"),
        ("sql", "sql"),
        ("html", "html"),
        ("htm", "html"),
        ("css", "css"),
        ("scss", "scss"),
        ("sass", "sass"),
        ("less", "less"),
        ("json", "json"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("toml", "toml"),
        ("xml", "xml"),
        ("md", "markdown"),
        ("markdown", "markdown"),
        ("dockerfile", "dockerfile"),
        ("makefile", "makefile"),
        ("cmake", "cmake"),
        ("lua", "lua"),
        ("perl", "perl"),
        ("r", "r"),
        ("scala", "scala"),
        ("clj", "clojure"),
        ("ex", "elixir"),
        ("exs", "elixir"),
        ("erl", "erlang"),
        ("hs", "haskell"),
        ("ml", "ocaml"),
        ("vim", "vim"),
        ("graphql", "graphql"),
        ("proto", "protobuf"),
        ("tf", "hcl"),
        ("hcl", "hcl"),
    ] {
        assert_eq!(
            get_language_from_path(&format!("file.{extension}")),
            Some(language)
        );
    }
    assert_eq!(get_language_from_path("file.mli"), None);
}

#[test]
fn custom_theme_watcher_reloads_and_notifies() {
    let _guard = THEME_TEST_LOCK.lock().expect("theme test lock");
    let temp = tempfile::tempdir().expect("temporary agent directory");
    let themes = temp.path().join("themes");
    std::fs::create_dir(&themes).expect("themes directory");
    let theme_path = themes.join("watch-test.json");
    let original = DARK_THEME_JSON.replace("\t\"name\": \"dark\"", "\t\"name\": \"watch-test\"");
    std::fs::write(&theme_path, &original).expect("initial custom theme");

    // SAFETY: this test serializes all theme-global environment access.
    unsafe { std::env::set_var("PI_CODING_AGENT_DIR", temp.path()) };
    let changes = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&changes);
    on_theme_change(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    });
    init_theme(Some("watch-test"), true);
    assert_eq!(current_theme_name().as_deref(), Some("watch-test"));
    let before = theme().get_fg_ansi(ThemeColor::Accent).to_owned();

    std::thread::sleep(Duration::from_millis(150));
    let changed = original.replace("#8abeb7", "#ff0000");
    std::fs::write(&theme_path, changed).expect("modified custom theme");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && (theme().get_fg_ansi(ThemeColor::Accent) == before
            || changes.load(Ordering::SeqCst) == 0)
    {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_ne!(theme().get_fg_ansi(ThemeColor::Accent), before);
    assert!(changes.load(Ordering::SeqCst) > 0);

    // SAFETY: the mutex prevents concurrent access by this integration test.
    unsafe { std::env::remove_var("PI_CODING_AGENT_DIR") };
    init_theme(Some("dark"), false);
}
