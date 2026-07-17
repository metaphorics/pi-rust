use parking_lot::Mutex;
use pi_coding_agent::{
    CommandRunner, DefaultPackageManager, InstallMethod, LatestRelease, PackageCommand,
    PackageScope, PackageSource, ProcessSelfUpdater, SelfUpdater, Settings, SettingsManager,
    UpdateTarget, apply_patterns, detect_install_method, get_package_command_help,
    handle_package_command, handle_package_command_with_self_updater, parse_package_command,
    parse_source,
};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Invocation {
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

#[derive(Clone, Default)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<Invocation>>>,
    capture_result: Arc<Mutex<Option<String>>>,
    capture_error: Arc<Mutex<bool>>,
}

impl CommandRunner for RecordingRunner {
    fn run(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> pi_coding_agent::package_manager::Result<()> {
        self.calls.lock().push(Invocation {
            command: command.into(),
            args: args.to_vec(),
            cwd: cwd.map(Path::to_path_buf),
        });
        if command == "git" && args.first().is_some_and(|arg| arg == "clone") {
            let target = PathBuf::from(args.last().expect("clone target"));
            fs::create_dir_all(&target)?;
        }
        Ok(())
    }

    fn capture(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        _timeout_ms: Option<u64>,
    ) -> pi_coding_agent::package_manager::Result<String> {
        self.calls.lock().push(Invocation {
            command: command.into(),
            args: args.to_vec(),
            cwd: cwd.map(Path::to_path_buf),
        });
        if *self.capture_error.lock() {
            return Err(pi_coding_agent::PackageManagerError::Message(
                "fixture capture failure".into(),
            ));
        }
        if let Some(result) = self.capture_result.lock().clone() {
            return Ok(result);
        }
        Ok(if args.iter().any(|arg| arg.contains("upstream")) {
            "origin/main".into()
        } else if args.iter().any(|arg| arg == "HEAD") {
            "old".into()
        } else {
            "new".into()
        })
    }
}

#[derive(Clone)]
struct LocalGitRunner {
    repository: PathBuf,
}

impl CommandRunner for LocalGitRunner {
    fn run(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> pi_coding_agent::package_manager::Result<()> {
        let mut args = args.to_vec();
        if command == "git" && args.first().is_some_and(|arg| arg == "clone") {
            args[1] = self.repository.to_string_lossy().into_owned();
        }
        pi_coding_agent::ProcessCommandRunner.run(command, &args, cwd)
    }

    fn capture(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> pi_coding_agent::package_manager::Result<String> {
        pi_coding_agent::ProcessCommandRunner.capture(command, args, cwd, timeout_ms)
    }
}

#[derive(Clone)]
struct DependencyGitRunner {
    repository: PathBuf,
    installs: Arc<Mutex<Vec<Vec<String>>>>,
}

impl CommandRunner for DependencyGitRunner {
    fn run(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> pi_coding_agent::package_manager::Result<()> {
        if command == "npm" {
            self.installs.lock().push(args.to_vec());
            fs::create_dir_all(cwd.expect("npm cwd").join("node_modules/dependency"))?;
            return Ok(());
        }
        let mut args = args.to_vec();
        if command == "git" && args.first().is_some_and(|arg| arg == "clone") {
            args[1] = self.repository.to_string_lossy().into_owned();
        }
        pi_coding_agent::ProcessCommandRunner.run(command, &args, cwd)
    }

    fn capture(
        &self,
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        timeout_ms: Option<u64>,
    ) -> pi_coding_agent::package_manager::Result<String> {
        pi_coding_agent::ProcessCommandRunner.capture(command, args, cwd, timeout_ms)
    }
}

fn git(repository: &Path, args: &[&str]) {
    pi_coding_agent::ProcessCommandRunner
        .run(
            "git",
            &args
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>(),
            Some(repository),
        )
        .unwrap();
}

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("project");
    let agent = temp.path().join("agent");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&agent).unwrap();
    (temp, cwd, agent)
}

fn manager(cwd: &Path, agent: &Path) -> DefaultPackageManager<RecordingRunner> {
    DefaultPackageManager::with_runner(
        cwd,
        agent,
        SettingsManager::create(cwd, Some(agent.to_path_buf())),
        RecordingRunner::default(),
    )
}

#[test]
fn parses_npm_git_and_local_sources_with_oracle_identities() {
    assert_eq!(
        parse_source("npm:@scope/tool@1.2.3"),
        PackageSource::Npm {
            spec: "@scope/tool@1.2.3".into(),
            name: "@scope/tool".into(),
            version: Some("1.2.3".into()),
            range: Some("1.2.3".into()),
            pinned: true,
        }
    );
    assert!(matches!(
        parse_source("npm:tool@^2"),
        PackageSource::Npm { pinned: false, .. }
    ));
    assert!(matches!(
        parse_source("npm:tool@1.2.3-beta.1"),
        PackageSource::Npm { pinned: true, range: Some(range), .. } if range == "1.2.3-beta.1"
    ));
    assert!(matches!(
        parse_source("npm:tool@not-a-range"),
        PackageSource::Npm {
            pinned: false,
            range: None,
            ..
        }
    ));
    assert_eq!(
        parse_source("git:git@github.com:user/repo@v1.0.0"),
        PackageSource::Git {
            repo: "git@github.com:user/repo".into(),
            host: "github.com".into(),
            path: "user/repo".into(),
            git_ref: Some("v1.0.0".into()),
            pinned: true,
        }
    );
    assert!(matches!(
        parse_source("https://github.com/user/repo.git"),
        PackageSource::Git { host, path, .. } if host == "github.com" && path == "user/repo"
    ));
    assert!(matches!(
        parse_source("git@github.com:user/repo"),
        PackageSource::Local { .. }
    ));
    assert!(matches!(
        parse_source("git:github.com/user/repo#feature"),
        PackageSource::Git { git_ref: Some(git_ref), pinned: true, .. } if git_ref == "feature"
    ));
    assert!(matches!(
        parse_source("github:user/repo"),
        PackageSource::Local { .. }
    ));
    assert_eq!(
        parse_source("local: ./extension"),
        PackageSource::Local {
            path: "local: ./extension".into()
        }
    );
}

#[test]
fn local_install_persists_relative_to_settings_and_is_idempotent() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("packages/local-package");
    fs::create_dir_all(&package).unwrap();
    let mut manager = manager(&cwd, &agent);

