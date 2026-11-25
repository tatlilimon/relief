//! Where things live for this user: home, configuration, state, and the tools on PATH.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

pub struct Env {
    pub home: PathBuf,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    /// The current time in seconds since the epoch.
    pub now: i64,
    vars: HashMap<String, String>,
}

impl Env {
    pub fn from_vars(vars: HashMap<String, String>, now: i64) -> Result<Env> {
        let get = |key: &str| vars.get(key).filter(|v| !v.is_empty());
        let home = get("HOME").context("HOME is not set")?;
        // Canonical, so that paths found on disk compare equal to it.
        let home = Path::new(home)
            .canonicalize()
            .with_context(|| format!("home directory {home} is not usable"))?;
        let xdg = |key: &str, default: &str| match get(key) {
            Some(dir) => PathBuf::from(dir),
            None => home.join(default),
        };
        let config_file = match get("RELIEF_CONFIG") {
            Some(file) => PathBuf::from(file),
            None => xdg("XDG_CONFIG_HOME", ".config").join("relief/config.toml"),
        };
        let state_dir = xdg("XDG_STATE_HOME", ".local/state").join("relief");
        // Tests cannot backdate a file's status-change time, so they move the clock instead.
        let now = get("RELIEF_TEST_NOW")
            .and_then(|v| v.parse().ok())
            .unwrap_or(now);
        Ok(Env {
            home,
            config_file,
            state_dir,
            now,
            vars,
        })
    }

    pub fn from_process() -> Result<Env> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Env::from_vars(std::env::vars().collect(), now)
    }

    /// An environment variable; set-but-empty counts as unset.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn xdg(&self, key: &str, default: &str) -> PathBuf {
        match self.var(key) {
            Some(dir) => PathBuf::from(dir),
            None => self.home.join(default),
        }
    }

    /// `$XDG_CACHE_HOME`, or `~/.cache`.
    pub fn cache_home(&self) -> PathBuf {
        self.xdg("XDG_CACHE_HOME", ".cache")
    }

    /// `$XDG_DATA_HOME`, or `~/.local/share`.
    pub fn data_home(&self) -> PathBuf {
        self.xdg("XDG_DATA_HOME", ".local/share")
    }

    /// `$XDG_CONFIG_HOME`, or `~/.config`.
    pub fn config_home(&self) -> PathBuf {
        self.xdg("XDG_CONFIG_HOME", ".config")
    }

    /// The executable `program` would run from PATH.
    pub fn which(&self, program: &str) -> Option<PathBuf> {
        std::env::split_paths(self.var("PATH")?)
            .map(|dir| dir.join(program))
            .find(|file| {
                file.metadata()
                    .is_ok_and(|md| md.is_file() && md.permissions().mode() & 0o111 != 0)
            })
    }

    /// Turn a path from the configuration (`~/x`, `~`, or absolute) into a real one.
    pub fn expand(&self, path: &str) -> PathBuf {
        if path == "~" {
            self.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            PathBuf::from(path)
        }
    }

    /// A path as shown to the user, with the home directory written as `~`.
    pub fn tilde(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

#[cfg(test)]
#[path = "env_tests.rs"]
mod tests;
