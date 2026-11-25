//! Package-manager caches: which entries have gone unused and how to drop them safely.
//!
//! Three ways of shrinking a cache, chosen per tool by what its layout tolerates:
//! - entry by entry, where entries are independent (Go, yarn, npx, Playwright);
//! - all or nothing, once the whole cache has gone unused (pip, bun, npm's content store);
//! - the tool's own prune command, where the tool forbids touching its files (uv, pnpm).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rayon::prelude::*;

use crate::env::Env;
use crate::fsmeta::{self, Measure, SharedTimes, Usage};
use crate::remove::is_trash;

const DAY: i64 = 86_400;
/// How long a tool may take to say where its cache is.
const ASK: Duration = Duration::from_secs(10);

/// Every cache kind, for validating configuration.
pub const KINDS: &[&str] = &[
    "go-build",
    "go-mod",
    "npm",
    "yarn",
    "pnpm",
    "uv",
    "pip",
    "bun",
    "playwright",
    "cargo",
    "cypress",
    "puppeteer",
    "node-gyp",
    "prisma",
    "electron",
    "huggingface",
    "composer",
    "deno",
];

/// The tool's own way of dropping what it no longer needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prune {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Space it is expected to free, when that can be known in advance.
    pub estimate: Option<u64>,
}

/// One cache found on this machine.
#[derive(Clone, Debug)]
pub struct Finding {
    pub kind: &'static str,
    pub root: PathBuf,
    /// Size of the whole cache.
    pub total: Usage,
    /// Entries unused for long enough to delete.
    pub stale: Vec<PathBuf>,
    pub stale_bytes: u64,
    pub prune: Option<Prune>,
    /// What happens when a deleted entry is needed again.
    pub regen: &'static str,
}

pub struct Scan<'a> {
    pub env: &'a Env,
    /// Days without use after which an entry of this kind is stale; `None` skips the kind.
    pub days: &'a (dyn Fn(&str) -> Option<u32> + Sync),
    pub count_reads: bool,
}

/// How one part of a cache may be shrunk.
enum Part {
    /// Independent entries: each goes when it has not been used for long enough.
    Each(Vec<PathBuf>),
    /// Everything inside goes together, and only once none of it is in use.
    Whole(PathBuf),
}

struct Layout {
    root: PathBuf,
    parts: Vec<Part>,
    prune: Option<Prune>,
    regen: &'static str,
}

type Locate = fn(&Env) -> Option<Layout>;

const LOCATORS: &[(&str, Locate)] = &[
    ("go-build", go_build),
    ("go-mod", go_mod),
    ("npm", npm),
    ("yarn", yarn),
    ("pnpm", pnpm),
    ("uv", uv),
    ("pip", pip),
    ("bun", bun),
    ("playwright", playwright),
    ("cargo", cargo),
    ("cypress", cypress),
    ("puppeteer", puppeteer),
    ("node-gyp", node_gyp),
    ("prisma", prisma),
    ("electron", electron),
    ("huggingface", huggingface),
    ("composer", composer),
    ("deno", deno),
];

/// Find the caches on this machine and what has gone unused in each.
pub fn scan(scan: &Scan) -> Vec<Finding> {
    LOCATORS
        .par_iter()
        .filter_map(|(kind, locate)| {
            let days = (scan.days)(kind)?;
            let layout = locate(scan.env)?;
            Some(judge(
                kind,
                layout,
                scan,
                scan.env.now - i64::from(days) * DAY,
            ))
        })
        .collect()
}