    manager
        .install_and_persist("./packages/local-package", false)
        .unwrap();
    manager
        .install_and_persist("./packages/local-package/", false)
        .unwrap();

    let entries = manager
        .settings_manager()
        .global_settings()
        .get("packages")
        .unwrap()
        .as_array()
        .unwrap();
    assert_eq!(entries.len(), 1);
    let persisted = entries[0].as_str().unwrap();
    assert!(!Path::new(persisted).is_absolute());
    assert_eq!(
        fs::canonicalize(agent.join(persisted)).unwrap(),
        fs::canonicalize(&package).unwrap()
    );
    assert!(
        manager
            .remove_and_persist(&format!("{}/", package.display()), false)
            .unwrap()
    );
    assert!(
        manager
            .settings_manager()
            .global_settings()
            .get("packages")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn source_alias_updates_object_without_losing_filters() {
    let (_temp, cwd, agent) = fixture();
    let mut settings = Settings::new();
    settings.insert(
        "packages",
        json!([{ "source": "git:git@github.com:user/repo", "autoload": false, "extensions": ["+index.ts"] }]),
    );
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );

    assert!(
        manager
            .add_source_to_settings("https://github.com/user/repo", false)
            .unwrap()
    );
    let entry = &manager
        .settings_manager()
        .global_settings()
        .get("packages")
        .unwrap()[0];
    assert_eq!(entry["source"], "https://github.com/user/repo");
    assert_eq!(entry["autoload"], false);
    assert_eq!(entry["extensions"], json!(["+index.ts"]));
}

#[test]
fn discovers_manifest_and_convention_resources_with_filters() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("local-package");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::create_dir_all(package.join("skills/demo")).unwrap();
    fs::write(package.join("src/a.ts"), "export default () => {};").unwrap();
    fs::write(package.join("src/b.ts"), "export default () => {};").unwrap();
    fs::write(
        package.join("skills/demo/SKILL.md"),
        "---\nname: demo\n---\n",
    )
    .unwrap();
    fs::write(
        package.join("package.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "fixture",
            "pi": {
                "extensions": ["src/*.ts", "!src/b.ts"],
                "skills": ["skills"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "packages",
        json!([{
            "source": package.to_string_lossy(),
            "extensions": ["+src/b.ts", "-src/a.ts"]
        }]),
    );
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );

    let resources = manager.resolve().unwrap();
    assert_eq!(resources.extensions.len(), 1);
    assert!(
        resources
            .extensions
            .iter()
            .any(|r| r.path.ends_with("src/a.ts") && !r.enabled)
    );
    assert!(
        !resources
            .extensions
            .iter()
            .any(|r| r.path.ends_with("src/b.ts"))
    );
    assert_eq!(resources.skills.len(), 1);
    assert!(resources.skills[0].enabled);
    assert_eq!(resources.skills[0].metadata.scope, PackageScope::User);
}

#[test]
fn empty_filter_array_disables_all_resources() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("pkg-empty-filter");
    fs::create_dir_all(package.join("extensions")).unwrap();
    fs::write(
        package.join("extensions/foo.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        package.join("extensions/bar.ts"),
        "export default () => {};",
    )
    .unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "packages",
        json!([{
            "source": package.to_string_lossy(),
            "extensions": []
        }]),
    );
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );
    let resources = manager.resolve().unwrap();
    assert_eq!(resources.extensions.len(), 2);
    assert!(resources.extensions.iter().all(|r| !r.enabled));
}

#[test]
fn empty_filter_array_autoload_false_produces_nothing() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("pkg-delta-empty");
    fs::create_dir_all(package.join("extensions")).unwrap();
    fs::write(
        package.join("extensions/foo.ts"),
        "export default () => {};",
    )
    .unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "packages",
        json!([{
            "source": package.to_string_lossy(),
            "autoload": false,
            "extensions": []
        }]),
    );
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );
    let resources = manager.resolve().unwrap();
    assert_eq!(resources.extensions.len(), 0);
}

