//! Finding regenerable directories inside projects and deciding whether their project is
//! still in use.
//!
//! Use is judged for a whole *unit*: the outermost project directory inside the enclosing
//! git repository. A monorepo is one unit, so one busy package keeps its neighbours'
//! dependencies in place.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, FileType};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::fsmeta::{self, Measure, SharedTimes, Usage};
use crate::remove::is_trash;
use crate::rules::{self, Listing, Proof};

const DAY: i64 = 86_400;

/// A directory that could be deleted and produced again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub kind: &'static str,
    pub path: PathBuf,
    /// The directory holding the manifest.
    pub project: PathBuf,
    /// The directory whose activity decides whether this is in use.
    pub unit: PathBuf,
    pub git_root: Option<PathBuf>,
    /// The command that brings the directory back.
    pub regen: String,
}

#[derive(Debug, Default)]
pub struct Found {
    pub candidates: Vec<Candidate>,
    /// Tombstones left by an interrupted run.
    pub leftovers: Vec<PathBuf>,
}

pub struct Search<'a> {
    pub home: &'a Path,
    /// Directories that are never entered.
    pub ignore: &'a [PathBuf],
    pub cross_filesystems: bool,
}

/// Search `roots` (canonical paths) for candidates.
pub fn discover(roots: &[PathBuf], search: &Search) -> Found {
    let found = Mutex::new(Found::default());
    for root in roots {
        let Ok(md) = fs::symlink_metadata(root) else {
            continue;
        };
        let walker = Walker {
            search,
            dev: md.dev(),
            found: &found,
        };
        if md.is_dir() && !walker.ignored(root) {
            walker.visit(root, &context_above(root, search.home));
        }
    }
    found.into_inner().unwrap()
}

/// What the directories above tell about the one being visited.
#[derive(Clone, Default)]
struct Context {
    /// Nearest directory at or above holding `.git`.
    git_root: Option<PathBuf>,
    /// Outermost project directory at or above, inside that repository.
    unit: Option<PathBuf>,
}

impl Context {
    fn enter(&self, dir: &Path, listing: &Listing, home: &Path) -> Context {
        if dir == home {
            // The home directory is neither a project nor a repository of projects, even
            // when a stray package.json or a dotfiles repository lives there.
            return Context::default();
        }
        let here = || Some(dir.to_path_buf());
        let project = rules::is_project(listing);
        if listing.has(".git") {
            Context {
                git_root: here(),
                unit: if project { here() } else { None },
            }
        } else {
            Context {
                git_root: self.git_root.clone(),
                unit: self
                    .unit
                    .clone()
                    .or_else(|| if project { here() } else { None }),
            }
        }
    }
}

fn entries(dir: &Path) -> Option<Vec<(OsString, FileType)>> {
    let read = fs::read_dir(dir).ok()?;
    Some(
        read.filter_map(Result::ok)
            .filter_map(|e| Some((e.file_name(), e.file_type().ok()?)))
            .collect(),
    )
}

fn listing(entries: &[(OsString, FileType)]) -> Listing {
    Listing::new(entries.iter().map(|(name, _)| name.to_string_lossy()))
}

/// The context a search starting at `root` inherits from the directories above it.
fn context_above(root: &Path, home: &Path) -> Context {
    if home.starts_with(root) {
        return Context::default();
    }
    let mut above: Vec<&Path> = root
        .ancestors()
        .skip(1)
        .take_while(|dir| *dir != home && dir.parent().is_some())
        .collect();
    above.reverse();
    let mut context = Context::default();
    for dir in above {
        if let Some(found) = entries(dir) {
            context = context.enter(dir, &listing(&found), home);
        }
    }
    context
}

struct Walker<'a> {
    search: &'a Search<'a>,
    dev: u64,
    found: &'a Mutex<Found>,
}

