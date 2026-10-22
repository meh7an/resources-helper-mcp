use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{Error, Result};

const APP_DIR: &str = ".resources-helper";
const HOME_VAR: &str = "USERPROFILE";

/// Locations of the per-user files under `%USERPROFILE%\.resources-helper\`.
///
/// Not under `AppData`: Windows redirects writes there from MSIX-packaged apps, such as the
/// Claude desktop app and every process it starts, into a per-package folder that processes
/// outside the package cannot see. Sessions in the desktop app and in a terminal would then
/// get different tokens, databases, and lock files.
///
/// Nothing is created here; the daemon creates the directory on first start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
    root: PathBuf,
}

impl Paths {
    /// Resolves the paths from the `USERPROFILE` environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when `USERPROFILE` is unset or empty.
    pub fn from_env() -> Result<Self> {
        Self::from_home(env::var_os(HOME_VAR))
    }

    fn from_home(home: Option<OsString>) -> Result<Self> {
        match home {
            Some(home) if !home.is_empty() => Ok(Self::under(home)),
            _ => Err(Error::Config(format!("{HOME_VAR} is not set"))),
        }
    }

    /// Paths rooted at `<base>\.resources-helper`, for tests and non-default installs.
    pub fn under(base: impl Into<PathBuf>) -> Self {
        let home = base.into();
        Self {
            root: home.join(APP_DIR),
            home,
        }
    }

    /// The home directory the install lives under, which also holds Claude Code's `.claude`
    /// directory and `.claude.json`.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The `.resources-helper` directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the installed `rh`, `rhd`, and `rh-mcp` executables live.
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// The TOML config file.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// The bearer token clients send to the daemon.
    pub fn token_file(&self) -> PathBuf {
        self.root.join("token")
    }

    /// The SQLite database.
    pub fn database_file(&self) -> PathBuf {
        self.root.join("rh.db")
    }

    /// The file the running daemon keeps locked, so a second one refuses to start.
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("rhd.lock")
    }

    /// Directory for the daemon's log files.
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_from_home_roots_under_dot_dir() {
        let home = PathBuf::from(r"C:\Users\someone");
        let paths = Paths::from_home(Some(home.clone().into_os_string())).unwrap();
        let root = home.join(".resources-helper");
        assert_eq!(paths.home(), home);
        assert_eq!(paths.root(), root);
        assert_eq!(paths.bin_dir(), root.join("bin"));
        assert_eq!(paths.config_file(), root.join("config.toml"));
        assert_eq!(paths.token_file(), root.join("token"));
        assert_eq!(paths.database_file(), root.join("rh.db"));
        assert_eq!(paths.log_dir(), root.join("logs"));
        assert_eq!(paths.lock_file(), root.join("rhd.lock"));
    }

    #[test]
    fn test_paths_from_home_unset_or_empty_is_config_error() {
        for home in [None, Some(OsString::new())] {
            let result = Paths::from_home(home);
            assert!(matches!(result, Err(Error::Config(message)) if message.contains(HOME_VAR)));
        }
    }

    #[test]
    fn test_paths_from_env_matches_user_profile() {
        if let Some(home) = env::var_os(HOME_VAR).filter(|home| !home.is_empty()) {
            assert_eq!(Paths::from_env().unwrap(), Paths::under(home));
        }
    }
}