#[test]
fn root_only_index_with_empty_filter_loads_nothing() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("pkg-root-index-filter");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("index.ts"), "export default () => {};").unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "packages",
        json!([{
            "source": package.to_string_lossy(),
            "extensions": []
        }]),
    );
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );
    let resources = manager.resolve().unwrap();
    assert_eq!(resources.extensions.len(), 0);
}

#[test]
fn layered_manifest_and_user_filters() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("pkg-layered");
    fs::create_dir_all(package.join("extensions")).unwrap();
    fs::write(
        package.join("extensions/foo.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        package.join("extensions/bar.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        package.join("extensions/baz.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        package.join("package.json"),
        serde_json::to_vec_pretty(&json!({
            "name": "layered",
            "pi": {
                "extensions": ["extensions/*.ts", "!extensions/baz.ts"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "packages",
        json!([{
            "source": package.to_string_lossy(),
            "extensions": ["!**/bar.ts"]
        }]),
    );
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );
    let resources = manager.resolve().unwrap();
    // foo.ts: included (not excluded by anyone)
    assert!(
        resources
            .extensions
            .iter()
            .any(|r| r.path.ends_with("foo.ts") && r.enabled)
    );
    // bar.ts: excluded by user filter, present but disabled
    assert!(
        resources
            .extensions
            .iter()
            .any(|r| r.path.ends_with("bar.ts") && !r.enabled)
    );
    // baz.ts: excluded by manifest, completely absent
    assert!(
        !resources
            .extensions
            .iter()
            .any(|r| r.path.ends_with("baz.ts"))
    );
}

#[test]
fn falls_back_to_package_index_extension() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("single-extension");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("index.ts"), "export default () => {};").unwrap();
    let mut settings = Settings::new();
    settings.insert("packages", json!([package.to_string_lossy()]));
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );

    let resources = manager.resolve().unwrap();
    assert_eq!(resources.extensions.len(), 1);
    assert!(resources.extensions[0].path.ends_with("index.ts"));
}

#[test]
fn convention_discovery_only_loads_extension_entries_and_skills() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("convention-package");
    let extensions = package.join("extensions");
    let skills = package.join("skills");
    fs::create_dir_all(extensions.join("with-index")).unwrap();
    fs::create_dir_all(extensions.join("with-manifest/src")).unwrap();
    fs::create_dir_all(extensions.join("helper-directory/nested")).unwrap();
    fs::create_dir_all(extensions.join("ignored-directory")).unwrap();
    fs::create_dir_all(skills.join("nested-skill/deeper")).unwrap();
    fs::create_dir_all(skills.join("container/child-skill")).unwrap();

    fs::write(extensions.join("root.ts"), "export default () => {};").unwrap();
    fs::write(extensions.join("root.js"), "export default () => {};").unwrap();
    fs::write(
        extensions.join(".gitignore"),
        "ignored-root.ts\nignored-directory/\n",
    )
    .unwrap();
    fs::write(
        extensions.join("ignored-root.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        extensions.join("ignored-directory/index.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(extensions.join("root.md"), "not an extension").unwrap();
    fs::write(
        extensions.join("with-index/index.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        extensions.join("with-index/index.js"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        extensions.join("with-manifest/package.json"),
        serde_json::to_vec(&json!({ "pi": { "extensions": ["src/entry.ts"] } })).unwrap(),
    )
    .unwrap();
    fs::write(
        extensions.join("with-manifest/src/entry.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        extensions.join("with-manifest/index.ts"),
        "export default () => {};",
    )
    .unwrap();
    fs::write(
        extensions.join("helper-directory/helper.ts"),
        "export const helper = true;",
    )
    .unwrap();
    fs::write(
        extensions.join("helper-directory/nested/index.ts"),
        "export default () => {};",
    )
    .unwrap();

    fs::write(skills.join("root.md"), "root skill").unwrap();
    fs::write(skills.join("helper.txt"), "not a skill").unwrap();
    fs::write(
        skills.join("nested-skill/SKILL.md"),
        "---\nname: nested\n---\n",
    )
    .unwrap();
    fs::write(
        skills.join("nested-skill/helper.md"),
        "not a separate skill",
    )
    .unwrap();
    fs::write(
        skills.join("nested-skill/deeper/SKILL.md"),
        "---\nname: shadowed\n---\n",
    )
    .unwrap();
    fs::write(
        skills.join("container/child-skill/SKILL.md"),
        "---\nname: child\n---\n",
    )
    .unwrap();

    let mut settings = Settings::new();
    settings.insert("packages", json!([package.to_string_lossy()]));
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );

    let resources = manager.resolve().unwrap();
    let extension_paths = resources
        .extensions
        .iter()
        .map(|resource| {
            resource
                .path
                .strip_prefix(&extensions)
                .unwrap()
                .to_path_buf()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        extension_paths,
        vec![
            PathBuf::from("root.js"),
            PathBuf::from("root.ts"),
            PathBuf::from("with-index/index.ts"),
            PathBuf::from("with-manifest/src/entry.ts"),
        ]
    );
    let skill_paths = resources
        .skills
        .iter()
        .map(|resource| resource.path.strip_prefix(&skills).unwrap().to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(
        skill_paths,
        vec![
            PathBuf::from("container/child-skill/SKILL.md"),
            PathBuf::from("nested-skill/SKILL.md"),
            PathBuf::from("root.md"),
        ]
    );
}

#[test]
fn skill_directory_with_skill_file_is_a_single_entry() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("root-skill-package");
    let skills = package.join("skills");
    fs::create_dir_all(skills.join("nested")).unwrap();
    fs::write(skills.join("SKILL.md"), "---\nname: root\n---\n").unwrap();
    fs::write(skills.join("helper.md"), "not a separate skill").unwrap();
    fs::write(skills.join("nested/SKILL.md"), "---\nname: nested\n---\n").unwrap();

    let mut settings = Settings::new();
    settings.insert("packages", json!([package.to_string_lossy()]));
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );

    let resources = manager.resolve().unwrap();
    assert_eq!(resources.skills.len(), 1);
    assert!(resources.skills[0].path.ends_with("skills/SKILL.md"));
}

#[test]
fn ignored_parent_skill_does_not_suppress_nested_skill() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("ignored-parent-skill-package");
    let skills = package.join("skills");
    fs::create_dir_all(skills.join("parent/nested")).unwrap();
    fs::write(skills.join(".gitignore"), "parent/SKILL.md\n").unwrap();
    fs::write(skills.join("parent/SKILL.md"), "---\nname: ignored\n---\n").unwrap();
    fs::write(
        skills.join("parent/nested/SKILL.md"),
        "---\nname: nested\n---\n",
    )
    .unwrap();

    let mut settings = Settings::new();
    settings.insert("packages", json!([package.to_string_lossy()]));
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );

    let resources = manager.resolve().unwrap();
    assert_eq!(resources.skills.len(), 1);
    assert!(
        resources.skills[0]
            .path
            .ends_with("skills/parent/nested/SKILL.md")
    );
}

#[test]
fn applies_pattern_precedence() {
    let root = PathBuf::from("/fixture");
    let paths = vec![root.join("a.ts"), root.join("b.ts")];
    let enabled = apply_patterns(
        &paths,
        &[
            "*.ts".into(),
            "!b.ts".into(),
            "+b.ts".into(),
            "-a.ts".into(),
        ],
        &root,
    );
    assert_eq!(enabled, [root.join("b.ts")].into_iter().collect());

    let skill = root.join("skills/demo/SKILL.md");
    assert_eq!(
        apply_patterns(std::slice::from_ref(&skill), &["demo".into()], &root),
        [skill.clone()].into_iter().collect()
    );
    assert_eq!(
        apply_patterns(std::slice::from_ref(&skill), &["skills/demo".into()], &root),
        [skill].into_iter().collect()
    );
}

#[test]
fn npm_and_git_use_injected_commands_and_managed_paths() {
    let (_temp, cwd, agent) = fixture();
    let runner = RecordingRunner::default();
    let calls = runner.calls.clone();
    let mut settings = Settings::new();
    settings.insert("npmCommand", json!(["bun"]));
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        runner,
    );

    manager
        .install_and_persist("npm:@scope/tool", false)
        .unwrap();
    manager
        .install_and_persist("git:github.com/user/repo", false)
        .unwrap();

    let calls = calls.lock();
    assert!(calls.iter().any(|call| call.command == "bun"
        && call.args[0] == "install"
        && call.args.contains(&"--omit=peer".into())));
    let clone = calls
        .iter()
        .find(|call| call.command == "git" && call.args.first().is_some_and(|arg| arg == "clone"))
        .unwrap();
    assert!(Path::new(clone.args.last().unwrap()).ends_with("git/github.com/user/repo"));
}

#[test]
fn configured_plain_npm_command_uses_plain_git_dependency_install() {
    let (_temp, cwd, agent) = fixture();
    let runner = RecordingRunner::default();
    let calls = runner.calls.clone();
    let mut settings = Settings::new();
    settings.insert("npmCommand", json!(["npm"]));
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        runner,
    );

    let checkout = agent.join("git/github.com/user/repo");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join("package.json"), "{}").unwrap();
    manager
        .install_and_persist("git:github.com/user/repo", false)
        .unwrap();

    assert!(calls.lock().iter().any(|call| {
        call.command == "npm" && call.args == ["install"] && call.cwd.as_deref() == Some(&checkout)
    }));
}