fn judge(kind: &'static str, layout: Layout, scan: &Scan, cutoff: i64) -> Finding {
    let measure = Measure {
        count_reads: scan.count_reads,
        // A file a project links to is in use, whoever touched it.
        shared_times: SharedTimes::Count,
    };
    let unused = |usage: &Usage| usage.files > 0 && usage.last_use <= cutoff;
    let mut stale = Vec::new();
    let mut stale_bytes = 0;
    let mut emptied = false;
    for part in layout.parts {
        match part {
            Part::Each(entries) => {
                let gone: Vec<(PathBuf, u64)> = entries
                    .into_par_iter()
                    .filter_map(|entry| {
                        let usage = fsmeta::measure_tree(&entry, measure);
                        let leftover = entry.file_name().is_some_and(is_trash);
                        (leftover || unused(&usage)).then_some((entry, usage.bytes))
                    })
                    .collect();
                for (entry, bytes) in gone {
                    stale.push(entry);
                    stale_bytes += bytes;
                }
            }
            Part::Whole(dir) => {
                let usage = fsmeta::measure_tree(&dir, measure);
                if unused(&usage) {
                    stale.extend(children(&dir).into_iter().map(|c| c.path));
                    stale_bytes += usage.bytes;
                    emptied = true;
                }
            }
        }
    }
    stale.sort();
    Finding {
        kind,
        total: fsmeta::measure_tree(&layout.root, measure),
        root: layout.root,
        stale,
        stale_bytes,
        // Nothing is left to prune in a cache that is being emptied.
        prune: if emptied { None } else { layout.prune },
        regen: layout.regen,
    }
}

struct Child {
    path: PathBuf,
    name: String,
    is_dir: bool,
    is_file: bool,
}

fn children(dir: &Path) -> Vec<Child> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .filter_map(|e| {
            let kind = e.file_type().ok()?;
            Some(Child {
                path: e.path(),
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: kind.is_dir(),
                is_file: kind.is_file(),
            })
        })
        .collect()
}

fn subdirs(dir: &Path) -> impl Iterator<Item = Child> {
    children(dir).into_iter().filter(|c| c.is_dir)
}

/// The canonical form of the first candidate that is a directory.
fn existing(candidates: impl IntoIterator<Item = Option<PathBuf>>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .flatten()
        .find(|dir| dir.is_dir())
        .and_then(|dir| dir.canonicalize().ok())
}

/// Ask a tool on PATH for a directory. The tool is started in the home directory, so that
/// the configuration of a project relief happens to be started in (an `.npmrc`, a `uv.toml`)
/// cannot point it at a directory of its choosing.
fn ask(env: &Env, program: &str, args: &[&str]) -> Option<PathBuf> {
    let answer = crate::proc::output_in(&env.home, &env.which(program)?, args, ASK)?;
    Some(PathBuf::from(answer)).filter(|p| p.is_absolute())
}

fn var(env: &Env, key: &str) -> Option<PathBuf> {
    env.var(key).map(PathBuf::from).filter(|p| p.is_absolute())
}

const REFETCH: &str = "downloaded again when needed";

fn go_build(env: &Env) -> Option<Layout> {
    let root = existing([
        ask(env, "go", &["env", "GOCACHE"]),
        var(env, "GOCACHE"),
        Some(env.cache_home().join("go-build")),
    ])?;
    let hex = |name: &str| name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit());
    let buckets: Vec<Child> = subdirs(&root).filter(|c| hex(&c.name)).collect();
    if buckets.is_empty() {
        return None;
    }
    let entries = buckets
        .iter()
        .flat_map(|bucket| children(&bucket.path))
        .filter(|c| c.is_file)
        .map(|c| c.path)
        .collect();
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: "rebuilt by the next go build",
    })
}

