//! Process-level tests for the wired `pi` binary (P5-B4).
//!
//! Each test drives the real executable (`CARGO_BIN_EXE_pi`) with an
//! isolated agent dir and project cwd; nothing here touches the network
//! (`PI_OFFLINE=1`, no prompts reach a provider).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

struct Bin {
    _tmp: TempDir,
    agent_dir: PathBuf,
    project_dir: PathBuf,
}

impl Bin {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        let project_dir = tmp.path().join("project");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        Self {
            _tmp: tmp,
            agent_dir,
            project_dir,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
        command
            .current_dir(&self.project_dir)
            .env("PI_CODING_AGENT_DIR", &self.agent_dir)
            .env("PI_OFFLINE", "1")
            .env_remove("PI_CODING_AGENT_SESSION_DIR");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("spawn pi")
    }

    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pi");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait pi")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn version_prints_and_exits_zero() {
    let bin = Bin::new();
    let output = bin.run(&["--version"]);
    assert!(output.status.success());
    // Drop-in contract: prints the replaced npm pi's version, not the
    // crate version.
    assert_eq!(stdout(&output), "0.80.7\n");
    assert!(stderr(&output).is_empty());
}

#[test]
fn help_prints_usage_on_stdout() {
    let bin = Bin::new();
    let output = bin.run(&["--help"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("Usage:"), "help missing usage: {text}");
    assert!(text.contains("pi install <source>"));
    assert!(text.contains("--list-models"));
}

#[test]
fn unknown_single_dash_option_is_an_error() {
    let bin = Bin::new();
    let output = bin.run(&["-x"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Error: Unknown option: -x"));
    assert!(stdout(&output).is_empty());
}

#[test]
fn fork_conflicts_with_continue() {
    let bin = Bin::new();
    let output = bin.run(&["--fork", "abc", "--continue"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("--fork cannot be combined with --continue"),
        "got: {}",
        stderr(&output)
    );
}

#[test]
fn rpc_rejects_file_arguments() {
    let bin = Bin::new();
    let output = bin.run(&["--mode", "rpc", "@x.txt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("@file arguments are not supported in RPC mode"));
}

#[test]
fn print_mode_without_models_exits_one_with_pure_stdout() {
    let bin = Bin::new();
    let output = bin.run_with_stdin(&["-p"], "hi");
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("No models available."));
    assert!(stdout(&output).is_empty(), "stdout must stay pure");
}

#[test]
fn missing_file_argument_errors_before_dispatch() {
    let bin = Bin::new();
    let output = bin.run(&["-p", "@does-not-exist.txt", "hello"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Error: File not found:"));
}

#[test]
fn package_install_list_remove_round_trip() {
    let bin = Bin::new();
    let pkg = bin.project_dir.join("local-pkg");
    std::fs::create_dir_all(pkg.join("skills/demo")).expect("pkg dirs");
    std::fs::write(
        pkg.join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: d\n---\nbody\n",
    )
    .expect("skill");

    let install = bin.run(&["install", "./local-pkg"]);
    assert!(
        install.status.success(),
        "install failed: {}",
        stderr(&install)
    );
    let settings =
        std::fs::read_to_string(bin.agent_dir.join("settings.json")).expect("settings.json");
    assert!(settings.contains("local-pkg"), "settings: {settings}");

    let list = bin.run(&["list"]);
    assert!(list.status.success());
    assert!(stdout(&list).contains("local-pkg"));

    let remove = bin.run(&["remove", "./local-pkg"]);
    assert!(remove.status.success(), "remove: {}", stderr(&remove));
    let settings =
        std::fs::read_to_string(bin.agent_dir.join("settings.json")).expect("settings.json");
    assert!(!settings.contains("local-pkg"), "settings: {settings}");
}

#[test]
fn rpc_mode_serves_state_and_survives_garbage() {
    let bin = Bin::new();
    let output = bin
        .command()
        .args(["--mode", "rpc"])
        .env("ANTHROPIC_API_KEY", "test-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|mut child| {
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(
                    b"{\"type\":\"get_state\",\"id\":\"1\"}\n{\"type\":\"bogus\",\"id\":\"9\"}\nnot json\n",
                )
                .expect("write stdin");
            child.wait_with_output().expect("wait")
        })
        .expect("spawn");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.trim().lines().collect();
    assert!(lines[0].starts_with(
        "{\"id\":\"1\",\"type\":\"response\",\"command\":\"get_state\",\"success\":true,"
    ));
    assert_eq!(
        lines[1],
        "{\"id\":\"9\",\"type\":\"response\",\"command\":\"bogus\",\"success\":false,\"error\":\"Unknown command: bogus\"}"
    );
    assert!(lines[2].starts_with(
        "{\"type\":\"response\",\"command\":\"parse\",\"success\":false,\"error\":\"Failed to parse command: "
    ));
}

#[test]
fn export_writes_html_from_session_fixture() {
    let bin = Bin::new();
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session_pi_written.jsonl");
    let session = bin.project_dir.join("session.jsonl");
    std::fs::copy(&fixture, &session).expect("copy fixture");
    let out_path = bin.project_dir.join("export.html");
    let output = bin.run(&[
        "--export",
        session.to_str().unwrap(),
        out_path.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("Exported to: "));
    assert!(out_path.exists());
    let html = std::fs::read_to_string(&out_path).expect("html");
    assert!(html.contains("<html"));
}

#[test]
fn no_session_flag_skips_session_files() {
    let bin = Bin::new();
    let output = bin.run_with_stdin(&["--no-session", "-p"], "hi");
    // No models configured: exits 1, but must not create a session file.
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !bin.agent_dir.join("sessions").exists(),
        "no session dir expected"
    );
}

/// `-ne` suppresses extension auto-discovery only: an explicit `-e <path>`
/// still binds the sidecar (oracle main.ts:665-669). With an unusable Bun
/// the attempted bind surfaces as the degrade warning; without `-e` the
/// same environment stays silent because Bun is never consulted.
#[test]
fn no_extensions_still_binds_explicit_extension_paths() {
    let bin = Bin::new();
    let ext = bin.project_dir.join("explicit-ext.ts");
    std::fs::write(&ext, "export default function () {}").expect("write ext");

    // -ne -e <path>: bind attempted, degrades with the sidecar warning.
    let output = bin
        .command()
        .args([
            "--mode",
            "rpc",
            "-ne",
            "-e",
            ext.to_str().expect("utf8 path"),
        ])
        .env("PI_RUST_BUN", "/nonexistent/bun")
        .env("ANTHROPIC_API_KEY", "test-key")
        .stdin(Stdio::null())
        .output()
        .expect("spawn pi");
    assert_eq!(output.status.code(), Some(0));
    let err = stderr(&output);
    assert!(
        err.contains("extensions disabled"),
        "explicit -e under -ne must attempt the sidecar bind: {err}"
    );

    // -ne alone: no discovery, no bind, no warning.
    let output = bin
        .command()
        .args(["--mode", "rpc", "-ne"])
        .env("PI_RUST_BUN", "/nonexistent/bun")
        .env("ANTHROPIC_API_KEY", "test-key")
        .stdin(Stdio::null())
        .output()
        .expect("spawn pi");
    assert_eq!(output.status.code(), Some(0));
    let err = stderr(&output);
    assert!(
        !err.contains("extensions disabled"),
        "-ne alone must never resolve Bun: {err}"
    );
}

// ============================================================================
// `pi config` (oracle handleConfigCommand, package-manager-cli.ts:553-624)
// ============================================================================

const CONFIG_HELP: &str = "Usage:\n  pi config [-l] [--approve|--no-approve]\n\nOpen the resource configuration TUI to enable or disable package resources.\nWithout -l, starts in global settings (~/.pi/agent/settings.json).\nPress Tab in the TUI to switch between global and project-local modes.\n\nOptions:\n  -l, --local       Edit project overrides (.pi/settings.json)\n  -a, --approve     Trust project-local files for this command with -l\n  -na, --no-approve Ignore project-local files for this command with -l\n";

#[test]
fn config_help_prints_usage_and_exits_zero() {
    let bin = Bin::new();
    for flags in [&["config", "--help"][..], &["config", "-h"][..]] {
        let output = bin.run(flags);
        assert!(output.status.success());
        assert_eq!(stdout(&output), CONFIG_HELP, "{flags:?}");
        assert_eq!(stderr(&output), "", "{flags:?}");
    }
}

#[test]
fn config_rejects_unknown_option_and_argument() {
    let bin = Bin::new();
    let output = bin.run(&["config", "-x"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "Unknown option -x for \"config\".\nUse \"pi --help\" or \"pi config [-l] [--approve|--no-approve]\".\n"
    );
    assert_eq!(stdout(&output), "");

    let output = bin.run(&["config", "bogus"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "Unexpected argument bogus.\nUsage: pi config [-l] [--approve|--no-approve]\n"
    );
    assert_eq!(stdout(&output), "");
}

/// `-l` requires project trust: with trust-requiring project resources,
/// no saved decision, and no TTY the project resolves untrusted; `-na`
/// forces the same refusal.
#[test]
fn config_local_refuses_untrusted_project() {
    let bin = Bin::new();
    std::fs::create_dir_all(bin.project_dir.join(".pi")).expect("project config dir");
    std::fs::write(
        bin.project_dir.join(".pi/settings.json"),
        "{ malformed json",
    )
    .expect("project settings");
    for flags in [&["config", "-l"][..], &["config", "-l", "-na"][..]] {
        let output = bin.run(flags);
        assert_eq!(output.status.code(), Some(1), "{flags:?}");
        assert_eq!(
            stderr(&output),
            "Project is not trusted. Use --approve to modify local resource config.\n",
            "{flags:?}"
        );
        assert_eq!(stdout(&output), "", "{flags:?}");
    }
}

#[test]
fn package_manager_trust_scenarios() {
    // 1. no-TTY untrusted local install refuses and writes nothing
    {
        let bin = Bin::new();
        std::fs::create_dir_all(bin.project_dir.join(".pi")).expect("project config dir");
        std::fs::write(
            bin.project_dir.join(".pi/settings.json"),
            "{ malformed json",
        )
        .expect("project settings");

        let pkg = bin.project_dir.join("local-pkg");
        std::fs::create_dir_all(pkg.join("skills/demo")).expect("pkg dirs");
        std::fs::write(
            pkg.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nbody\n",
        )
        .expect("skill");

        let output = bin.run(&["install", "--local", "./local-pkg"]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            stderr(&output),
            "Project is not trusted. Use --approve to modify local package config.\n"
        );

        let settings = std::fs::read_to_string(bin.project_dir.join(".pi/settings.json"))
            .expect("project settings");
        assert_eq!(settings, "{ malformed json");
    }

    // 2. approved succeeds
    {
        let bin = Bin::new();
        std::fs::create_dir_all(bin.project_dir.join(".pi")).expect("project config dir");
        std::fs::write(bin.project_dir.join(".pi/settings.json"), "{}").expect("project settings");

        let pkg = bin.project_dir.join("local-pkg");
        std::fs::create_dir_all(pkg.join("skills/demo")).expect("pkg dirs");
        std::fs::write(
            pkg.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nbody\n",
        )
        .expect("skill");

        let output = bin.run(&["install", "--local", "--approve", "./local-pkg"]);
        assert!(output.status.success(), "stderr: {}", stderr(&output));

        let settings = std::fs::read_to_string(bin.project_dir.join(".pi/settings.json"))
            .expect("project settings");
        assert!(settings.contains("local-pkg"), "settings: {settings}");
    }

    // 3. saved trust succeeds
    {
        let bin = Bin::new();
        std::fs::create_dir_all(bin.project_dir.join(".pi")).expect("project config dir");
        std::fs::write(bin.project_dir.join(".pi/settings.json"), "{}").expect("project settings");

        let pkg = bin.project_dir.join("local-pkg");
        std::fs::create_dir_all(pkg.join("skills/demo")).expect("pkg dirs");
        std::fs::write(
            pkg.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nbody\n",
        )
        .expect("skill");

        let trust_json = format!(
            "{{\n  \"{}\": true\n}}\n",
            bin.project_dir.canonicalize().unwrap().display()
        );
        std::fs::write(bin.agent_dir.join("trust.json"), trust_json).expect("trust.json");

        let output = bin.run(&["install", "--local", "./local-pkg"]);
        assert!(output.status.success(), "stderr: {}", stderr(&output));

        let settings = std::fs::read_to_string(bin.project_dir.join(".pi/settings.json"))
            .expect("project settings");
        assert!(settings.contains("local-pkg"), "settings: {settings}");
    }

    // 4. explicit no overrides saved yes
    {
        let bin = Bin::new();
        std::fs::create_dir_all(bin.project_dir.join(".pi")).expect("project config dir");
        std::fs::write(bin.project_dir.join(".pi/settings.json"), "{}").expect("project settings");

        let pkg = bin.project_dir.join("local-pkg");
        std::fs::create_dir_all(pkg.join("skills/demo")).expect("pkg dirs");
        std::fs::write(
            pkg.join("skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: d\n---\nbody\n",
        )
        .expect("skill");

        let trust_json = format!(
            "{{\n  \"{}\": true\n}}\n",
            bin.project_dir.canonicalize().unwrap().display()
        );
        std::fs::write(bin.agent_dir.join("trust.json"), trust_json).expect("trust.json");

        let output = bin.run(&["install", "--local", "--no-approve", "./local-pkg"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(
            stderr(&output)
                .contains("Project is not trusted. Use --approve to modify local package config.")
        );

        let settings = std::fs::read_to_string(bin.project_dir.join(".pi/settings.json"))
            .expect("project settings");
        assert!(!settings.contains("local-pkg"));
    }

    // 5. list/read behavior exact oracle
    {
        let bin = Bin::new();
        std::fs::create_dir_all(bin.project_dir.join(".pi")).expect("project config dir");
        std::fs::write(bin.project_dir.join(".pi/settings.json"), "{\n  \"packages\": [\n    {\n      \"source\": \"./project-pkg\",\n      \"scope\": \"project\"\n    }\n  ]\n}").expect("project settings");

        std::fs::write(bin.agent_dir.join("settings.json"), "{\n  \"packages\": [\n    {\n      \"source\": \"./user-pkg\",\n      \"scope\": \"user\"\n    }\n  ]\n}").expect("user settings");

        let output = bin.run(&["list"]);
        assert!(output.status.success());
        let text = stdout(&output);
        assert!(text.contains("User packages:"));
        assert!(text.contains("user-pkg"));
        assert!(!text.contains("Project packages:"));
        assert!(!text.contains("project-pkg"));
    }
}