#[test]
fn cli_parser_preserves_aliases_conflicts_and_help() {
    let options = parse_package_command(&["uninstall", "-l", "./pkg"]).unwrap();
    assert_eq!(options.command, PackageCommand::Remove);
    assert!(options.local);
    assert_eq!(options.source.as_deref(), Some("./pkg"));

    let options = parse_package_command(&["update", "--self", "--extensions"]).unwrap();
    assert_eq!(options.update_target, Some(UpdateTarget::All));

    let options = parse_package_command(&["update", "--all", "--extension", "npm:x"]).unwrap();
    assert_eq!(
        options.conflicting_options.as_deref(),
        Some("--all cannot be combined with --self, --extensions, or --extension")
    );

    let help = get_package_command_help(PackageCommand::Install);
    assert!(help.contains("pi install git:git@github.com:user/repo"));
    assert!(help.ends_with('\n'));
}
/// Oracle handleConfigCommand parsing (package-manager-cli.ts:557-587):
/// help wins over anything else, flags accumulate, the first invalid token
/// stops parsing, non-`config` argv is not a config invocation.
#[test]
fn config_parser_matches_oracle_precedence() {
    use pi_coding_agent::{get_config_command_help, get_config_command_usage, parse_config_command};

    assert!(parse_config_command(&["install", "x"]).is_none());
    assert!(parse_config_command(&[] as &[&str]).is_none());

    let options = parse_config_command(&["config"]).unwrap();
    assert_eq!(options, pi_coding_agent::ConfigCommandOptions::default());

    // Help wins even after an invalid option.
    let options = parse_config_command(&["config", "-bogus", "--help"]).unwrap();
    assert!(options.help);
    assert!(options.invalid_option.is_none());

    let options = parse_config_command(&["config", "-l", "-a"]).unwrap();
    assert!(options.local);
    assert_eq!(options.project_trust_override, Some(true));

    let options = parse_config_command(&["config", "--local", "-na"]).unwrap();
    assert!(options.local);
    assert_eq!(options.project_trust_override, Some(false));

    let options = parse_config_command(&["config", "-x", "extra"]).unwrap();
    assert_eq!(options.invalid_option.as_deref(), Some("-x"));
    assert!(options.invalid_argument.is_none());

    let options = parse_config_command(&["config", "extra", "-x"]).unwrap();
    assert_eq!(options.invalid_argument.as_deref(), Some("extra"));
    assert!(options.invalid_option.is_none());

    assert_eq!(
        get_config_command_usage(),
        "pi config [-l] [--approve|--no-approve]"
    );
    let help = get_config_command_help();
    assert!(help.starts_with("Usage:\n  pi config [-l] [--approve|--no-approve]\n"));
    assert!(help.ends_with("with -l\n"));
}