fn go_mod(env: &Env) -> Option<Layout> {
    let gopath = env
        .var("GOPATH")
        .and_then(|paths| std::env::split_paths(paths).next())
        .unwrap_or_else(|| env.home.join("go"));
    let root = existing([
        ask(env, "go", &["env", "GOMODCACHE"]),
        var(env, "GOMODCACHE"),
        Some(gopath.join("pkg/mod")),
    ])?;
    let download = root.join("cache/download");
    if !download.is_dir() {
        return None;
    }
    let mut entries = Vec::new();
    // Extracted modules: github.com/user/repo@v1.2.3, at whatever depth the path has.
    fn modules(dir: &Path, depth: u32, entries: &mut Vec<PathBuf>) {
        for child in subdirs(dir) {
            if child.name.contains('@') {
                entries.push(child.path);
            } else if depth < 12 {
                modules(&child.path, depth + 1, entries);
            }
        }
    }
    for top in subdirs(&root).filter(|c| c.name != "cache") {
        if top.name.contains('@') {
            entries.push(top.path);
        } else {
            modules(&top.path, 0, &mut entries);
        }
    }
    // Downloaded archives: cache/download/<module>/@v/<version>.zip and friends.
    fn downloads(dir: &Path, depth: u32, entries: &mut Vec<PathBuf>) {
        for child in subdirs(dir) {
            if child.name == "@v" {
                entries.extend(
                    children(&child.path)
                        .into_iter()
                        .filter(|c| c.is_file)
                        .map(|c| c.path),
                );
            } else if child.name != "sumdb" && depth < 12 {
                downloads(&child.path, depth + 1, entries);
            }
        }
    }
    downloads(&download, 0, &mut entries);
    entries.extend(subdirs(&root.join("cache/vcs")).map(|c| c.path));
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: REFETCH,
    })
}

/// The `cache=` setting of the user's npmrc.
fn npmrc_cache(env: &Env) -> Option<PathBuf> {
    let text = fs::read_to_string(env.home.join(".npmrc")).ok()?;
    let value = text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "cache").then(|| value.trim().trim_matches('"').to_string())
    })?;
    Some(env.expand(&value))
}

fn npm(env: &Env) -> Option<Layout> {
    let root = existing([
        var(env, "npm_config_cache"),
        var(env, "NPM_CONFIG_CACHE"),
        npmrc_cache(env),
        Some(env.home.join(".npm")),
    ])?;
    let (content, npx) = (root.join("_cacache"), root.join("_npx"));
    if !content.is_dir() && !npx.is_dir() {
        return None;
    }
    Some(Layout {
        parts: vec![
            Part::Each(subdirs(&npx).map(|c| c.path).collect()),
            // The index refers to the content, so single files are not removed from under it.
            Part::Whole(content),
        ],
        root,
        prune: None,
        regen: REFETCH,
    })
}

fn yarn(env: &Env) -> Option<Layout> {
    let root = existing([
        var(env, "YARN_CACHE_FOLDER"),
        Some(env.cache_home().join("yarn")),
    ])?;
    let version =
        |name: &str| name.starts_with('v') && name[1..].bytes().all(|b| b.is_ascii_digit());
    let versions: Vec<Child> = subdirs(&root).filter(|c| version(&c.name)).collect();
    if versions.is_empty() {
        return None;
    }
    let entries = versions
        .iter()
        .flat_map(|v| subdirs(&v.path))
        .filter(|c| !c.name.starts_with('.') || is_trash(c.name.as_ref()))
        .map(|c| c.path)
        .collect();
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: REFETCH,
    })
}

fn pip(env: &Env) -> Option<Layout> {
    let root = existing([
        var(env, "PIP_CACHE_DIR"),
        Some(env.cache_home().join("pip")),
    ])?;
    let known = ["http", "http-v2", "wheels", "selfcheck"];
    if !known.iter().any(|dir| root.join(dir).is_dir()) {
        return None;
    }
    Some(Layout {
        parts: vec![Part::Whole(root.clone())],
        root,
        prune: None,
        regen: REFETCH,
    })
}

fn bun(env: &Env) -> Option<Layout> {
    let install = var(env, "BUN_INSTALL").unwrap_or_else(|| env.home.join(".bun"));
    let root = existing([
        var(env, "BUN_INSTALL_CACHE_DIR"),
        Some(install.join("install/cache")),
    ])?;
    Some(Layout {
        parts: vec![Part::Whole(root.clone())],
        root,
        prune: None,
        regen: REFETCH,
    })
}

