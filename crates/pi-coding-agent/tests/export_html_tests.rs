use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use pi_coding_agent::{ExportHtmlOptions, export_session_file_to_html};
use serde_json::{Value, json};

const PI_SESSION: &str = include_str!("fixtures/pi-written-session.jsonl");

fn current_session_fixture(dir: &Path, malicious: bool) -> PathBuf {
    let mut lines: Vec<Value> = PI_SESSION
        .lines()
        .map(|line| serde_json::from_str(line).expect("pi fixture line"))
        .collect();
    lines[0]["version"] = json!(3);
    let mut parent: Option<String> = None;
    for (index, entry) in lines.iter_mut().skip(1).enumerate() {
        let id = format!("entry-{index:02}");
        entry["id"] = json!(id);
        entry["parentId"] = parent.as_ref().map_or(Value::Null, |id| json!(id));
        parent = Some(id);
    }
    if malicious {
        let id = "entry-xss";
        lines.push(json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": "2025-11-20T23:34:00.000Z",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "# Safe heading\n<script>globalThis.__piExportXss = true</script>\n[bad](javascript:alert(1))\n```html\n<img src=x onerror=alert(2)>\n```"
                    },
                    {
                        "type": "image",
                        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
                        "mimeType": "image/png"
                    }
                ],
                "timestamp": 1_763_681_640_000_i64
            }
        }));
    }
    let path = dir.join("pi-written-current.jsonl");
    let body = lines
        .into_iter()
        .map(|line| serde_json::to_string(&line).expect("serialize fixture"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{body}\n")).expect("write current fixture");
    path
}

fn embedded_session_data(html: &str) -> Value {
    let start_tag = r#"<script id="session-data" type="application/json">"#;
    let start = html.find(start_tag).expect("session data script") + start_tag.len();
    let end = html[start..].find("</script>").expect("session data end") + start;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&html[start..end])
        .expect("base64 session data");
    serde_json::from_slice(&bytes).expect("session JSON")
}

#[test]
fn exports_real_pi_session_as_standalone_deterministic_document() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = current_session_fixture(temp.path(), false);
    let first = temp.path().join("first.html");
    let second = temp.path().join("second.html");

    export_session_file_to_html(
        &input,
        ExportHtmlOptions {
            output_path: Some(first.clone()),
            theme_name: Some("dark".to_string()),
            tool_renderer: None,
        },
    )
    .expect("first export");
    export_session_file_to_html(
        &input,
        ExportHtmlOptions {
            output_path: Some(second.clone()),
            theme_name: Some("dark".to_string()),
            tool_renderer: None,
        },
    )
    .expect("second export");

    let first_bytes = fs::read(&first).expect("first HTML");
    let second_bytes = fs::read(&second).expect("second HTML");
    assert_eq!(
        first_bytes, second_bytes,
        "identical input must produce identical bytes"
    );
    let html = String::from_utf8(first_bytes).expect("UTF-8 HTML");
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(html.contains("* marked v18.0.5"), "marked must be embedded");
    assert!(
        html.contains("Highlight.js"),
        "highlight.js must be embedded"
    );
    assert!(!html.contains("<script src="), "export must not need a CDN");
    assert!(!html.contains("{{SESSION_DATA}}"));

    let data = embedded_session_data(&html);
    assert_eq!(data["header"]["id"], "d703a1a9-1b7b-4fb1-b512-c9738b1fe617");
    assert_eq!(data["leafId"], "entry-08");
    assert_eq!(data["entries"].as_array().map(Vec::len), Some(9));
    let encoded = serde_json::to_string(&data).expect("session data text");
    assert!(encoded.contains("toolu_017qEkVzzPb7b7o4FkgJLF23"));
    assert!(encoded.contains("Pi Coding Agent Themes"));
}

#[test]
fn session_data_is_not_injected_into_document_source_and_images_are_embedded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let input = current_session_fixture(temp.path(), true);
    let output = std::env::var_os("PI_HTML_TEST_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| temp.path().join("safe.html"));

    export_session_file_to_html(
        &input,
        ExportHtmlOptions {
            output_path: Some(output.clone()),
            theme_name: Some("light".to_string()),
            tool_renderer: None,
        },
    )
    .expect("safe export");
    let html = fs::read_to_string(&output).expect("HTML");
    assert!(!html.contains("globalThis.__piExportXss = true"));
    assert!(!html.contains("javascript:alert(1)"));
    assert!(html.contains("--exportPageBg: #f8f8f8"));

    let data = embedded_session_data(&html);
    let encoded = serde_json::to_string(&data).expect("session JSON");
    assert!(encoded.contains("globalThis.__piExportXss"));
    assert!(encoded.contains("image/png"));
    assert!(encoded.contains("iVBORw0KGgo"));
}

#[test]
fn reports_oracle_file_and_theme_errors_and_default_name() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_name = format!("pi-export-missing-{}.jsonl", std::process::id());
    let error = export_session_file_to_html(&missing_name, ExportHtmlOptions::default())
        .expect_err("missing file must fail");
    let missing = std::env::current_dir().expect("cwd").join(missing_name);
    assert_eq!(error, format!("File not found: {}", missing.display()));

    let input = current_session_fixture(temp.path(), false);
    let error = export_session_file_to_html(
        &input,
        ExportHtmlOptions {
            output_path: Some(temp.path().join("unused.html")),
            theme_name: Some("not-a-theme".to_string()),
            tool_renderer: None,
        },
    )
    .expect_err("unknown theme must fail");
    assert_eq!(error, "Theme not found: not-a-theme");

    let expected = PathBuf::from("pi-session-pi-written-current.html");
    let result = export_session_file_to_html(&input, ExportHtmlOptions::default())
        .expect("default output path");
    assert_eq!(result, expected);
    assert!(expected.exists());
    fs::remove_file(&expected).expect("remove default export");

    let result = export_session_file_to_html(
        &input,
        ExportHtmlOptions {
            output_path: Some(PathBuf::new()),
            theme_name: None,
            tool_renderer: None,
        },
    )
    .expect("empty output path uses default");
    assert_eq!(result, expected);
    fs::remove_file(&expected).expect("remove empty-path export");

    let file_url_output = temp.path().join("from-file-url.html");
    let file_url = url::Url::from_file_path(&input).expect("file URL");
    export_session_file_to_html(
        Path::new(file_url.as_str()),
        ExportHtmlOptions {
            output_path: Some(file_url_output.clone()),
            theme_name: None,
            tool_renderer: None,
        },
    )
    .expect("file URL input");
    assert!(file_url_output.exists());

    let text_input = temp.path().join("session.backup");
    fs::copy(&input, &text_input).expect("copy non-jsonl session");
    let expected = PathBuf::from("pi-session-session.backup.html");
    let result = export_session_file_to_html(&text_input, ExportHtmlOptions::default())
        .expect("non-jsonl default output path");
    assert_eq!(result, expected);
    fs::remove_file(&expected).expect("remove non-jsonl export");
}