#[test]
fn binary_independent_install_list_remove_smoke() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("fixture-package");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("index.js"), "export default () => {};").unwrap();
    let mut manager = manager(&cwd, &agent);

    let install = handle_package_command(
        &["install".into(), package.to_string_lossy().into_owned()],
        &mut manager,
    )
    .unwrap();
    assert_eq!(install.exit_code, 0);
    assert_eq!(install.stdout, format!("Installed {}\n", package.display()));

    let list = handle_package_command(&["list".into()], &mut manager).unwrap();
    assert_eq!(list.exit_code, 0);
    assert!(list.stdout.contains("User packages:"));
    assert!(
        list.stdout
            .contains(package.file_name().unwrap().to_str().unwrap())
    );

    let remove = handle_package_command(
        &["remove".into(), format!("{}/", package.display())],
        &mut manager,
    )
    .unwrap();
    assert_eq!(remove.exit_code, 0);
    assert!(
        handle_package_command(&["list".into()], &mut manager)
            .unwrap()
            .stdout
            .contains("No packages installed.")
    );
}

#[test]
fn local_file_and_resource_less_directory_are_extension_sources() {
    let (_temp, cwd, agent) = fixture();
    let file = cwd.join("single.ts");
    let directory = cwd.join("extension-directory");
    fs::write(&file, "export default () => {};").unwrap();
    fs::create_dir_all(&directory).unwrap();
    let mut settings = Settings::new();
    settings.insert(
        "packages",
        json!([file.to_string_lossy(), directory.to_string_lossy()]),
    );
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        RecordingRunner::default(),
    );

    let resources = manager.resolve().unwrap();
    let file = fs::canonicalize(file).unwrap();
    let directory = fs::canonicalize(directory).unwrap();
    assert!(
        resources
            .extensions
            .iter()
            .any(|resource| resource.path == file)
    );
    assert!(
        resources
            .extensions
            .iter()
            .any(|resource| resource.path == directory)
    );
}

