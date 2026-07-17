//! Package-manager subcommand parsing and binary-independent execution.
//!
//! The binary calls this before general argument parsing, matching pi's boot
//! order. Returning a value instead of writing stdout makes the command path
//! deterministic and keeps JSON/RPC stdout ownership separate.

use crate::config::{APP_NAME, CONFIG_DIR_NAME, PACKAGE_NAME};
use crate::package_manager::{
    CommandRunner, DefaultPackageManager, PackageManagerError, ProcessCommandRunner,
};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageCommand {
    Install,
    Remove,
    Update,
    List,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateTarget {
    All,
    SelfOnly,
    Extensions(Option<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageCommandOptions {
    pub command: PackageCommand,
    pub source: Option<String>,
    pub update_target: Option<UpdateTarget>,
    pub show_extensions_skipped_note: bool,
    pub local: bool,
    pub force: bool,
    pub project_trust_override: Option<bool>,
    pub help: bool,
    pub invalid_option: Option<String>,
    pub invalid_argument: Option<String>,
    pub missing_option_value: Option<String>,
    pub conflicting_options: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMethod {
    Npm,
    Pnpm,
    Bun,
    BunBinary,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelfUpdateOutcome {
    UpToDate {
        version: String,
    },
    Updated {
        from: String,
        to: String,
        display: String,
        note: Option<String>,
    },
    Unavailable {
        instruction: String,
        entrypoint: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestRelease {
    pub version: String,
    pub package_name: Option<String>,
    pub note: Option<String>,
}

pub trait SelfUpdater {
    fn update(
        &self,
        force: bool,
        npm_command: Option<&[String]>,
    ) -> Result<SelfUpdateOutcome, PackageManagerError>;
}

pub struct ProcessSelfUpdater<R = ProcessCommandRunner> {
    runner: R,
    entrypoint: PathBuf,
    current_version: String,
    release_override: Option<LatestRelease>,
}

impl Default for ProcessSelfUpdater<ProcessCommandRunner> {
    fn default() -> Self {
        Self {
            runner: ProcessCommandRunner,
            entrypoint: std::env::current_exe().unwrap_or_default(),
            current_version: crate::config::VERSION.to_string(),
            release_override: None,
        }
    }
}

impl<R: CommandRunner> ProcessSelfUpdater<R> {
    pub fn with_runner(
        runner: R,
        entrypoint: impl Into<PathBuf>,
        current_version: impl Into<String>,
    ) -> Self {
        Self {
            runner,
            entrypoint: entrypoint.into(),
            current_version: current_version.into(),
            release_override: None,
        }
    }

    pub fn with_runner_and_release(
        runner: R,
        entrypoint: impl Into<PathBuf>,
        current_version: impl Into<String>,
        release: LatestRelease,
    ) -> Self {
        Self {
            runner,
            entrypoint: entrypoint.into(),
            current_version: current_version.into(),
            release_override: Some(release),
        }
    }

    fn latest_release(&self) -> Result<LatestRelease, PackageManagerError> {
        if let Some(release) = &self.release_override {
            return Ok(release.clone());
        }
        if std::env::var_os("PI_SKIP_VERSION_CHECK").is_some()
            || std::env::var_os("PI_OFFLINE").is_some()
        {
            return Err(PackageManagerError::Message(format!(
                "Could not determine latest {APP_NAME} version."
            )));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| PackageManagerError::Message(error.to_string()))?;
        runtime.block_on(async {
            let response = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|error| PackageManagerError::Message(error.to_string()))?
                .get("https://pi.dev/api/latest-version")
                .header("User-Agent", format!("pi/{}", self.current_version))
                .header("accept", "application/json")
                .send()
                .await
                .map_err(|error| {
                    PackageManagerError::Message(format!(
                        "Could not determine latest {APP_NAME} version: {error}"
                    ))
                })?;
            if !response.status().is_success() {
                return Err(PackageManagerError::Message(format!(
                    "Could not determine latest {APP_NAME} version."
                )));
            }
            let value: serde_json::Value = response.json().await.map_err(|error| {
                PackageManagerError::Message(format!(
                    "Could not determine latest {APP_NAME} version: {error}"
                ))
            })?;
            let version = value
                .get("version")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .ok_or_else(|| {
                    PackageManagerError::Message(format!(
                        "Could not determine latest {APP_NAME} version."
                    ))
                })?
                .to_string();
            Ok(LatestRelease {
                version,
                package_name: value
                    .get("packageName")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
                note: value
                    .get("note")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|note| !note.is_empty())
                    .map(str::to_string),
            })
        })
    }

    fn global_package_roots(
        &self,
        method: InstallMethod,
        npm_command: Option<&[String]>,
    ) -> Vec<PathBuf> {
        let capture = |command: &str, args: Vec<String>| {
            self.runner
                .capture(command, &args, None, Some(10_000))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| PathBuf::from(value.trim()))
        };
        match method {
            InstallMethod::Npm => {
                let (command, mut args) = npm_command
                    .and_then(|parts| parts.split_first())
                    .map_or_else(
                        || ("npm", Vec::new()),
                        |(command, args)| (command.as_str(), args.to_vec()),
                    );
                if Path::new(command)
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    == Some("bun")
                {
                    args.extend(["pm".to_string(), "bin".to_string(), "-g".to_string()]);
                    return capture(command, args)
                        .and_then(|bin| bin.parent().map(Path::to_path_buf))
                        .map(|parent| parent.join("install/global/node_modules"))
                        .into_iter()
                        .collect();
                }
                args.extend(["root".to_string(), "-g".to_string()]);
                capture(command, args).into_iter().collect()
            }
            InstallMethod::Pnpm => capture("pnpm", vec!["root".into(), "-g".into()])
                .or_else(|| self.inferred_pnpm_global().map(|(root, _)| root))
                .into_iter()
                .collect(),
            InstallMethod::Bun => capture("bun", vec!["pm".into(), "bin".into(), "-g".into()])
                .and_then(|bin| bin.parent().map(Path::to_path_buf))
                .map(|parent| parent.join("install/global/node_modules"))
                .into_iter()
                .collect(),
            InstallMethod::BunBinary | InstallMethod::Unknown => Vec::new(),
        }
    }

    fn inferred_pnpm_global(&self) -> Option<(PathBuf, PathBuf)> {
        let pnpm_store = self
            .entrypoint
            .ancestors()
            .find(|path| path.file_name() == Some(std::ffi::OsStr::new(".pnpm")))?;
        let root = pnpm_store.parent()?.to_path_buf();
        if root.parent()?.file_name() != Some(std::ffi::OsStr::new("global")) {
            return None;
        }
        let bin_dir = std::env::var_os("PNPM_HOME")
            .map(PathBuf::from)
            .or_else(|| root.parent()?.parent().map(Path::to_path_buf))?;
        Some((root, bin_dir))
    }

    fn managed_install_status(
        &self,
        method: InstallMethod,
        npm_command: Option<&[String]>,
    ) -> ManagedInstallStatus {
        let entrypoint =
            std::fs::canonicalize(&self.entrypoint).unwrap_or_else(|_| self.entrypoint.clone());
        let managed = self
            .global_package_roots(method, npm_command)
            .into_iter()
            .any(|root| {
                let root = std::fs::canonicalize(&root).unwrap_or(root);
                entrypoint.starts_with(root)
            });
        if !managed {
            return ManagedInstallStatus::Unmanaged;
        }
        let package_dir = entrypoint
            .ancestors()
            .find(|path| path.join("package.json").is_file())
            .or_else(|| entrypoint.parent());
        if package_dir.is_some_and(|path| {
            std::fs::metadata(path).is_ok_and(|metadata| !metadata.permissions().readonly())
        }) {
            ManagedInstallStatus::Writable
        } else {
            ManagedInstallStatus::ReadOnly
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedInstallStatus {
    Writable,
    Unmanaged,
    ReadOnly,
}

impl<R: CommandRunner> SelfUpdater for ProcessSelfUpdater<R> {
    fn update(
        &self,
        force: bool,
        npm_command: Option<&[String]>,
    ) -> Result<SelfUpdateOutcome, PackageManagerError> {
        let release = self.latest_release()?;
        let package_name = release
            .package_name
            .clone()
            .unwrap_or_else(|| PACKAGE_NAME.to_string());
        let should_run = force
            || package_name != PACKAGE_NAME
            || match (
                semver::Version::parse(&release.version),
                semver::Version::parse(&self.current_version),
            ) {
                (Ok(latest), Ok(current)) => latest > current,
                _ => release.version != self.current_version,
            };
        if !should_run {
            return Ok(SelfUpdateOutcome::UpToDate {
                version: self.current_version.clone(),
            });
        }

        let install_spec = format!("{package_name}@{}", release.version);
        let method = detect_install_method(&self.entrypoint);
        if matches!(
            method,
            InstallMethod::Npm | InstallMethod::Pnpm | InstallMethod::Bun
        ) {
            match self.managed_install_status(method, npm_command) {
                ManagedInstallStatus::Writable => {}
                ManagedInstallStatus::Unmanaged => {
                    return Ok(SelfUpdateOutcome::Unavailable {
                        instruction: format!(
                            "This installation is not managed by a global {} install. Update it with the package manager, wrapper, or source checkout that provides it.",
                            install_method_name(method)
                        ),
                        entrypoint: Some(self.entrypoint.clone()),
                    });
                }
                ManagedInstallStatus::ReadOnly => {
                    return Ok(SelfUpdateOutcome::Unavailable {
                        instruction: format!(
                            "This installation is managed by a global {} install, but the install path is not writable. Update it yourself with: {}",
                            install_method_name(method),
                            install_spec
                        ),
                        entrypoint: Some(self.entrypoint.clone()),
                    });
                }
            }
        }
        let configured = npm_command.and_then(|parts| parts.split_first());
        let (command, prefix, install_flags, uninstall_verb, global_args) = match method {
            InstallMethod::Npm => {
                let (command, prefix) = configured.map_or_else(
                    || ("npm".to_string(), Vec::new()),
                    |(command, args)| (command.clone(), args.to_vec()),
                );
                (
                    command,
                    prefix,
                    ["install", "-g", "--ignore-scripts", "--min-release-age=0"]
                        .map(str::to_string)
                        .to_vec(),
                    "uninstall",
                    Vec::new(),
                )
            }
            InstallMethod::Pnpm => {
                let root_available = self
                    .runner
                    .capture("pnpm", &["root".into(), "-g".into()], None, Some(10_000))
                    .is_ok_and(|root| !root.trim().is_empty());
                let global_args = if root_available {
                    Vec::new()
                } else {
                    self.inferred_pnpm_global()
                        .map(|(_, bin_dir)| {
                            vec![format!("--config.global-bin-dir={}", bin_dir.display())]
                        })
                        .unwrap_or_default()
                };
                let mut install_flags = [
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "--config.minimumReleaseAge=0",
                ]
                .map(str::to_string)
                .to_vec();
                install_flags.extend(global_args.clone());
                (
                    "pnpm".to_string(),
                    Vec::new(),
                    install_flags,
                    "remove",
                    global_args,
                )
            }
            InstallMethod::Bun => (
                "bun".to_string(),
                Vec::new(),
                [
                    "install",
                    "-g",
                    "--ignore-scripts",
                    "--minimum-release-age=0",
                ]
                .map(str::to_string)
                .to_vec(),
                "uninstall",
                Vec::new(),
            ),
            InstallMethod::BunBinary | InstallMethod::Unknown => {
                return Ok(SelfUpdateOutcome::Unavailable {
                    instruction: if method == InstallMethod::BunBinary {
                        "Download from: https://github.com/earendil-works/pi-mono/releases/latest"
                            .to_string()
                    } else {
                        format!(
                            "Update {install_spec} using the package manager, wrapper, or source checkout that provides this installation."
                        )
                    },
                    entrypoint: (!self.entrypoint.as_os_str().is_empty())
                        .then(|| self.entrypoint.clone()),
                });
            }
        };
        let mut install_args = prefix.clone();
        install_args.extend(install_flags);
        install_args.push(install_spec);
        let mut displays = vec![format_display(&command, &install_args)];
        self.runner.run(&command, &install_args, None)?;
        if package_name != PACKAGE_NAME {
            let mut uninstall_args = prefix;
            uninstall_args.extend([uninstall_verb.to_string(), "-g".to_string()]);
            uninstall_args.extend(global_args);
            uninstall_args.push(PACKAGE_NAME.to_string());
            displays.push(format_display(&command, &uninstall_args));
            self.runner.run(&command, &uninstall_args, None)?;
        }
        Ok(SelfUpdateOutcome::Updated {
            from: self.current_version.clone(),
            to: release.version,
            display: displays.join(" && "),
            note: release.note,
        })
    }
}

fn format_display(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(|arg| {
            if arg.chars().any(char::is_whitespace) {
                format!("\"{arg}\"")
            } else {
                arg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn detect_install_method(entrypoint: &Path) -> InstallMethod {
    let normalized = entrypoint
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    if normalized.contains("/.bun/bin/") && !normalized.contains("node_modules") {
        return InstallMethod::BunBinary;
    }
    if normalized.contains("/.pnpm/") || normalized.contains("/pnpm/") {
        return InstallMethod::Pnpm;
    }
    if normalized.contains("/install/global/node_modules/") {
        return InstallMethod::Bun;
    }
    if normalized.contains("/node_modules/") {
        return InstallMethod::Npm;
    }
    InstallMethod::Unknown
}

impl PackageCommandOutput {
    fn success(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            exit_code: 0,
        }
    }

    fn failure(stderr: String) -> Self {
        Self {
            stdout: String::new(),
            stderr,
            exit_code: 1,
        }
    }
}

pub fn get_package_command_usage(command: PackageCommand) -> String {
    match command {
        PackageCommand::Install => {
            format!("{APP_NAME} install <source> [-l] [--approve|--no-approve]")
        }
        PackageCommand::Remove => {
            format!("{APP_NAME} remove <source> [-l] [--approve|--no-approve]")
        }
        PackageCommand::Update => format!(
            "{APP_NAME} update [source|self|pi] [--self|--extensions|--all] [--extension <source>] [--approve|--no-approve] [--force]"
        ),
        PackageCommand::List => format!("{APP_NAME} list [--approve|--no-approve]"),
    }
}

pub fn get_package_command_help(command: PackageCommand) -> String {
    match command {
        PackageCommand::Install => format!(
            "Usage:\n  {}\n\nInstall a package and add it to settings.\n\nOptions:\n  -l, --local       Install project-locally ({CONFIG_DIR_NAME}/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  {APP_NAME} install npm:@foo/bar\n  {APP_NAME} install git:github.com/user/repo\n  {APP_NAME} install git:git@github.com:user/repo\n  {APP_NAME} install https://github.com/user/repo\n  {APP_NAME} install ssh://git@github.com/user/repo\n  {APP_NAME} install ./local/path\n",
            get_package_command_usage(command)
        ),
        PackageCommand::Remove => format!(
            "Usage:\n  {}\n\nRemove a package and its source from settings.\nAlias: {APP_NAME} uninstall <source> [-l]\n\nOptions:\n  -l, --local       Remove from project settings ({CONFIG_DIR_NAME}/settings.json)\n  -a, --approve     Trust project-local files for this command\n  -na, --no-approve Ignore project-local files for this command\n\nExamples:\n  {APP_NAME} remove npm:@foo/bar\n  {APP_NAME} uninstall npm:@foo/bar\n",
            get_package_command_usage(command)
        ),
        PackageCommand::Update => format!(
            "Usage:\n  {}\n\nUpdate pi and installed packages.\n\nOptions:\n  --self                  Update pi only (default when no target is given)\n  --extensions            Update installed packages only\n  --all                   Update pi and installed packages\n  --extension <source>    Update one package only\n  -a, --approve           Trust project-local files for this command\n  -na, --no-approve       Ignore project-local files for this command\n  --force                 Reinstall pi even if the current version is latest\n\nShort forms:\n  {APP_NAME} update                Update pi only\n  {APP_NAME} update --all          Update pi and all extensions\n  {APP_NAME} update <source>       Update one package\n  {APP_NAME} update pi             Update pi only (self works as alias to pi)\n",
            get_package_command_usage(command)
        ),
        PackageCommand::List => format!(
            "Usage:\n  {}\n\nList installed packages from user and project settings.\n\nOptions:\n  -a, --approve      Trust project-local files for this command\n  -na, --no-approve  Ignore project-local files for this command\n",
            get_package_command_usage(command)
        ),
    }
}

pub fn parse_package_command<S: AsRef<str>>(args: &[S]) -> Option<PackageCommandOptions> {
    let raw = args.first()?.as_ref();
    let command = match raw {
        "install" => PackageCommand::Install,
        "remove" | "uninstall" => PackageCommand::Remove,
        "update" => PackageCommand::Update,
        "list" => PackageCommand::List,
        _ => return None,
    };
    let mut options = PackageCommandOptions {
        command,
        source: None,
        update_target: None,
        show_extensions_skipped_note: false,
        local: false,
        force: false,
        project_trust_override: None,
        help: false,
        invalid_option: None,
        invalid_argument: None,
        missing_option_value: None,
        conflicting_options: None,
    };
    let mut self_flag = false;
    let mut extensions_flag = false;
    let mut all_flag = false;
    let mut extension_flag_source = None;
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].as_ref();
        match arg {
            "-h" | "--help" => options.help = true,
            "-l" | "--local" => {
                if matches!(command, PackageCommand::Install | PackageCommand::Remove) {
                    options.local = true;
                } else {
                    options.invalid_option.get_or_insert_with(|| arg.into());
                }
            }
            "--self" => {
                if command == PackageCommand::Update {
                    self_flag = true;
                } else {
                    options.invalid_option.get_or_insert_with(|| arg.into());
                }
            }
            "--extensions" => {
                if command == PackageCommand::Update {
                    extensions_flag = true;
                } else {
                    options.invalid_option.get_or_insert_with(|| arg.into());
                }
            }
            "--all" => {
                if command == PackageCommand::Update {
                    all_flag = true;
                } else {
                    options.invalid_option.get_or_insert_with(|| arg.into());
                }
            }
            "-a" | "--approve" => options.project_trust_override = Some(true),
            "-na" | "--no-approve" => options.project_trust_override = Some(false),
            "--force" => {
                if command == PackageCommand::Update {
                    options.force = true;
                } else {
                    options.invalid_option.get_or_insert_with(|| arg.into());
                }
            }
            "--extension" => {
                if command != PackageCommand::Update {
                    options.invalid_option.get_or_insert_with(|| arg.into());
                } else if index + 1 >= args.len() || args[index + 1].as_ref().starts_with('-') {
                    options
                        .missing_option_value
                        .get_or_insert_with(|| arg.into());
                } else {
                    let value = args[index + 1].as_ref().to_string();
                    if extension_flag_source.is_some() {
                        options
                            .conflicting_options
                            .get_or_insert_with(|| "--extension can only be provided once".into());
                    } else {
                        extension_flag_source = Some(value);
                    }
                    index += 1;
                }
            }
            _ if arg.starts_with('-') => {
                options.invalid_option.get_or_insert_with(|| arg.into());
            }
            _ if options.source.is_none() => options.source = Some(arg.into()),
            _ => {
                options.invalid_argument.get_or_insert_with(|| arg.into());
            }
        }
        index += 1;
    }

    if command == PackageCommand::Update {
        if all_flag && (self_flag || extensions_flag || extension_flag_source.is_some()) {
            options.conflicting_options.get_or_insert_with(|| {
                "--all cannot be combined with --self, --extensions, or --extension".into()
            });
        }
        if all_flag && options.source.is_some() {
            options
                .conflicting_options
                .get_or_insert_with(|| "--all cannot be combined with a positional source".into());
        }
        options.update_target = if let Some(source) = extension_flag_source {
            if self_flag || extensions_flag || all_flag {
                options.conflicting_options.get_or_insert_with(|| {
                    "--extension cannot be combined with --self, --extensions, or --all".into()
                });
            }
            if options.source.is_some() {
                options.conflicting_options.get_or_insert_with(|| {
                    "--extension cannot be combined with a positional source".into()
                });
            }
            Some(UpdateTarget::Extensions(Some(source)))
        } else if let Some(source) = options.source.as_deref() {
            if source == "self" || source == "pi" {
                Some(if extensions_flag {
                    UpdateTarget::All
                } else {
                    UpdateTarget::SelfOnly
                })
            } else {
                if extensions_flag || self_flag || all_flag {
                    options.conflicting_options.get_or_insert_with(|| "positional update targets cannot be combined with --self, --extensions, or --all".into());
                }
                Some(UpdateTarget::Extensions(Some(source.to_string())))
            }
        } else if all_flag || (self_flag && extensions_flag) {
            Some(UpdateTarget::All)
        } else if self_flag {
            Some(UpdateTarget::SelfOnly)
        } else if extensions_flag {
            Some(UpdateTarget::Extensions(None))
        } else {
            options.show_extensions_skipped_note = true;
            Some(UpdateTarget::SelfOnly)
        };
    }
    Some(options)
}

pub fn handle_package_command<R: CommandRunner>(
    args: &[String],
    package_manager: &mut DefaultPackageManager<R>,
) -> Option<PackageCommandOutput> {
    handle_package_command_with_self_updater(args, package_manager, &ProcessSelfUpdater::default())
}

pub fn handle_package_command_with_self_updater<R: CommandRunner, U: SelfUpdater>(
    args: &[String],
    package_manager: &mut DefaultPackageManager<R>,
    self_updater: &U,
) -> Option<PackageCommandOutput> {
    let options = parse_package_command(args)?;
    if options.help {
        return Some(PackageCommandOutput::success(get_package_command_help(
            options.command,
        )));
    }
    if let Some(option) = &options.invalid_option {
        return Some(PackageCommandOutput::failure(format!(
            "Unknown option {option} for \"{}\".\nUse \"{APP_NAME} --help\" or \"{}\".\n",
            command_name(options.command),
            get_package_command_usage(options.command)
        )));
    }
    if let Some(option) = &options.missing_option_value {
        return Some(PackageCommandOutput::failure(format!(
            "Missing value for {option}.\nUsage: {}\n",
            get_package_command_usage(options.command)
        )));
    }
    if let Some(argument) = &options.invalid_argument {
        return Some(PackageCommandOutput::failure(format!(
            "Unexpected argument {argument}.\nUsage: {}\n",
            get_package_command_usage(options.command)
        )));
    }
    if let Some(conflict) = &options.conflicting_options {
        return Some(PackageCommandOutput::failure(format!(
            "{conflict}\nUsage: {}\n",
            get_package_command_usage(options.command)
        )));
    }
    if matches!(
        options.command,
        PackageCommand::Install | PackageCommand::Remove
    ) && options.source.is_none()
    {
        return Some(PackageCommandOutput::failure(format!(
            "Missing {} source.\nUsage: {}\n",
            command_name(options.command),
            get_package_command_usage(options.command)
        )));
    }
    if let Some(trusted) = options.project_trust_override {
        package_manager
            .settings_manager_mut()
            .set_project_trusted(trusted);
    }
    if options.local && !package_manager.settings_manager().is_project_trusted() {
        return Some(PackageCommandOutput::failure(
            "Project is not trusted. Use --approve to modify local package config.\n".into(),
        ));
    }

    let includes_self_update = options.command == PackageCommand::Update
        && matches!(
            options.update_target.as_ref(),
            Some(UpdateTarget::All | UpdateTarget::SelfOnly)
        );
    let show_extensions_skipped_note = options.show_extensions_skipped_note;
    let force_self_update = options.force;
    let self_update_npm_command = package_manager.settings_manager().get_npm_command();

    let outcome = match options.command {
        PackageCommand::Install => package_manager
            .install_and_persist(options.source.as_deref().unwrap(), options.local)
            .map(|_| format!("Installed {}\n", options.source.unwrap())),
        PackageCommand::Remove => package_manager
            .remove_and_persist(options.source.as_deref().unwrap(), options.local)
            .and_then(|removed| {
                if removed {
                    Ok(format!("Removed {}\n", options.source.unwrap()))
                } else {
                    Err(crate::package_manager::PackageManagerError::Message(
                        format!("No matching package found for {}", options.source.unwrap()),
                    ))
                }
            }),
        PackageCommand::List => {
            let packages = package_manager.list_configured_packages();
            if packages.is_empty() {
                Ok("No packages installed.\n".into())
            } else {
                let mut output = String::new();
                let user: Vec<_> = packages
                    .iter()
                    .filter(|pkg| pkg.scope == crate::package_manager::PackageScope::User)
                    .collect();
                let project: Vec<_> = packages
                    .iter()
                    .filter(|pkg| pkg.scope == crate::package_manager::PackageScope::Project)
                    .collect();
                if !user.is_empty() {
                    output.push_str("User packages:\n");
                    append_packages(&mut output, &user);
                }
                if !project.is_empty() {
                    if !user.is_empty() {
                        output.push('\n');
                    }
                    output.push_str("Project packages:\n");
                    append_packages(&mut output, &project);
                }
                Ok(output)
            }
        }
        PackageCommand::Update => {
            let target = options.update_target.unwrap_or(UpdateTarget::SelfOnly);
            match target {
                UpdateTarget::Extensions(source) => {
                    package_manager.update(source.as_deref()).map(|_| {
                        source.map_or_else(
                            || "Updated packages\n".into(),
                            |source| format!("Updated {source}\n"),
                        )
                    })
                }
                UpdateTarget::All => package_manager
                    .update(None)
                    .map(|_| "Updated packages\n".into()),
                UpdateTarget::SelfOnly => Ok(String::new()),
            }
        }
    };
    Some(match outcome {
        Ok(mut stdout) if includes_self_update => {
            if show_extensions_skipped_note {
                stdout.insert_str(
                    0,
                    &format!(
                        "Extensions are skipped. Run {APP_NAME} update --extensions to update extensions.\n"
                    ),
                );
            }
            match self_updater.update(force_self_update, self_update_npm_command.as_deref()) {
                Ok(SelfUpdateOutcome::UpToDate { version }) => {
                    stdout.push_str(&format!("{APP_NAME} is already up to date (v{version})\n"));
                    PackageCommandOutput::success(stdout)
                }
                Ok(SelfUpdateOutcome::Updated {
                    from,
                    to,
                    display,
                    note,
                }) => {
                    if let Some(note) = note {
                        stdout.push_str(note.trim());
                        stdout.push('\n');
                    }
                    stdout.push_str(&format!("Updating {APP_NAME} with {display}...\n"));
                    stdout.push_str(&format!("Updated {APP_NAME} from {from} to {to}\n"));
                    PackageCommandOutput::success(stdout)
                }
                Ok(SelfUpdateOutcome::Unavailable {
                    instruction,
                    entrypoint,
                }) => PackageCommandOutput {
                    stdout,
                    stderr: self_update_unavailable_output(&instruction, entrypoint.as_deref()),
                    exit_code: 1,
                },
                Err(error) => PackageCommandOutput {
                    stdout,
                    stderr: format!("Error: {error}\n"),
                    exit_code: 1,
                },
            }
        }
        Ok(stdout) => PackageCommandOutput::success(stdout),
        Err(error)
            if error
                .to_string()
                .starts_with("No matching package found for ") =>
        {
            PackageCommandOutput::failure(format!("{error}\n"))
        }
        Err(error) => PackageCommandOutput::failure(format!("Error: {error}\n")),
    })
}

fn self_update_unavailable_output(instruction: &str, entrypoint: Option<&Path>) -> String {
    let mut output =
        format!("error: {APP_NAME} cannot self-update this installation.\n{instruction}\n");
    if let Some(entrypoint) = entrypoint {
        output.push('\n');
        output.push_str(&format!(
            "Location of pi executable: {}\n",
            entrypoint.display()
        ));
    }
    output
}

fn append_packages(output: &mut String, packages: &[&crate::package_manager::ConfiguredPackage]) {
    for package in packages {
        output.push_str("  ");
        output.push_str(&package.source);
        if package.filtered {
            output.push_str(" (filtered)");
        }
        output.push('\n');
        if let Some(path) = &package.installed_path {
            output.push_str("    ");
            output.push_str(&path.to_string_lossy());
            output.push('\n');
        }
    }
}

fn command_name(command: PackageCommand) -> &'static str {
    match command {
        PackageCommand::Install => "install",
        PackageCommand::Remove => "remove",
        PackageCommand::Update => "update",
        PackageCommand::List => "list",
    }
}

fn install_method_name(method: InstallMethod) -> &'static str {
    match method {
        InstallMethod::Npm => "npm",
        InstallMethod::Pnpm => "pnpm",
        InstallMethod::Bun => "bun",
        InstallMethod::BunBinary => "bun-binary",
        InstallMethod::Unknown => "unknown",
    }
}
