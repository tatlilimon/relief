//! The user's rules: where to look, how long "unused" is, and what may be deleted unattended.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{caches, docker, rules};

/// What an unattended run does with one kind of thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Delete it.
    Auto,
    /// Leave it for an interactive `relief clean`.
    Ask,
    /// Do not look for it at all.
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub mode: Mode,
    /// Days without use after which the thing counts as unused.
    pub days: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    mode: Option<Mode>,
    #[serde(alias = "idle_days", alias = "unused_days")]
    days: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    mode: Option<Mode>,
    #[serde(alias = "idle_days", alias = "unused_days")]
    days: Option<u32>,
    #[serde(default)]
    kind: BTreeMap<String, Rule>,
}

impl Section {
    /// The rule for one kind: its own setting, else the section's, else the built-in
    /// default for that kind.
    fn policy(&self, kind: &str, default: Policy) -> Policy {
        let rule = self.kind.get(kind);
        Policy {
            mode: rule
                .and_then(|r| r.mode)
                .or(self.mode)
                .unwrap_or(default.mode),
            days: rule
                .and_then(|r| r.days)
                .or(self.days)
                .unwrap_or(default.days),
        }
    }

    fn check(&self, name: &str, kinds: &[&str]) -> Result<()> {
        let days = self.kind.values().map(|r| r.days).chain([self.days]);
        if days.flatten().any(|d| d == 0) {
            bail!("[{name}]: days must be at least 1, or things used today would be deleted");
        }
        for kind in self.kind.keys() {
            if !kinds.contains(&kind.as_str()) {
                bail!(
                    "[{name}.kind.{kind}]: unknown kind; known kinds are {}",
                    kinds.join(", ")
                );
            }
        }
        Ok(())
    }
}

fn default_roots() -> Vec<String> {
    vec!["~".to_string()]
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directories searched for projects.
    #[serde(default = "default_roots")]
    pub roots: Vec<String>,
    /// Directories that are never touched.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Whether reading a file counts as using it.
    #[serde(default = "yes")]
    pub count_reads: bool,
    #[serde(default)]
    projects: Section,
    #[serde(default)]
    caches: Section,
    #[serde(default)]
    docker: Section,
}

const fn auto(days: u32) -> Policy {
    Policy {
        mode: Mode::Auto,
        days,
    }
}

fn project_default(_kind: &str) -> Policy {
    auto(30)
}

fn cache_default(kind: &str) -> Policy {
    match kind {
        // Models run to gigabytes and take long to fetch again.
        "huggingface" => Policy {
            mode: Mode::Ask,
            days: 60,
        },
        _ => auto(60),
    }
}

fn docker_default(kind: &str) -> Policy {
    match kind {
        "build-cache" => auto(30),
        "dangling-images" => auto(7),
        _ => Policy {
            mode: Mode::Ask,
            days: 60,
        },
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.projects.check("projects", &rules::kinds())?;
        config.caches.check("caches", caches::KINDS)?;
        config.docker.check("docker", docker::KINDS)?;
        Ok(config)
    }

    /// Read the file, or use the defaults when there is none.
    pub fn load(file: &Path) -> Result<Config> {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", file.display())),
        };
        Config::parse(&text).with_context(|| format!("in {}", file.display()))
    }

    pub fn project(&self, kind: &str) -> Policy {
        self.projects.policy(kind, project_default(kind))
    }

    pub fn cache(&self, kind: &str) -> Policy {
        self.caches.policy(kind, cache_default(kind))
    }

    pub fn docker(&self, kind: &str) -> Policy {
        self.docker.policy(kind, docker_default(kind))
    }
}

/// The commented file that `relief config init` writes.
pub const TEMPLATE: &str = r#"# relief configuration. Every line below shows the default.

# Directories searched for projects. Hidden directories are never searched.
roots = ["~"]

# Directories that are never touched, e.g. ["~/archive", "~/work/client-x"].
# A file named .reliefignore inside a directory has the same effect for that directory.
ignore = []

# Count reading a project's build output or installed dependencies (which running or
# building it does) as using the project, and reading a cache entry as using the entry.
# Reading source files never counts. Turn this off if something on your machine reads every
# file regularly (some backup tools do), which makes everything look used.
count_reads = true

# What an unattended run (`relief clean --auto`, the schedule) does with each kind:
#   "auto"  delete,  "ask"  leave for `relief clean`,  "off"  do not look at all.

# Build output and installed dependencies inside projects.
[projects]
mode = "auto"
# A project is unused when for this many days none of its files changed, nothing read its
# build output or dependencies, git saw no activity and no process worked inside it.
idle_days = 30

# One kind can have its own rule, for example because Rust rebuilds are slow:
# [projects.kind.rust]
# idle_days = 90
# mode = "ask"

# Package-manager and tool caches. Unset, every kind is "auto" after 60 days, except
# huggingface, whose models are large to fetch again: it is "ask".
[caches]
# mode = "auto"
# A cache entry is unused when it was not read or changed for this many days.
# unused_days = 60

# [caches.kind.uv]
# mode = "off"

# Docker, through the docker command. Volumes are never deleted: they can hold data.
# Unset, the kinds behave like this:
#   build-cache      "auto", entries docker has not used for 30 days
#   dangling-images  "auto", untagged images older than 7 days that no container uses
#   images           "ask",  pulled images no container uses, last pulled 60+ days ago
[docker]
# [docker.kind.images]
# mode = "auto"
# unused_days = 90
"#;

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