#[test]
fn file_url_is_percent_decoded_for_install_and_identity() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("package with spaces");
    fs::create_dir_all(&package).unwrap();
    let file_url = url::Url::from_file_path(&package).unwrap().to_string();
    let mut manager = manager(&cwd, &agent);

    manager.install_and_persist(&file_url, false).unwrap();
    assert!(
        manager
            .remove_and_persist(package.to_str().unwrap(), false)
            .unwrap()
    );
}

#[test]
fn npm_update_skips_installed_current_version() {
    let (_temp, cwd, agent) = fixture();
    let installed = agent.join("npm/node_modules/tool");
    fs::create_dir_all(&installed).unwrap();
    fs::write(installed.join("package.json"), r#"{"version":"1.2.3"}"#).unwrap();
    let mut settings = Settings::new();
    settings.insert("packages", json!(["npm:tool"]));
    let runner = RecordingRunner::default();
    *runner.capture_result.lock() = Some(r#""1.2.3""#.into());
    let calls = runner.calls.clone();
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(settings, true),
        runner,
    );

    manager.update(Some("npm:tool")).unwrap();
    let calls = calls.lock();
    assert!(
        calls
            .iter()
            .any(|call| call.args.starts_with(&["view".into(), "tool".into()]))
    );
    assert!(
        !calls
            .iter()
            .any(|call| call.args.first().is_some_and(|arg| arg == "install"))
    );
}

#[test]
fn process_capture_enforces_timeout() {
    let runner = pi_coding_agent::ProcessCommandRunner;
    let started = std::time::Instant::now();
    let error = runner
        .capture("sh", &["-c".into(), "sleep 1".into()], None, Some(25))
        .unwrap_err();
    assert!(error.to_string().contains("timed out after 25ms"));
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
}

#[test]
fn package_command_applies_project_trust_overrides() {
    let (_temp, cwd, agent) = fixture();
    let package = cwd.join("fixture");
    fs::create_dir_all(&package).unwrap();
    let mut approved = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::in_memory(Settings::new(), false),
        RecordingRunner::default(),
    );
    let output = handle_package_command(
        &[
            "install".into(),
            "--local".into(),
            "--approve".into(),
            package.to_string_lossy().into_owned(),
        ],
        &mut approved,
    )
    .unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(approved.settings_manager().is_project_trusted());

    let mut rejected = manager(&cwd, &agent);
    let output = handle_package_command(
        &[
            "install".into(),
            "--local".into(),
            "--no-approve".into(),
            package.to_string_lossy().into_owned(),
        ],
        &mut rejected,
    )
    .unwrap();
    assert_eq!(output.exit_code, 1);
    assert_eq!(
        output.stderr,
        "Project is not trusted. Use --approve to modify local package config.\n"
    );
}

#[test]
fn remove_not_found_error_has_no_generic_prefix() {
    let (_temp, cwd, agent) = fixture();
    let mut manager = manager(&cwd, &agent);
    let output =
        handle_package_command(&["remove".into(), "npm:missing".into()], &mut manager).unwrap();
    assert_eq!(output.exit_code, 1);
    assert_eq!(output.stderr, "No matching package found for npm:missing\n");
}

#[test]
fn self_update_unavailable_uses_oracle_error_shape() {
    let (_temp, cwd, agent) = fixture();
    let mut manager = manager(&cwd, &agent);
    let updater = ProcessSelfUpdater::with_runner_and_release(
        RecordingRunner::default(),
        "/opt/pi/bin/pi",
        "0.1.0",
        LatestRelease {
            version: "0.2.0".into(),
            package_name: None,
            note: None,
        },
    );
    let output = handle_package_command_with_self_updater(
        &["update".into(), "--self".into()],
        &mut manager,
        &updater,
    )
    .unwrap();
    assert_eq!(output.exit_code, 1);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.starts_with(
        "error: pi cannot self-update this installation.\n\
Update @earendil-works/pi-coding-agent@0.2.0 using the package manager, wrapper, or source checkout that provides this installation.\n"
    ));
    assert!(
        output
            .stderr
            .contains("\nLocation of pi executable: /opt/pi/bin/pi")
    );

    let output =
        handle_package_command_with_self_updater(&["update".into()], &mut manager, &updater)
            .unwrap();
    assert_eq!(
        output.stdout,
        "Extensions are skipped. Run pi update --extensions to update extensions.\n"
    );
}