impl Walker<'_> {
    fn ignored(&self, path: &Path) -> bool {
        self.search.ignore.iter().any(|i| path.starts_with(i))
    }

    fn visit(&self, dir: &Path, above: &Context) {
        let Some(found) = entries(dir) else {
            return;
        };
        let listing = listing(&found);
        if rules::NOT_A_WORKSPACE.iter().any(|tag| listing.has(tag)) {
            return;
        }
        let context = above.enter(dir, &listing, self.search.home);
        let mut inside = Vec::new();
        for (name, kind) in &found {
            let path = dir.join(name);
            if is_trash(name) {
                self.found.lock().unwrap().leftovers.push(path);
                continue;
            }
            // A symbolic link is never a directory here, so links are neither deleted nor
            // followed.
            if !kind.is_dir() || self.ignored(&path) {
                continue;
            }
            let name = name.to_string_lossy();
            if let Some(candidate) = self.candidate(dir, &listing, &name, &path, &context) {
                self.found.lock().unwrap().candidates.push(candidate);
                continue;
            }
            if name.starts_with('.') || rules::NEVER_SEARCH.contains(&name.as_ref()) {
                continue;
            }
            if !self.search.cross_filesystems
                && fs::symlink_metadata(&path).map(|md| md.dev()).ok() != Some(self.dev)
            {
                continue;
            }
            inside.push(path);
        }
        inside
            .par_iter()
            .for_each(|path| self.visit(path, &context));
    }

    fn candidate(
        &self,
        dir: &Path,
        listing: &Listing,
        name: &str,
        path: &Path,
        context: &Context,
    ) -> Option<Candidate> {
        if dir == self.search.home {
            return None;
        }
        let (rule, artifact) = rules::find_artifact(listing, name)?;
        let unit = context.unit.as_ref()?;
        let proven = match artifact.proof {
            Proof::None => true,
            Proof::Inside(file) => path.join(file).is_file(),
            Proof::Sibling(entry) => listing.has(entry),
        };
        if !proven {
            return None;
        }
        let regen = if artifact.locks.is_empty() {
            artifact.regen
        } else {
            // A member of a workspace is restored from the lockfile at the workspace root.
            dir.ancestors()
                .take_while(|d| d.starts_with(unit))
                .find_map(|d| {
                    artifact
                        .locks
                        .iter()
                        .find(|(lock, _)| d.join(lock).is_file())
                        .map(|(_, command)| *command)
                })?
        };
        Some(Candidate {
            kind: rule.kind,
            path: path.to_path_buf(),
            project: dir.to_path_buf(),
            unit: unit.clone(),
            git_root: context.git_root.clone(),
            regen: regen.to_string(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing in the unit was used for long enough: safe to delete.
    Unused,
    /// A file in the unit was read or changed recently.
    InUse,
    /// A running process is working inside the unit.
    Running,
    /// The directory holds files tracked by git, so it is not purely generated.
    Tracked,
    /// The unit is in a git repository but git could not tell us what is tracked.
    Unverified,
}

#[derive(Clone, Debug)]
pub struct ProjectItem {
    pub candidate: Candidate,
    /// Size of the directory; absent when the unit was found in use before measuring.
    pub usage: Option<Usage>,
    /// Latest use found in the unit (a lower bound when the search stopped early).
    pub last_use: i64,
    pub idle_days: u64,
    pub verdict: Verdict,
}

pub struct Judge<'a> {
    pub now: i64,
    /// Days without use after which a project kind counts as unused.
    pub days: &'a (dyn Fn(&str) -> u32 + Sync),
    pub count_reads: bool,
    /// Measure every candidate, even in units already known to be in use.
    pub measure_all: bool,
    /// Working directories of running processes.
    pub cwds: &'a [PathBuf],
}

/// Measure the candidates and decide which are unused.
pub fn evaluate(candidates: Vec<Candidate>, judge: &Judge) -> Vec<ProjectItem> {
    let artifacts: HashSet<PathBuf> = candidates.iter().map(|c| c.path.clone()).collect();
    let mut units: BTreeMap<PathBuf, Vec<Candidate>> = BTreeMap::new();
    for candidate in candidates {
        units
            .entry(candidate.unit.clone())
            .or_default()
            .push(candidate);
    }
    units
        .into_par_iter()
        .flat_map_iter(|(unit, candidates)| judge.unit(&unit, candidates, &artifacts))
        .collect()
}

impl Judge<'_> {
    fn unit(
        &self,
        unit: &Path,
        candidates: Vec<Candidate>,
        artifacts: &HashSet<PathBuf>,
    ) -> Vec<ProjectItem> {
        let measure = Measure {
            count_reads: self.count_reads,
            shared_times: SharedTimes::Ignore,
        };
        let sizes = |candidates: &[Candidate]| -> Vec<Usage> {
            candidates
                .par_iter()
                .map(|c| fsmeta::measure_tree(&c.path, measure))
                .collect()
        };
        let item =
            |candidate: Candidate, usage: Option<Usage>, last_use: i64, verdict| ProjectItem {
                candidate,
                usage,
                last_use,
                idle_days: ((self.now - last_use).max(0) / DAY) as u64,
                verdict,
            };
        let all = |candidates: Vec<Candidate>, last_use: i64, verdict: Verdict| {
            let usages = self.measure_all.then(|| sizes(&candidates));
            candidates
                .into_iter()
                .enumerate()
                .map(|(i, c)| item(c, usages.as_ref().map(|u| u[i]), last_use, verdict))
                .collect::<Vec<_>>()
        };

        if self.cwds.iter().any(|cwd| cwd.starts_with(unit)) {
            return all(candidates, self.now, Verdict::Running);
        }

        // A use after this moment means no artifact of the unit can be unused yet.
        let shortest = candidates
            .iter()
            .map(|c| (self.days)(c.kind))
            .min()
            .unwrap_or(0);
        let cutoff = self.now - i64::from(shortest) * DAY;
        let skip =
            |path: &Path| artifacts.contains(path) || path.file_name().is_some_and(|n| n == ".git");
        let stop_at = if self.measure_all {
            i64::MAX
        } else {
            cutoff + 1
        };
        // Outside the artifacts only changes count. Searches, indexers and coding agents
        // read the sources of projects nobody works on; running or building a project reads
        // what was installed or built for it, and that is measured below.
        let changes = Measure {
            count_reads: false,
            ..measure
        };
        let mut last_use = fsmeta::newest_use(unit, changes, &skip, stop_at);
        if candidates[0].git_root.as_deref() == Some(unit) {
            last_use = last_use.max(git_activity(unit));
        }
        if !self.measure_all && last_use > cutoff {
            return all(candidates, last_use, Verdict::InUse);
        }

        let usages = sizes(&candidates);
        let last_use = usages
            .iter()
            .fold(last_use, |latest, u| latest.max(u.last_use));
        candidates
            .into_iter()
            .zip(usages)
            .map(|(candidate, usage)| {
                let limit = i64::from((self.days)(candidate.kind)) * DAY;
                let verdict = if self.now - last_use < limit {
                    Verdict::InUse
                } else {
                    match &candidate.git_root {
                        None => Verdict::Unused,
                        Some(root) => match tracked_by_git(root, &candidate.path) {
                            Some(false) => Verdict::Unused,
                            Some(true) => Verdict::Tracked,
                            None => Verdict::Unverified,
                        },
                    }
                };
                item(candidate, Some(usage), last_use, verdict)
            })
            .collect()
    }
}

/// Latest sign that someone worked with the repository: a commit, checkout, pull or fetch.
fn git_activity(repo: &Path) -> i64 {
    ["HEAD", "index", "logs/HEAD", "ORIG_HEAD", "FETCH_HEAD"]
        .iter()
        .filter_map(|file| fs::metadata(repo.join(".git").join(file)).ok())
        .map(|md| md.mtime())
        .max()
        .unwrap_or(0)
}

/// Whether git tracks any file under `path`. `None` when git cannot say.
fn tracked_by_git(repo: &Path, path: &Path) -> Option<bool> {
    let relative = path.strip_prefix(repo).ok()?;
    let mut child = Command::new("git")
        // A repository's own configuration can name a program for git to run
        // (core.fsmonitor). A directory found on disk does not get to run anything.
        .args(["-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-z", "--"])
        .arg(relative)
        // Never refresh the index (that would itself look like activity) and never fall
        // back to a repository further up.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CEILING_DIRECTORIES", repo.parent()?)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut first = [0u8; 1];
    let any = child.stdout.take()?.read(&mut first).unwrap_or(0) > 0;
    if any {
        let _ = child.kill();
        let _ = child.wait();
        return Some(true);
    }
    child.wait().ok()?.success().then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsmeta::testutil::{DAY, now, set_times, write};
    use crate::remove::TRASH_PREFIX;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;

    struct Home {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Home {
        fn new() -> Home {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().canonicalize().unwrap().join("home");
            fs::create_dir_all(&path).unwrap();
            Home { _dir: dir, path }
        }

        /// Create files (paths relative to the home directory).
        fn files(&self, paths: &[&str]) -> &Home {
            for p in paths {
                write(&self.path.join(p), 2000);
            }
            self
        }

        fn discover(&self) -> Found {
            self.discover_from(&[""], &[])
        }

        fn discover_from(&self, roots: &[&str], ignore: &[&str]) -> Found {
            let roots: Vec<PathBuf> = roots.iter().map(|r| self.path.join(r)).collect();
            let ignore: Vec<PathBuf> = ignore.iter().map(|r| self.path.join(r)).collect();
            let mut found = discover(
                &roots,
                &Search {
                    home: &self.path,
                    ignore: &ignore,
                    cross_filesystems: false,
                },
            );
            found.candidates.sort_by(|a, b| a.path.cmp(&b.path));
            found
        }

        /// Candidate paths relative to home.
        fn found(&self) -> Vec<String> {
            self.rel(&self.discover().candidates)
        }

        fn rel(&self, candidates: &[Candidate]) -> Vec<String> {
            candidates
                .iter()
                .map(|c| {
                    c.path
                        .strip_prefix(&self.path)
                        .unwrap()
                        .display()
                        .to_string()
                })
                .collect()
        }

        fn unit_of(&self, artifact: &str) -> String {
            let found = self.discover();
            let c = found
                .candidates
                .iter()
                .find(|c| c.path == self.path.join(artifact))
                .unwrap_or_else(|| panic!("{artifact} not found"));
            c.unit
                .strip_prefix(&self.path)
                .unwrap()
                .display()
                .to_string()
        }

        fn git(&self, dir: &str, args: &[&str]) {
            let ok = Command::new("git")
                .arg("-C")
                .arg(self.path.join(dir))
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        }
    }

    const NODE: [&str; 3] = [
        "package.json",
        "package-lock.json",
        "node_modules/x/index.js",
    ];

    fn node(dir: &str) -> Vec<String> {
        NODE.iter().map(|f| format!("{dir}/{f}")).collect()
    }

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn finds_dependencies_that_a_lockfile_can_restore() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));

        let found = home.discover();

        assert_eq!(home.rel(&found.candidates), ["code/app/node_modules"]);
        let c = &found.candidates[0];
        assert_eq!(c.kind, "node");
        assert_eq!(c.regen, "npm ci");
        assert_eq!(c.project, home.path.join("code/app"));
    }

    #[test]
    fn dependencies_without_a_lockfile_are_left_alone() {
        let home = Home::new();
        home.files(&["code/app/package.json", "code/app/node_modules/x/index.js"]);

        assert_eq!(home.found(), Vec::<String>::new());
    }

    #[test]
    fn a_workspace_lockfile_covers_its_members() {
        let home = Home::new();
        home.files(&[
            "mono/package.json",
            "mono/pnpm-lock.yaml",
            "mono/packages/web/package.json",
            "mono/packages/web/node_modules/x/index.js",
        ]);

        let found = home.discover();

        assert_eq!(
            home.rel(&found.candidates),
            ["mono/packages/web/node_modules"]
        );
        assert_eq!(found.candidates[0].regen, "pnpm install");
        assert_eq!(found.candidates[0].unit, home.path.join("mono"));
    }

    #[test]
    fn the_home_directory_is_never_a_project() {
        let home = Home::new();
        home.files(&NODE);

        assert_eq!(home.found(), Vec::<String>::new());
    }

    #[test]
    fn installed_code_is_not_searched_for_projects() {
        let home = Home::new();
        // A package that ships its own lockfile and dependencies, an extracted app inside a
        // virtualenv, and a project under a hidden directory.
        home.files(&strs(&node("code/app/node_modules/dep")));
        home.files(&["code/app/package.json", "code/tool/venv/pyvenv.cfg"]);
        home.files(&strs(&node("code/tool/venv/lib/site/app")));
        home.files(&strs(&node(".config/editor/extension")));

        assert_eq!(home.found(), Vec::<String>::new());
    }

    #[test]
    fn a_build_directory_needs_proof_of_what_it_is() {
        let home = Home::new();
        home.files(&[
            "code/real/Cargo.toml",
            "code/real/target/CACHEDIR.TAG",
            "code/real/target/debug/app",
            "code/odd/Cargo.toml",
            "code/odd/target/notes.txt",
            "code/py/requirements.txt",
            "code/py/env/secrets.txt",
        ]);

        assert_eq!(home.found(), ["code/real/target"]);
    }

    #[test]
    fn a_symlinked_directory_is_not_a_candidate() {
        let home = Home::new();
        home.files(&[
            "code/app/package.json",
            "code/app/package-lock.json",
            "store/x/index.js",
        ]);
        symlink(
            home.path.join("store"),
            home.path.join("code/app/node_modules"),
        )
        .unwrap();

        assert_eq!(home.found(), Vec::<String>::new());
    }

    #[test]
    fn the_unit_is_the_outermost_project_inside_the_repository() {
        let home = Home::new();
        home.files(&[
            "repo/.git/HEAD",
            "repo/package.json",
            "repo/package-lock.json",
        ]);
        home.files(&strs(&node("repo/apps/web")));

        assert_eq!(home.unit_of("repo/apps/web/node_modules"), "repo");
    }

    #[test]
    fn unrelated_projects_in_one_repository_are_separate_units() {
        let home = Home::new();
        home.files(&["repo/.git/HEAD", "repo/README.md"]);
        home.files(&strs(&node("repo/old/site")));
        home.files(&strs(&node("repo/new/site")));

        assert_eq!(home.unit_of("repo/old/site/node_modules"), "repo/old/site");
        assert_eq!(home.unit_of("repo/new/site/node_modules"), "repo/new/site");
    }

    #[test]
    fn a_stray_manifest_above_a_repository_does_not_join_projects_together() {
        let home = Home::new();
        home.files(&["code/package.json", "code/app/.git/HEAD"]);
        home.files(&strs(&node("code/app")));

        let found = home.discover();

        assert_eq!(found.candidates[0].unit, home.path.join("code/app"));
        assert_eq!(
            found.candidates[0].git_root,
            Some(home.path.join("code/app"))
        );
    }

    #[test]
    fn the_unit_is_found_even_when_the_search_starts_below_it() {
        let home = Home::new();
        home.files(&[
            "repo/.git/HEAD",
            "repo/package.json",
            "repo/package-lock.json",
        ]);
        home.files(&[
            "repo/apps/web/package.json",
            "repo/apps/web/node_modules/x/index.js",
        ]);

        let found = home.discover_from(&["repo/apps"], &[]);

        assert_eq!(home.rel(&found.candidates), ["repo/apps/web/node_modules"]);
        assert_eq!(found.candidates[0].unit, home.path.join("repo"));
        assert_eq!(found.candidates[0].git_root, Some(home.path.join("repo")));
    }

    #[test]
    fn ignored_and_opted_out_directories_are_skipped() {
        let home = Home::new();
        home.files(&strs(&node("code/keep")));
        home.files(&strs(&node("code/optout")));
        home.files(&strs(&node("code/normal")));
        home.files(&["code/optout/.reliefignore"]);

        let found = home.discover_from(&[""], &["code/keep"]);

        assert_eq!(home.rel(&found.candidates), ["code/normal/node_modules"]);
    }

    #[test]
    fn tombstones_of_an_interrupted_run_are_reported() {
        let home = Home::new();
        let tombstone = format!("code/app/{TRASH_PREFIX}node_modules.1-0/x/index.js");
        home.files(&["code/app/package.json", &tombstone]);

        let found = home.discover();

        assert_eq!(
            found.leftovers,
            [home
                .path
                .join(format!("code/app/{TRASH_PREFIX}node_modules.1-0"))]
        );
    }

    // --- deciding what is unused -------------------------------------------------------

    /// Judge 100 days from now, so files created by the test look 100 days old.
    fn judge_after_100_days<'a>(
        home: &Home,
        edit: impl FnOnce(&mut Judge<'a>),
    ) -> Vec<ProjectItem> {
        const THIRTY: &(dyn Fn(&str) -> u32 + Sync) = &|_| 30;
        let candidates = home.discover().candidates;
        let mut judge = Judge {
            now: now() + 100 * DAY,
            days: THIRTY,
            count_reads: true,
            measure_all: false,
            cwds: &[],
        };
        edit(&mut judge);
        let mut items = evaluate(candidates, &judge);
        items.sort_by(|a, b| a.candidate.path.cmp(&b.candidate.path));
        items
    }

    fn touch_in_95_days(home: &Home, file: &str) {
        let t = now() + 95 * DAY;
        set_times(&home.path.join(file), t, t);
    }

    #[test]
    fn a_project_nobody_touched_is_unused() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].verdict, Verdict::Unused);
        assert!(
            (99..=100).contains(&items[0].idle_days),
            "{}",
            items[0].idle_days
        );
        assert!(items[0].usage.unwrap().bytes >= 2000);
    }

    #[test]
    fn editing_a_source_file_keeps_the_project_in_use() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        home.files(&["code/app/src/main.js"]);
        touch_in_95_days(&home, "code/app/src/main.js");

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::InUse);
        assert!(
            (4..=5).contains(&items[0].idle_days),
            "{}",
            items[0].idle_days
        );
    }

    #[test]
    fn using_the_build_output_keeps_the_project_in_use() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        touch_in_95_days(&home, "code/app/node_modules/x/index.js");

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::InUse);
    }

    #[test]
    fn activity_anywhere_in_the_unit_keeps_every_artifact_of_it() {
        let home = Home::new();
        home.files(&["mono/package.json", "mono/package-lock.json"]);
        home.files(&["mono/a/package.json", "mono/a/node_modules/x/index.js"]);
        home.files(&[
            "mono/b/package.json",
            "mono/b/node_modules/x/index.js",
            "mono/b/src/i.js",
        ]);
        touch_in_95_days(&home, "mono/b/src/i.js");

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.verdict == Verdict::InUse));
    }

    #[test]
    fn an_idle_project_next_to_a_busy_one_is_still_unused() {
        let home = Home::new();
        home.files(&strs(&node("code/old")));
        home.files(&strs(&node("code/busy")));
        touch_in_95_days(&home, "code/busy/package.json");

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].candidate.project, home.path.join("code/busy"));
        assert_eq!(items[0].verdict, Verdict::InUse);
        assert_eq!(items[1].verdict, Verdict::Unused);
    }

    #[test]
    fn each_kind_waits_its_own_number_of_days() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        home.files(&["code/app/Cargo.toml", "code/app/target/CACHEDIR.TAG"]);
        let days = |kind: &str| if kind == "rust" { 120 } else { 30 };

        let items = judge_after_100_days(&home, |j| j.days = &days);

        assert_eq!(items[0].candidate.kind, "node");
        assert_eq!(items[0].verdict, Verdict::Unused);
        assert_eq!(items[1].candidate.kind, "rust");
        assert_eq!(items[1].verdict, Verdict::InUse);
    }

    #[test]
    fn reading_source_files_does_not_keep_a_project() {
        // Searches, indexers and coding agents read sources of projects nobody works on.
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        home.files(&["code/app/src/main.js"]);
        let read = now() + 95 * DAY;
        set_times(&home.path.join("code/app/src/main.js"), read, 1_000_000);

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::Unused);
    }

    #[test]
    fn reading_the_dependencies_keeps_a_project_only_when_reads_count() {
        // Running or building a project reads what is installed for it.
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        let read = now() + 95 * DAY;
        set_times(
            &home.path.join("code/app/node_modules/x/index.js"),
            read,
            1_000_000,
        );

        let counting = judge_after_100_days(&home, |_| {});
        let ignoring = judge_after_100_days(&home, |j| j.count_reads = false);

        assert_eq!(counting[0].verdict, Verdict::InUse);
        assert_eq!(ignoring[0].verdict, Verdict::Unused);
    }

    #[test]
    fn a_process_working_in_the_project_keeps_it() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        home.files(&strs(&node("code/other")));
        let cwds = [home.path.join("code/app/src")];

        let items = judge_after_100_days(&home, |j| j.cwds = &cwds);

        assert_eq!(items[0].verdict, Verdict::Running);
        assert_eq!(items[1].verdict, Verdict::Unused);
    }

    #[test]
    fn git_activity_keeps_the_repository_in_use() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        home.files(&["code/app/.git/HEAD", "code/app/.git/index"]);
        touch_in_95_days(&home, "code/app/.git/index");

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::InUse);
    }

    #[test]
    fn git_activity_elsewhere_in_a_shared_repository_says_nothing_about_a_project() {
        let home = Home::new();
        home.files(&["repo/README.md"]);
        home.files(&strs(&node("repo/old")));
        home.git("repo", &["init", "-q"]);
        touch_in_95_days(&home, "repo/.git/HEAD");

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::Unused);
    }

    #[test]
    fn a_directory_with_files_tracked_by_git_is_kept() {
        let home = Home::new();
        home.files(&strs(&node("code/committed")));
        home.files(&strs(&node("code/ignored")));
        for repo in ["code/committed", "code/ignored"] {
            home.git(repo, &["init", "-q"]);
        }
        home.git("code/committed", &["add", "-f", "node_modules/x/index.js"]);

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::Tracked);
        assert_eq!(items[1].verdict, Verdict::Unused);
    }

    #[test]
    fn a_repository_git_cannot_read_is_not_touched() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        home.files(&["code/app/.git/garbage"]);

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::Unverified);
    }

    #[test]
    fn a_repository_cannot_make_git_run_a_program_of_its_own() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        home.git("code/app", &["init", "-q"]);
        let mark = home.path.join("ran");
        let program = crate::proc::testutil::script(
            &home.path.join("hooks"),
            "monitor",
            &format!("touch {}", mark.display()),
        );
        home.git(
            "code/app",
            &["config", "core.fsmonitor", &program.display().to_string()],
        );

        let items = judge_after_100_days(&home, |_| {});

        assert_eq!(items[0].verdict, Verdict::Unused);
        assert!(!mark.exists(), "git ran the program the repository named");
    }

    #[test]
    fn a_project_in_use_is_only_measured_on_request() {
        let home = Home::new();
        home.files(&strs(&node("code/app")));
        touch_in_95_days(&home, "code/app/package.json");

        let quick = judge_after_100_days(&home, |_| {});
        let full = judge_after_100_days(&home, |j| j.measure_all = true);

        assert_eq!(quick[0].usage, None);
        assert!(full[0].usage.unwrap().bytes >= 2000);
        assert_eq!(full[0].verdict, Verdict::InUse);
    }
}
