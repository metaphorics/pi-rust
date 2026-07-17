use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use pi_ext_protocol::{
    Envelope, GetAllThemesResult, GetThemeResult, ResponseResult, TerminalInputResult,
    ToolExecuteResult, decode_frame, encode_frame,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

struct MethodFixture {
    name: &'static str,
    direction: &'static str,
    family: &'static str,
    bytes: &'static [u8],
}

macro_rules! fixture {
    ($name:literal, $direction:literal, $family:literal) => {
        MethodFixture {
            name: $name,
            direction: $direction,
            family: $family,
            bytes: include_bytes!(concat!("../fixtures/", $name)),
        }
    };
}

macro_rules! result_fixture {
    ($name:literal) => {
        ($name, include_bytes!(concat!("../fixtures/", $name)))
    };
}

fn get_method_direction(method: &str) -> &'static str {
    match method {
        "lifecycle/init" | "event/emit" | "ui/terminal_input" | "ui/focus" | "ui/resize"
        | "tool/execute" | "provider/stream" | "command/execute" | "shortcut/invoke"
        | "session/setup" | "session/sync" | "state/update" => "rust-to-sidecar",

        "lifecycle/initialized"
        | "action/sendUserMessage"
        | "ui/getTheme"
        | "ui/getAllThemes"
        | "ui/editorSubmit"
        | "ui/editorChange"
        | "ui/terminalInputActive"
        | "tool/update"
        | "provider/register"
        | "error/extension" => "sidecar-to-rust",

        _ => panic!("unknown method: {}", method),
    }
}

const METHOD_FIXTURES: &[MethodFixture] = &[
    fixture!(
        "rust-to-sidecar-lifecycle.json",
        "rust-to-sidecar",
        "lifecycle"
    ),
    fixture!(
        "sidecar-to-rust-lifecycle.json",
        "sidecar-to-rust",
        "lifecycle"
    ),
    fixture!("rust-to-sidecar-event.json", "rust-to-sidecar", "event"),
    fixture!("sidecar-to-rust-action.json", "sidecar-to-rust", "action"),
    fixture!("rust-to-sidecar-ui.json", "rust-to-sidecar", "ui"),
    fixture!("sidecar-to-rust-ui.json", "sidecar-to-rust", "ui"),
    fixture!(
        "sidecar-to-rust-ui-get-all-themes.json",
        "sidecar-to-rust",
        "ui"
    ),
    fixture!("rust-to-sidecar-ui-focus.json", "rust-to-sidecar", "ui"),
    fixture!("rust-to-sidecar-ui-resize.json", "rust-to-sidecar", "ui"),
    fixture!(
        "sidecar-to-rust-ui-editor-submit.json",
        "sidecar-to-rust",
        "ui"
    ),
    fixture!(
        "sidecar-to-rust-ui-editor-change.json",
        "sidecar-to-rust",
        "ui"
    ),
    fixture!(
        "sidecar-to-rust-ui-terminal-input-active.json",
        "sidecar-to-rust",
        "ui"
    ),
    fixture!("rust-to-sidecar-tool.json", "rust-to-sidecar", "tool"),
    fixture!("sidecar-to-rust-tool.json", "sidecar-to-rust", "tool"),
    fixture!(
        "rust-to-sidecar-provider.json",
        "rust-to-sidecar",
        "provider"
    ),
    fixture!(
        "sidecar-to-rust-provider.json",
        "sidecar-to-rust",
        "provider"
    ),
    fixture!("rust-to-sidecar-command.json", "rust-to-sidecar", "command"),
    fixture!(
        "rust-to-sidecar-shortcut.json",
        "rust-to-sidecar",
        "shortcut"
    ),
    fixture!(
        "rust-to-sidecar-session-setup.json",
        "rust-to-sidecar",
        "session"
    ),
    fixture!(
        "rust-to-sidecar-session-sync.json",
        "rust-to-sidecar",
        "session"
    ),
    fixture!("rust-to-sidecar-state.json", "rust-to-sidecar", "state"),
    fixture!("sidecar-to-rust-error.json", "sidecar-to-rust", "error"),
];

const RESULT_FIXTURES: &[(&str, &[u8])] = &[
    result_fixture!("sidecar-to-rust-ui-terminal-input-result.json"),
    result_fixture!("rust-to-sidecar-ui-theme-catalog-result.json"),
    result_fixture!("rust-to-sidecar-ui-theme-lookup-result.json"),
    result_fixture!("sidecar-to-rust-tool-execute-result.json"),
];