#[test]
fn managed_self_update_plans_and_executes_oracle_npm_steps() {
    let (_temp, cwd, agent) = fixture();
    let global_root = cwd.join("global/node_modules");
    let package_dir = global_root.join("@earendil-works/pi-coding-agent");
    let entrypoint = package_dir.join("dist/cli.js");
    fs::create_dir_all(entrypoint.parent().unwrap()).unwrap();
    fs::write(package_dir.join("package.json"), "{}").unwrap();
    let runner = RecordingRunner::default();
    *runner.capture_result.lock() = Some(global_root.to_string_lossy().into_owned());
    let calls = runner.calls.clone();
    let updater = ProcessSelfUpdater::with_runner_and_release(
        runner,
        &entrypoint,
        "1.0.0",
        LatestRelease {
            version: "1.1.0".into(),
            package_name: Some("@earendil-works/pi-coding-agent-next".into()),
            note: Some("Release note".into()),
        },
    );
    let mut manager = manager(&cwd, &agent);
    let output = handle_package_command_with_self_updater(
        &["update".into(), "--self".into()],
        &mut manager,
        &updater,
    )
    .unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.starts_with(
        "Release note\nUpdating pi with npm install -g --ignore-scripts --min-release-age=0"
    ));
    assert!(output.stdout.ends_with("Updated pi from 1.0.0 to 1.1.0\n"));
    let calls = calls.lock();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].args, ["root", "-g"].map(str::to_string));
    assert_eq!(
        calls[1].args,
        [
            "install",
            "-g",
            "--ignore-scripts",
            "--min-release-age=0",
            "@earendil-works/pi-coding-agent-next@1.1.0",
        ]
        .map(str::to_string)
    );
    assert_eq!(
        calls[2].args,
        ["uninstall", "-g", "@earendil-works/pi-coding-agent"].map(str::to_string)
    );
}

#[test]
fn managed_self_update_noops_when_current() {
    let (_temp, cwd, agent) = fixture();
    let updater = ProcessSelfUpdater::with_runner_and_release(
        RecordingRunner::default(),
        "/usr/lib/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
        "1.0.0",
        LatestRelease {
            version: "1.0.0".into(),
            package_name: None,
            note: None,
        },
    );
    let mut manager = manager(&cwd, &agent);
    let output = handle_package_command_with_self_updater(
        &["update".into(), "--self".into()],
        &mut manager,
        &updater,
    )
    .unwrap();
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "pi is already up to date (v1.0.0)\n");
}

#[test]
fn managed_self_update_uses_pnpm_and_bun_command_tables() {
    let temp = tempfile::tempdir().unwrap();
    let cases = [
        (
            InstallMethod::Pnpm,
            temp.path().join("pnpm/.pnpm/node_modules"),
            temp.path()
                .join("pnpm/.pnpm/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"),
            temp.path().join("pnpm/.pnpm/node_modules"),
            vec![
                "install",
                "-g",
                "--ignore-scripts",
                "--config.minimumReleaseAge=0",
                "@earendil-works/pi-coding-agent@2.0.0",
            ],
        ),
        (
            InstallMethod::Bun,
            temp.path().join("bun/install/global/node_modules"),
            temp.path().join(
                "bun/install/global/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
            ),
            temp.path().join("bun/bin"),
            vec![
                "install",
                "-g",
                "--ignore-scripts",
                "--minimum-release-age=0",
                "@earendil-works/pi-coding-agent@2.0.0",
            ],
        ),
    ];
    for (method, root, entrypoint, captured_root, expected_args) in cases {
        let package_dir = entrypoint.parent().unwrap().parent().unwrap();
        fs::create_dir_all(entrypoint.parent().unwrap()).unwrap();
        fs::write(package_dir.join("package.json"), "{}").unwrap();
        assert_eq!(detect_install_method(&entrypoint), method);
        let runner = RecordingRunner::default();
        *runner.capture_result.lock() = Some(captured_root.to_string_lossy().into_owned());
        let calls = runner.calls.clone();
        let updater = ProcessSelfUpdater::with_runner_and_release(
            runner,
            &entrypoint,
            "1.0.0",
            LatestRelease {
                version: "2.0.0".into(),
                package_name: None,
                note: None,
            },
        );
        let outcome = updater.update(false, None).unwrap();
        assert!(matches!(
            outcome,
            pi_coding_agent::SelfUpdateOutcome::Updated { .. }
        ));
        let calls = calls.lock();
        let install = calls
            .iter()
            .rev()
            .find(|call| call.args.first().is_some_and(|arg| arg == "install"))
            .unwrap();
        assert_eq!(
            install.args,
            expected_args
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert!(entrypoint.starts_with(root));
    }
}

#[test]
fn pnpm_root_failure_infers_global_bin_dir() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("pnpm/global/5");
    let entrypoint = root.join(
        ".pnpm/@earendil-works+pi-coding-agent/node_modules/@earendil-works/pi-coding-agent/dist/cli.js",
    );
    let package_dir = entrypoint.parent().unwrap().parent().unwrap();
    fs::create_dir_all(entrypoint.parent().unwrap()).unwrap();
    fs::write(package_dir.join("package.json"), "{}").unwrap();
    let runner = RecordingRunner::default();
    *runner.capture_error.lock() = true;
    let calls = runner.calls.clone();
    let updater = ProcessSelfUpdater::with_runner_and_release(
        runner,
        &entrypoint,
        "1.0.0",
        LatestRelease {
            version: "2.0.0".into(),
            package_name: None,
            note: None,
        },
    );

    let outcome = updater.update(false, None).unwrap();
    assert!(matches!(
        outcome,
        pi_coding_agent::SelfUpdateOutcome::Updated { .. }
    ));
    let calls = calls.lock();
    let install = calls
        .iter()
        .find(|call| call.args.first().is_some_and(|arg| arg == "install"))
        .unwrap();
    assert_eq!(
        install.args,
        [
            "install".to_string(),
            "-g".to_string(),
            "--ignore-scripts".to_string(),
            "--config.minimumReleaseAge=0".to_string(),
            format!(
                "--config.global-bin-dir={}",
                temp.path().join("pnpm").display()
            ),
            "@earendil-works/pi-coding-agent@2.0.0".to_string(),
        ]
    );
}