fn playwright(env: &Env) -> Option<Layout> {
    let root = existing([
        var(env, "PLAYWRIGHT_BROWSERS_PATH"),
        Some(env.cache_home().join("ms-playwright")),
    ])?;
    let entries = subdirs(&root)
        .filter(|c| !c.name.starts_with('.') || is_trash(c.name.as_ref()))
        .map(|c| c.path)
        .collect();
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: "npx playwright install",
    })
}

fn uv(env: &Env) -> Option<Layout> {
    let root = existing([
        ask(env, "uv", &["cache", "dir"]),
        var(env, "UV_CACHE_DIR"),
        Some(env.cache_home().join("uv")),
    ])?;
    if !root.join("CACHEDIR.TAG").is_file() {
        return None;
    }
    // uv's cache must not be edited by hand while it is in use; only uv prunes it.
    let prune = env.which("uv").map(|program| Prune {
        program,
        args: vec!["cache".to_string(), "prune".to_string()],
        estimate: None,
    });
    Some(Layout {
        parts: vec![Part::Whole(root.clone())],
        root,
        prune,
        regen: REFETCH,
    })
}

fn pnpm(env: &Env) -> Option<Layout> {
    let asked = ask(env, "pnpm", &["store", "path"]);
    let by_pnpm = asked.is_some();
    let root = existing([asked, Some(env.data_home().join("pnpm/store"))])?;
    let is_store = |dir: &Path| dir.join("files").is_dir();
    let looks_right = if by_pnpm {
        is_store(&root)
    } else {
        subdirs(&root).any(|version| is_store(&version.path))
    };
    if !looks_right {
        return None;
    }
    // pnpm prunes the files that no project links to; those are exactly the ones that
    // count as reclaimable here.
    let prune = env.which("pnpm").filter(|_| by_pnpm).and_then(|program| {
        let measure = Measure {
            count_reads: false,
            shared_times: SharedTimes::Count,
        };
        let orphans = fsmeta::measure_tree(&root.join("files"), measure);
        (orphans.files > 0 && orphans.bytes > 0).then(|| Prune {
            program,
            args: vec!["store".to_string(), "prune".to_string()],
            estimate: Some(orphans.bytes),
        })
    });
    Some(Layout {
        parts: vec![Part::Whole(root.clone())],
        root,
        prune,
        regen: REFETCH,
    })
}

/// Visible children (and leftovers) of a directory, as independent entries.
fn visible(dir: &Path, dirs_only: bool) -> Vec<PathBuf> {
    children(dir)
        .into_iter()
        .filter(|c| c.is_dir || (!dirs_only && c.is_file))
        .filter(|c| !c.name.starts_with('.') || is_trash(c.name.as_ref()))
        .map(|c| c.path)
        .collect()
}

/// A cache whose top-level entries are independent.
fn each(root: Option<PathBuf>, dirs_only: bool, regen: &'static str) -> Option<Layout> {
    let root = existing([root])?;
    let entries = visible(&root, dirs_only);
    if entries.is_empty() {
        return None;
    }
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen,
    })
}

fn cargo(env: &Env) -> Option<Layout> {
    let root = existing([var(env, "CARGO_HOME"), Some(env.home.join(".cargo"))])?;
    let registry = root.join("registry");
    let git = root.join("git");
    if !registry.is_dir() && !git.is_dir() {
        return None;
    }
    let mut entries = Vec::new();
    // registry/cache/<index>/<crate>.crate and registry/src/<index>/<crate>/; the index
    // itself is shared by every build and stays.
    for index in subdirs(&registry.join("cache")) {
        entries.extend(
            children(&index.path)
                .into_iter()
                .filter(|c| c.is_file)
                .map(|c| c.path),
        );
    }
    for index in subdirs(&registry.join("src")) {
        entries.extend(subdirs(&index.path).map(|c| c.path));
    }
    for part in ["checkouts", "db"] {
        entries.extend(subdirs(&git.join(part)).map(|c| c.path));
    }
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: "downloaded again by cargo when needed",
    })
}