#[test]
fn method_families_are_byte_exact_in_every_supported_direction() {
    let mut covered = BTreeSet::new();

    for fixture in METHOD_FIXTURES {
        assert_byte_exact(fixture.name, fixture.bytes);

        let wire: Value = serde_json::from_slice(fixture.bytes).expect("fixture is JSON");
        let method = wire["method"].as_str().expect("method fixture has method");

        let expected_direction = get_method_direction(method);

        // Assert the filename prefix matches the actual method direction
        assert!(
            fixture.name.starts_with(expected_direction),
            "fixture file name {} does not start with its expected direction prefix {}",
            fixture.name,
            expected_direction
        );

        // Assert the direction parameter in the fixture! macro matches the expected direction
        assert_eq!(
            fixture.direction, expected_direction,
            "direction parameter in fixture! macro for {} does not match expected {}",
            fixture.name, expected_direction
        );

        let (derived_family, _) = method.split_once('/').expect("method must contain a slash");
        assert_eq!(
            derived_family, fixture.family,
            "family mismatch for method {}",
            method
        );

        covered.insert((expected_direction, fixture.family));
    }

    let expected = BTreeSet::from([
        ("rust-to-sidecar", "lifecycle"),
        ("rust-to-sidecar", "event"),
        ("rust-to-sidecar", "ui"),
        ("rust-to-sidecar", "tool"),
        ("rust-to-sidecar", "provider"),
        ("rust-to-sidecar", "command"),
        ("rust-to-sidecar", "shortcut"),
        ("rust-to-sidecar", "session"),
        ("rust-to-sidecar", "state"),
        ("sidecar-to-rust", "lifecycle"),
        ("sidecar-to-rust", "action"),
        ("sidecar-to-rust", "ui"),
        ("sidecar-to-rust", "tool"),
        ("sidecar-to-rust", "provider"),
        ("sidecar-to-rust", "error"),
    ]);
    assert_eq!(covered, expected);

    // Negative assertions to ensure no phantom sidecar-to-rust session/state cells exist
    assert!(
        !covered.contains(&("sidecar-to-rust", "session")),
        "phantom sidecar-to-rust session cell found"
    );
    assert!(
        !covered.contains(&("sidecar-to-rust", "state")),
        "phantom sidecar-to-rust state cell found"
    );

    // Positive assertions to ensure actual rust-to-sidecar session/state cells are present
    assert!(
        covered.contains(&("rust-to-sidecar", "session")),
        "rust-to-sidecar session cell missing"
    );
    assert!(
        covered.contains(&("rust-to-sidecar", "state")),
        "rust-to-sidecar state cell missing"
    );
}

#[test]
fn result_payloads_are_byte_exact_and_typed() {
    for (name, bytes) in RESULT_FIXTURES {
        assert_byte_exact(name, bytes);
    }

    assert_typed_ok::<TerminalInputResult>(RESULT_FIXTURES[0].1);
    assert_typed_ok::<GetAllThemesResult>(RESULT_FIXTURES[1].1);
    assert_typed_ok::<GetThemeResult>(RESULT_FIXTURES[2].1);
    assert_typed_ok::<ToolExecuteResult>(RESULT_FIXTURES[3].1);
}

#[test]
fn every_checked_in_fixture_has_a_consumer() {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let checked_in = fs::read_dir(fixture_dir)
        .expect("fixtures directory")
        .map(|entry| {
            entry
                .expect("fixture directory entry")
                .file_name()
                .into_string()
                .expect("UTF-8 fixture name")
        })
        .collect::<BTreeSet<_>>();
    let consumed = METHOD_FIXTURES
        .iter()
        .map(|fixture| fixture.name.to_owned())
        .chain(RESULT_FIXTURES.iter().map(|(name, _)| (*name).to_owned()))
        .collect::<BTreeSet<_>>();

    assert_eq!(checked_in, consumed);
}

fn assert_byte_exact(name: &str, bytes: &[u8]) {
    assert!(bytes.ends_with(b"\n"), "{name} must end with one newline");
    assert!(
        !bytes[..bytes.len() - 1].contains(&b'\n'),
        "{name} must contain one NDJSON frame"
    );
    let decoded =
        decode_frame(bytes).unwrap_or_else(|error| panic!("{name} failed to decode: {error}"));
    let encoded =
        encode_frame(&decoded).unwrap_or_else(|error| panic!("{name} failed to encode: {error}"));
    assert_eq!(
        encoded, bytes,
        "{name} is not the canonical byte-exact encoding"
    );
}

fn assert_typed_ok<T: DeserializeOwned>(bytes: &[u8]) {
    let Envelope::Response { result, .. } = decode_frame(bytes).expect("result fixture decodes")
    else {
        panic!("result fixture is not a response");
    };
    let ResponseResult::Ok { ok } = result else {
        panic!("result fixture is not successful");
    };
    serde_json::from_value::<T>(ok).expect("result fixture matches its typed payload");
}
