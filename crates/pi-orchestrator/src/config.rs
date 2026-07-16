use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const CONFIG_DIR_NAME: &str = ".pi";
const ENV_ORCHESTRATOR_DIR: &str = "PI_ORCHESTRATOR_DIR";
const ENV_CONFIG_DIR: &str = "PI_CONFIG_DIR";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn configured_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn resolve_orchestrator_dir(
    orchestrator_dir: Option<OsString>,
    config_dir: Option<OsString>,
    home_dir: Option<&Path>,
) -> PathBuf {
    if let Some(path) = configured_path(orchestrator_dir) {
        return path;
    }

    let pi_dir = configured_path(config_dir).unwrap_or_else(|| {
        home_dir
            .map(|home| home.join(CONFIG_DIR_NAME))
            .unwrap_or_else(|| PathBuf::from(".").join(CONFIG_DIR_NAME))
    });
    pi_dir.join("orchestrator")
}

pub fn get_orchestrator_dir() -> PathBuf {
    resolve_orchestrator_dir(
        env::var_os(ENV_ORCHESTRATOR_DIR),
        env::var_os(ENV_CONFIG_DIR),
        dirs::home_dir().as_deref(),
    )
}

pub fn get_auth_path() -> PathBuf {
    get_orchestrator_dir().join("auth.json")
}

pub fn get_machine_path() -> PathBuf {
    get_orchestrator_dir().join("machine.json")
}

pub fn get_instances_path() -> PathBuf {
    get_orchestrator_dir().join("instances.json")
}

pub fn get_socket_path() -> PathBuf {
    get_orchestrator_dir().join("orchestrator.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orchestrator_dir_has_exact_precedence() {
        let home = Path::new("/home/tester");
        assert_eq!(
            resolve_orchestrator_dir(
                Some(OsString::from("/runtime/orchestrator")),
                Some(OsString::from("/runtime/pi")),
                Some(home),
            ),
            Path::new("/runtime/orchestrator")
        );
        assert_eq!(
            resolve_orchestrator_dir(None, Some(OsString::from("/runtime/pi")), Some(home)),
            Path::new("/runtime/pi/orchestrator")
        );
        assert_eq!(
            resolve_orchestrator_dir(None, None, Some(home)),
            Path::new("/home/tester/.pi/orchestrator")
        );
    }

    #[test]
    fn empty_env_values_are_ignored_like_node() {
        let home = Path::new("/home/tester");
        assert_eq!(
            resolve_orchestrator_dir(Some(OsString::new()), Some(OsString::new()), Some(home),),
            Path::new("/home/tester/.pi/orchestrator")
        );
    }

    #[test]
    fn missing_home_uses_the_established_current_directory_fallback() {
        assert_eq!(
            resolve_orchestrator_dir(None, None, None),
            Path::new("./.pi/orchestrator")
        );
    }

    #[test]
    fn file_names_match_the_oracle() {
        let root = resolve_orchestrator_dir(Some(OsString::from("/tmp/orchestrator")), None, None);
        assert_eq!(
            root.join("auth.json"),
            Path::new("/tmp/orchestrator/auth.json")
        );
        assert_eq!(
            root.join("machine.json"),
            Path::new("/tmp/orchestrator/machine.json")
        );
        assert_eq!(
            root.join("instances.json"),
            Path::new("/tmp/orchestrator/instances.json")
        );
        assert_eq!(
            root.join("orchestrator.sock"),
            Path::new("/tmp/orchestrator/orchestrator.sock")
        );
    }
}