fn cypress(env: &Env) -> Option<Layout> {
    let root = var(env, "CYPRESS_CACHE_FOLDER").unwrap_or_else(|| env.cache_home().join("Cypress"));
    each(Some(root), true, "npx cypress install")
}

fn puppeteer(env: &Env) -> Option<Layout> {
    let root = existing([
        var(env, "PUPPETEER_CACHE_DIR"),
        Some(env.cache_home().join("puppeteer")),
    ])?;
    // <browser>/<platform>-<version>
    let entries: Vec<PathBuf> = subdirs(&root)
        .flat_map(|browser| visible(&browser.path, true))
        .collect();
    if entries.is_empty() {
        return None;
    }
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: "npx puppeteer browsers install",
    })
}

fn node_gyp(env: &Env) -> Option<Layout> {
    let layout = each(
        Some(env.cache_home().join("node-gyp")),
        true,
        "downloaded again by the next native build",
    )?;
    // Entries are node versions with headers inside.
    let headers = |p: &PathBuf| p.join("include").is_dir() || p.file_name().is_some_and(is_trash);
    match &layout.parts[0] {
        Part::Each(entries) if entries.iter().all(headers) => Some(layout),
        _ => None,
    }
}

fn prisma(env: &Env) -> Option<Layout> {
    let root = existing([Some(env.cache_home().join("prisma"))])?;
    let entries: Vec<PathBuf> = subdirs(&root)
        .flat_map(|channel| visible(&channel.path, true))
        .collect();
    if entries.is_empty() {
        return None;
    }
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: "downloaded again by prisma when needed",
    })
}

fn electron(env: &Env) -> Option<Layout> {
    let layout = each(
        Some(env.cache_home().join("electron")),
        false,
        "downloaded again by the next electron install",
    )?;
    let named = |p: &PathBuf| {
        p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy();
            n.starts_with("electron")
                || n.starts_with("chromedriver")
                || n.starts_with("ffmpeg")
                || is_trash(n.as_ref().as_ref())
        }) || p.is_dir()
    };
    match &layout.parts[0] {
        Part::Each(entries) if entries.iter().all(named) => Some(layout),
        _ => None,
    }
}

fn huggingface(env: &Env) -> Option<Layout> {
    let home = var(env, "HF_HOME").unwrap_or_else(|| env.cache_home().join("huggingface"));
    let root = existing([var(env, "HF_HUB_CACHE"), Some(home.join("hub"))])?;
    let entries: Vec<PathBuf> = subdirs(&root)
        .filter(|c| {
            ["models--", "datasets--", "spaces--"]
                .iter()
                .any(|p| c.name.starts_with(p))
                || is_trash(c.name.as_ref())
        })
        .map(|c| c.path)
        .collect();
    if entries.is_empty() {
        return None;
    }
    Some(Layout {
        root,
        parts: vec![Part::Each(entries)],
        prune: None,
        regen: "downloaded again from the Hugging Face Hub",
    })
}

fn composer(env: &Env) -> Option<Layout> {
    let root = existing([
        var(env, "COMPOSER_CACHE_DIR"),
        Some(env.cache_home().join("composer")),
    ])?;
    if !["files", "repo", "vcs"]
        .iter()
        .any(|d| root.join(d).is_dir())
    {
        return None;
    }
    Some(Layout {
        parts: vec![Part::Whole(root.clone())],
        root,
        prune: None,
        regen: REFETCH,
    })
}

fn deno(env: &Env) -> Option<Layout> {
    let root = existing([var(env, "DENO_DIR"), Some(env.cache_home().join("deno"))])?;
    if !["deps", "remote", "npm", "gen"]
        .iter()
        .any(|d| root.join(d).is_dir())
    {
        return None;
    }
    Some(Layout {
        parts: vec![Part::Whole(root.clone())],
        root,
        prune: None,
        regen: REFETCH,
    })
}

#[cfg(test)]
#[path = "caches_tests.rs"]
mod tests;
