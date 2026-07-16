//! Bun executable and sidecar entry resolution (decision 7).
//!
//! Detection order: `$PI_RUST_BUN` override, `$BUN_INSTALL/bin`, then `$PATH`.
//! Absent ⇒ extensions are disabled and the caller prints one exact install
//! command ([`BUN_INSTALL_COMMAND`]). No auto-download, no bundling.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// The one exact command printed when Bun is missing.
pub const BUN_INSTALL_COMMAND: &str = "curl -fsSL https://bun.sh/install | bash";
/// Env var forcing a specific Bun executable.
pub const BUN_ENV_OVERRIDE: &str = "PI_RUST_BUN";
/// Env var forcing a specific sidecar checkout or entry script.
pub const SIDECAR_ENV_OVERRIDE: &str = "PI_RUST_SIDECAR";

const BUN_FILE_NAME: &str = if cfg!(windows) { "bun.exe" } else { "bun" };

/// Environment inputs for [`resolve_bun`], captured explicitly so resolution
/// stays a pure function (tests never mutate process env).
#[derive(Clone, Debug, Default)]
pub struct BunEnvironment {
    /// `$PI_RUST_BUN`.
    pub bun_override: Option<PathBuf>,
    /// `$BUN_INSTALL`.
    pub bun_install: Option<PathBuf>,
    /// `$PATH`.
    pub path: Option<OsString>,
}

impl BunEnvironment {
    pub fn from_env() -> Self {
        Self {
            bun_override: env::var_os(BUN_ENV_OVERRIDE).map(PathBuf::from),
            bun_install: env::var_os("BUN_INSTALL").map(PathBuf::from),
            path: env::var_os("PATH"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum BunResolveError {
    /// An explicit override that does not resolve is loud, never a fallthrough.
    #[error("${BUN_ENV_OVERRIDE} points at `{0}`, which is not an executable file")]
    BadOverride(PathBuf),
    #[error(
        "bun executable not found via ${BUN_ENV_OVERRIDE}, $BUN_INSTALL/bin, or $PATH; \
         extensions are disabled. Install Bun with: {BUN_INSTALL_COMMAND}"
    )]
    NotFound,
}

/// Resolve the Bun executable to a canonical path.
pub fn resolve_bun(env: &BunEnvironment) -> Result<PathBuf, BunResolveError> {
    if let Some(overridden) = &env.bun_override {
        return canonical_executable(overridden)
            .ok_or_else(|| BunResolveError::BadOverride(overridden.clone()));
    }
    if let Some(install) = &env.bun_install
        && let Some(found) = canonical_executable(&install.join("bin").join(BUN_FILE_NAME))
    {
        return Ok(found);
    }
    if let Some(path) = &env.path {
        for dir in env::split_paths(path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if let Some(found) = canonical_executable(&dir.join(BUN_FILE_NAME)) {
                return Ok(found);
            }
        }
    }
    Err(BunResolveError::NotFound)
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum SidecarResolveError {
    #[error(
        "${SIDECAR_ENV_OVERRIDE} points at `{0}`, which is neither an entry script \
         nor a directory containing src/main.ts"
    )]
    BadOverride(PathBuf),
    #[error(
        "sidecar entry not found under `{0}`; set ${SIDECAR_ENV_OVERRIDE} to a sidecar \
         checkout or entry script"
    )]
    NotFound(PathBuf),
}

/// Resolve the sidecar entry script to a canonical path.
///
/// `overridden` is `$PI_RUST_SIDECAR` (a file used verbatim, or a directory
/// joined with `src/main.ts`); the default is `<package_dir>/sidecar`.
pub fn resolve_sidecar_entry(
    overridden: Option<&Path>,
    package_dir: &Path,
) -> Result<PathBuf, SidecarResolveError> {
    if let Some(path) = overridden {
        return sidecar_entry_at(path)
            .ok_or_else(|| SidecarResolveError::BadOverride(path.to_path_buf()));
    }
    let default = package_dir.join("sidecar");
    sidecar_entry_at(&default).ok_or(SidecarResolveError::NotFound(default))
}

fn sidecar_entry_at(path: &Path) -> Option<PathBuf> {
    let meta = fs::metadata(path).ok()?;
    let entry = if meta.is_dir() {
        path.join("src").join("main.ts")
    } else {
        path.to_path_buf()
    };
    if fs::metadata(&entry).ok()?.is_file() {
        fs::canonicalize(&entry).ok()
    } else {
        None
    }
}

fn canonical_executable(path: &Path) -> Option<PathBuf> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    fs::canonicalize(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_executable(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn override_wins_and_is_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let bun = make_executable(dir.path(), "custom-bun");
        let env = BunEnvironment {
            bun_override: Some(bun.clone()),
            bun_install: Some(PathBuf::from("/nonexistent")),
            path: Some(OsString::from("/nonexistent")),
        };
        assert_eq!(resolve_bun(&env).unwrap(), fs::canonicalize(&bun).unwrap());
    }

    #[test]
    fn bad_override_is_loud_not_a_fallthrough() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-bun");
        let env = BunEnvironment {
            bun_override: Some(missing.clone()),
            ..Default::default()
        };
        assert_eq!(
            resolve_bun(&env),
            Err(BunResolveError::BadOverride(missing))
        );
    }

    #[cfg(unix)]
    #[test]
    fn bun_install_beats_path() {
        let install = tempfile::tempdir().unwrap();
        let bin = install.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let installed = make_executable(&bin, "bun");
        let path_dir = tempfile::tempdir().unwrap();
        make_executable(path_dir.path(), "bun");
        let env = BunEnvironment {
            bun_override: None,
            bun_install: Some(install.path().to_path_buf()),
            path: Some(path_dir.path().as_os_str().to_owned()),
        };
        assert_eq!(
            resolve_bun(&env).unwrap(),
            fs::canonicalize(&installed).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_skips_non_executables() {
        let first = tempfile::tempdir().unwrap();
        fs::write(first.path().join("bun"), "not executable").unwrap();
        let second = tempfile::tempdir().unwrap();
        let real = make_executable(second.path(), "bun");
        let joined =
            env::join_paths([first.path().to_path_buf(), second.path().to_path_buf()]).unwrap();
        let env = BunEnvironment {
            bun_override: None,
            bun_install: None,
            path: Some(joined),
        };
        assert_eq!(resolve_bun(&env).unwrap(), fs::canonicalize(&real).unwrap());
    }

    #[test]
    fn missing_bun_names_the_install_command() {
        let error = resolve_bun(&BunEnvironment::default()).unwrap_err();
        assert_eq!(error, BunResolveError::NotFound);
        assert!(error.to_string().contains(BUN_INSTALL_COMMAND));
    }

    #[test]
    fn sidecar_dir_override_resolves_entry() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        let entry = src.join("main.ts");
        fs::write(&entry, "// entry").unwrap();
        let resolved = resolve_sidecar_entry(Some(dir.path()), Path::new("/nonexistent")).unwrap();
        assert_eq!(resolved, fs::canonicalize(&entry).unwrap());

        let direct = resolve_sidecar_entry(Some(&entry), Path::new("/nonexistent")).unwrap();
        assert_eq!(direct, fs::canonicalize(&entry).unwrap());
    }

    #[test]
    fn sidecar_missing_default_reports_the_probed_path() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_sidecar_entry(None, dir.path()).unwrap_err();
        assert_eq!(
            error,
            SidecarResolveError::NotFound(dir.path().join("sidecar"))
        );
    }
}