#[test]
fn local_git_fixture_install_update_and_remove_smoke() {
    let (_temp, cwd, agent) = fixture();
    let repository = cwd.join("fixture-repository");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init"]);
    git(
        &repository,
        &["config", "user.email", "pi-rust@example.invalid"],
    );
    git(&repository, &["config", "user.name", "pi-rust test"]);
    fs::write(repository.join("index.ts"), "export const version = 1;\n").unwrap();
    git(&repository, &["add", "index.ts"]);
    git(&repository, &["commit", "-m", "v1"]);
    git(&repository, &["tag", "v1"]);
    fs::write(repository.join("index.ts"), "export const version = 2;\n").unwrap();
    git(&repository, &["commit", "-am", "v2"]);
    git(&repository, &["tag", "v2"]);

    let runner = LocalGitRunner {
        repository: repository.clone(),
    };
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::create(&cwd, Some(agent.clone())),
        runner,
    );
    let v1 = "git:localhost/user/repo@v1";
    let v2 = "git:localhost/user/repo@v2";
    manager.install_and_persist(v1, false).unwrap();
    let checkout = agent.join("git/localhost/user/repo");
    let head = pi_coding_agent::ProcessCommandRunner
        .capture(
            "git",
            &["describe".into(), "--tags".into(), "--exact-match".into()],
            Some(&checkout),
            Some(1_000),
        )
        .unwrap();
    assert_eq!(head, "v1");

    assert!(manager.add_source_to_settings(v2, false).unwrap());
    manager.update(Some(v2)).unwrap();
    let head = pi_coding_agent::ProcessCommandRunner
        .capture(
            "git",
            &["describe".into(), "--tags".into(), "--exact-match".into()],
            Some(&checkout),
            Some(1_000),
        )
        .unwrap();
    assert_eq!(head, "v2");
    assert!(manager.remove_and_persist(v2, false).unwrap());
    assert!(!checkout.exists());
}

#[test]
fn git_update_reinstalls_dependencies_after_clean() {
    let (_temp, cwd, agent) = fixture();
    let repository = cwd.join("dependency-repository");
    fs::create_dir_all(&repository).unwrap();
    git(&repository, &["init"]);
    git(
        &repository,
        &["config", "user.email", "pi-rust@example.invalid"],
    );
    git(&repository, &["config", "user.name", "pi-rust test"]);
    fs::write(repository.join("package.json"), "{\"version\":1}\n").unwrap();
    git(&repository, &["add", "package.json"]);
    git(&repository, &["commit", "-m", "v1"]);
    git(&repository, &["tag", "v1"]);
    fs::write(repository.join("package.json"), "{\"version\":2}\n").unwrap();
    git(&repository, &["commit", "-am", "v2"]);
    git(&repository, &["tag", "v2"]);

    let installs = Arc::new(Mutex::new(Vec::new()));
    let runner = DependencyGitRunner {
        repository,
        installs: installs.clone(),
    };
    let mut manager = DefaultPackageManager::with_runner(
        &cwd,
        &agent,
        SettingsManager::create(&cwd, Some(agent.clone())),
        runner,
    );
    let v1 = "git:localhost/user/dependency@v1";
    let v2 = "git:localhost/user/dependency@v2";
    manager.install_and_persist(v1, false).unwrap();
    let checkout = agent.join("git/localhost/user/dependency");
    let dependency = checkout.join("node_modules/dependency");
    assert!(dependency.is_dir());
    fs::write(checkout.join("untracked"), "removed by clean").unwrap();

    assert!(manager.add_source_to_settings(v2, false).unwrap());
    manager.update(Some(v2)).unwrap();

    assert!(!checkout.join("untracked").exists());
    assert!(dependency.is_dir());
    assert_eq!(
        installs.lock().as_slice(),
        [
            vec!["install".to_string(), "--omit=dev".to_string()],
            vec!["install".to_string(), "--omit=dev".to_string()],
        ]
    );
}
