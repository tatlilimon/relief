//! Which directories inside a project can be produced again, and how.
//!
//! A directory is only ever a candidate when the project says how to rebuild it: a manifest
//! next to it, and where versions would otherwise drift, a lockfile too.

/// Extra evidence that a directory really is what its name suggests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    None,
    /// This file must exist inside the directory.
    Inside(&'static str),
    /// This entry must exist next to the directory.
    Sibling(&'static str),
}

#[derive(Debug)]
pub struct Artifact {
    pub name: &'static str,
    pub proof: Proof,
    /// Lockfiles and the command each one restores the directory with. When the list is not
    /// empty, one of them must exist in the project or in a directory above it.
    pub locks: &'static [(&'static str, &'static str)],
    /// How to get the directory back when no lockfile decides it.
    pub regen: &'static str,
}

#[derive(Debug)]
pub struct Rule {
    pub kind: &'static str,
    /// Files that mark a directory as a project of this kind. `*` matches any run of
    /// characters.
    pub markers: &'static [&'static str],
    pub artifacts: &'static [Artifact],
}

const fn built(name: &'static str, regen: &'static str) -> Artifact {
    Artifact {
        name,
        proof: Proof::None,
        locks: &[],
        regen,
    }
}

const NODE_LOCKS: &[(&str, &str)] = &[
    ("pnpm-lock.yaml", "pnpm install"),
    ("yarn.lock", "yarn install"),
    ("bun.lock", "bun install"),
    ("bun.lockb", "bun install"),
    ("package-lock.json", "npm ci"),
    ("npm-shrinkwrap.json", "npm ci"),
];

const VENV: &str = "recreate the virtualenv from the project's dependency file";

const fn venv(name: &'static str) -> Artifact {
    Artifact {
        name,
        proof: Proof::Inside("pyvenv.cfg"),
        locks: &[],
        regen: VENV,
    }
}

pub static RULES: &[Rule] = &[
    Rule {
        kind: "node",
        markers: &["package.json"],
        artifacts: &[
            Artifact {
                name: "node_modules",
                proof: Proof::None,
                locks: NODE_LOCKS,
                regen: "",
            },
            built(".next", "next build"),
            built(".nuxt", "nuxt build"),
            built(".svelte-kit", "rebuilt by the next dev or build run"),
            built(".turbo", "rebuilt by the next turbo run"),
            built(".parcel-cache", "rebuilt by the next parcel run"),
            built(".angular", "rebuilt by the next ng build"),
        ],
    },
    Rule {
        kind: "rust",
        markers: &["Cargo.toml"],
        artifacts: &[Artifact {
            name: "target",
            proof: Proof::Inside("CACHEDIR.TAG"),
            locks: &[],
            regen: "cargo build",
        }],
    },
    Rule {
        kind: "python",
        markers: &[
            "pyproject.toml",
            "requirements*.txt",
            "setup.py",
            "setup.cfg",
            "Pipfile",
            "poetry.lock",
            "uv.lock",
        ],
        artifacts: &[
            venv(".venv"),
            venv("venv"),
            venv("env"),
            built(".tox", "tox"),
            built(".nox", "nox"),
            built(".mypy_cache", "rebuilt by the next mypy run"),
            built(".pytest_cache", "rebuilt by the next pytest run"),
            built(".ruff_cache", "rebuilt by the next ruff run"),
        ],
    },
    Rule {
        kind: "gradle",
        markers: &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        artifacts: &[
            built("build", "gradle build"),
            built(".gradle", "gradle build"),
        ],
    },
    Rule {
        kind: "maven",
        markers: &["pom.xml"],
        artifacts: &[built("target", "mvn package")],
    },
    Rule {
        kind: "php",
        markers: &["composer.json"],
        artifacts: &[Artifact {
            name: "vendor",
            proof: Proof::Inside("autoload.php"),
            locks: &[("composer.lock", "composer install")],
            regen: "",
        }],
    },
    Rule {
        kind: "dotnet",
        markers: &["*.csproj", "*.fsproj", "*.vbproj"],
        artifacts: &[
            built("obj", "dotnet build"),
            Artifact {
                name: "bin",
                proof: Proof::Sibling("obj"),
                locks: &[],
                regen: "dotnet build",
            },
        ],
    },
    Rule {
        kind: "elixir",
        markers: &["mix.exs"],
        artifacts: &[
            built("_build", "mix compile"),
            Artifact {
                name: "deps",
                proof: Proof::None,
                locks: &[("mix.lock", "mix deps.get")],
                regen: "",
            },
        ],
    },
    Rule {
        kind: "dart",
        markers: &["pubspec.yaml"],
        artifacts: &[
            built(".dart_tool", "dart pub get"),
            built("build", "flutter build"),
        ],
    },
    Rule {
        kind: "zig",
        markers: &["build.zig"],
        artifacts: &[
            built(".zig-cache", "zig build"),
            built("zig-cache", "zig build"),
            built("zig-out", "zig build"),
        ],
    },
    Rule {
        kind: "swift",
        markers: &["Package.swift"],
        artifacts: &[built(".build", "swift build")],
    },
    Rule {
        kind: "haskell",
        markers: &["stack.yaml", "cabal.project", "*.cabal"],
        artifacts: &[
            built(".stack-work", "stack build"),
            built("dist-newstyle", "cabal build"),
        ],
    },
];

/// Directories that hold installed third-party code. Nothing below them is a project of the
/// user's, so they are never searched.
pub const NEVER_SEARCH: &[&str] = &[
    "node_modules",
    "bower_components",
    "site-packages",
    "__pycache__",
];

/// A directory holding one of these is an installed environment, a cache, or opted out.
pub const NOT_A_WORKSPACE: &[&str] = &["pyvenv.cfg", "CACHEDIR.TAG", ".reliefignore"];

/// The names found in one directory.
#[derive(Debug, Default, Clone)]
pub struct Listing(Vec<String>);

impl Listing {
    pub fn new<I, S>(names: I) -> Listing
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Listing(names.into_iter().map(Into::into).collect())
    }

    /// Whether an entry matches `pattern`, in which `*` stands for any run of characters.
    pub fn has(&self, pattern: &str) -> bool {
        match pattern.split_once('*') {
            None => self.0.iter().any(|name| name == pattern),
            Some((head, tail)) => self.0.iter().any(|name| {
                name.len() >= head.len() + tail.len()
                    && name.starts_with(head)
                    && name.ends_with(tail)
            }),
        }
    }
}

/// The rule under which entry `name` of a directory with these entries can be regenerated.
pub fn find_artifact(listing: &Listing, name: &str) -> Option<(&'static Rule, &'static Artifact)> {
    RULES
        .iter()
        .filter(|rule| rule.markers.iter().any(|m| listing.has(m)))
        .find_map(|rule| {
            let artifact = rule.artifacts.iter().find(|a| a.name == name)?;
            Some((rule, artifact))
        })
}

/// Whether a directory with these entries is a project of any known kind.
pub fn is_project(listing: &Listing) -> bool {
    RULES
        .iter()
        .any(|rule| rule.markers.iter().any(|m| listing.has(m)))
}

/// Every project kind, for validating configuration.
pub fn kinds() -> Vec<&'static str> {
    RULES.iter().map(|r| r.kind).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(names: &[&str], entry: &str) -> Option<&'static str> {
        find_artifact(&Listing::new(names.iter().copied()), entry).map(|(rule, _)| rule.kind)
    }

    #[test]
    fn a_directory_is_an_artifact_only_next_to_its_manifest() {
        assert_eq!(
            kind_of(&["package.json", "node_modules"], "node_modules"),
            Some("node")
        );
        assert_eq!(
            kind_of(&["README.md", "node_modules"], "node_modules"),
            None
        );
    }

    #[test]
    fn the_same_name_means_different_things_in_different_projects() {
        assert_eq!(kind_of(&["Cargo.toml", "target"], "target"), Some("rust"));
        assert_eq!(kind_of(&["pom.xml", "target"], "target"), Some("maven"));
        assert_eq!(
            kind_of(&["build.gradle.kts", "build"], "build"),
            Some("gradle")
        );
        assert_eq!(kind_of(&["pubspec.yaml", "build"], "build"), Some("dart"));
    }

    #[test]
    fn a_generic_name_is_not_an_artifact_in_an_unrelated_project() {
        // `build` and `bin` hold hand-written files in plenty of node and rust projects.
        assert_eq!(kind_of(&["package.json", "build"], "build"), None);
        assert_eq!(kind_of(&["Cargo.toml", "bin"], "bin"), None);
        assert_eq!(kind_of(&["package.json", "src"], "src"), None);
    }

    #[test]
    fn manifests_can_be_matched_by_pattern() {
        assert_eq!(kind_of(&["App.csproj", "obj"], "obj"), Some("dotnet"));
        assert_eq!(
            kind_of(&["requirements-dev.txt", ".venv"], ".venv"),
            Some("python")
        );
        assert_eq!(kind_of(&["notes.txt", ".venv"], ".venv"), None);
        assert_eq!(kind_of(&["csproj", "obj"], "obj"), None);
    }

    #[test]
    fn a_pattern_needs_both_its_ends_to_fit() {
        let listing = Listing::new(["requirements.txt.bak", "xrequirements.txt"]);
        assert!(!listing.has("requirements*.txt"));
        assert!(Listing::new(["requirements.txt"]).has("requirements*.txt"));
        assert!(!Listing::new(["a.txt"]).has("a.txt*.txt"));
    }

    #[test]
    fn any_manifest_makes_a_directory_a_project() {
        assert!(is_project(&Listing::new(["go.sum", "mix.exs"])));
        assert!(is_project(&Listing::new(["Lib.fsproj"])));
        assert!(!is_project(&Listing::new(["README.md", "src", "Makefile"])));
    }

    #[test]
    fn node_modules_names_the_installer_that_matches_the_lockfile() {
        let (_, artifact) = find_artifact(
            &Listing::new(["package.json", "node_modules"]),
            "node_modules",
        )
        .unwrap();
        let command = |lock: &str| {
            artifact
                .locks
                .iter()
                .find(|(name, _)| *name == lock)
                .map(|(_, cmd)| *cmd)
        };
        assert_eq!(command("pnpm-lock.yaml"), Some("pnpm install"));
        assert_eq!(command("package-lock.json"), Some("npm ci"));
        assert_eq!(command("Cargo.lock"), None);
    }

    #[test]
    fn every_artifact_says_how_to_get_it_back() {
        for rule in RULES {
            for artifact in rule.artifacts {
                assert!(
                    !artifact.regen.is_empty() || !artifact.locks.is_empty(),
                    "{}/{} has no way back",
                    rule.kind,
                    artifact.name
                );
            }
        }
    }
}
