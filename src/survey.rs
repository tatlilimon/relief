//! One pass over the machine: everything that could be deleted, and why the rest is kept.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::caches::{self, Prune};
use crate::config::{Config, Mode};
use crate::docker;
use crate::env::Env;
use crate::fsmeta::{self, Measure, SharedTimes};
use crate::history;
use crate::projects::{self, Judge, Search, Verdict};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Only {
    Projects,
    Caches,
    Docker,
}

/// How an item is deleted.
#[derive(Clone, Debug)]
pub enum Action {
    /// Delete a directory inside a project.
    Tree { container: PathBuf },
    /// Delete stale entries of a cache whose whole size was `before`.
    Entries { paths: Vec<PathBuf>, before: u64 },
    /// Let the tool prune its own cache.
    Prune { prune: Prune, before: u64 },
    /// Finish deleting what an interrupted run left behind.
    Leftover,
    /// Run these docker commands, in order.
    Docker {
        program: PathBuf,
        commands: Vec<Vec<String>>,
    },
}

/// Something that can be deleted.
#[derive(Clone, Debug, Serialize)]
pub struct Item {
    /// `project`, `cache`, `prune`, `docker` or `leftover`.
    pub category: &'static str,
    pub kind: String,
    pub path: PathBuf,
    /// Space it frees; unknown for a prune the tool cannot estimate.
    pub bytes: Option<u64>,
    /// Days without use.
    pub days: Option<u64>,
    pub detail: String,
    /// How to get it back.
    pub regen: String,
    #[serde(serialize_with = "mode_name")]
    pub mode: Mode,
    #[serde(skip)]
    pub action: Action,
}

fn mode_name<S: serde::Serializer>(mode: &Mode, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match mode {
        Mode::Auto => "auto",
        Mode::Ask => "ask",
        Mode::Off => "off",
    })
}

/// Something that could be regenerated but is not offered for deletion.
#[derive(Clone, Debug, Serialize)]
pub struct Kept {
    pub kind: String,
    pub path: PathBuf,
    pub bytes: Option<u64>,
    pub days: u64,
    pub reason: &'static str,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub items: Vec<Item>,
    pub kept: Vec<Kept>,
    pub notes: Vec<String>,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Default)]
pub struct Request {
    /// Search only these directories for projects (and leave caches alone).
    pub paths: Vec<PathBuf>,
    pub only: Option<Only>,
    /// Also measure what is kept.
    pub all: bool,
    pub project_days: Option<u32>,
    pub cache_days: Option<u32>,
}

pub fn survey(env: &Env, config: &Config, request: &Request) -> Result<Report> {
    let mut report = Report::default();
    let limited = !request.paths.is_empty();
    let mut cache_roots = Vec::new();

    let wants = |only: Only| request.only.is_none_or(|o| o == only);

    if wants(Only::Docker)
        && !limited
        && let Some(program) = env.which("docker")
    {
        let days = |kind: &str| {
            let policy = config.docker(kind);
            (policy.mode != Mode::Off).then_some(policy.days)
        };
        let scan = docker::Scan {
            docker: program.clone(),
            now: env.now,
            days: &days,
        };
        if let Some(found) = docker::scan(&scan) {
            for finding in found.findings {
                report.items.push(Item {
                    category: "docker",
                    kind: finding.kind.to_string(),
                    path: PathBuf::from(format!("docker:{}", finding.name)),
                    bytes: finding.bytes,
                    days: finding.days,
                    detail: finding.name,
                    regen: finding.regen,
                    mode: config.docker(finding.kind).mode,
                    action: Action::Docker {
                        program: program.clone(),
                        commands: finding.commands,
                    },
                });
            }
            report.notes.extend(found.notes);
        }
    }

    if wants(Only::Caches) && !limited {
        let runs = history::load(&env.state_dir).unwrap_or_default();
        let days = |kind: &str| {
            let policy = config.cache(kind);
            (policy.mode != Mode::Off).then_some(request.cache_days.unwrap_or(policy.days))
        };
        let found = caches::scan(&caches::Scan {
            env,
            days: &days,
            count_reads: config.count_reads,
        });
        for cache in found {
            cache_roots.push(cache.root.clone());
            let policy = config.cache(cache.kind);
            let days = request.cache_days.unwrap_or(policy.days);
            if !cache.stale.is_empty() {
                report.items.push(Item {
                    category: "cache",
                    kind: cache.kind.to_string(),
                    path: cache.root.clone(),
                    bytes: Some(cache.stale_bytes),
                    days: None,
                    detail: format!("{} entries unused for {days}+ days", cache.stale.len()),
                    regen: cache.regen.to_string(),
                    mode: policy.mode,
                    action: Action::Entries {
                        paths: cache.stale,
                        before: cache.total.bytes,
                    },
                });
            }
            // Pruning again is pointless until the cache has been used since the last time.
            let pruned = history::last_prune(&runs, cache.kind).unwrap_or(i64::MIN);
            if let Some(prune) = cache.prune.filter(|_| cache.total.last_use > pruned) {
                let command = prune
                    .program
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                report.items.push(Item {
                    category: "prune",
                    kind: cache.kind.to_string(),
                    path: cache.root,
                    bytes: prune.estimate,
                    days: None,
                    detail: format!("`{command} {}`", prune.args.join(" ")),
                    regen: cache.regen.to_string(),
                    mode: policy.mode,
                    action: Action::Prune {
                        prune,
                        before: cache.total.bytes,
                    },
                });
            }
        }
    }

    if wants(Only::Projects) {
        let roots: Vec<PathBuf> = if limited {
            request
                .paths
                .iter()
                .map(|p| {
                    p.canonicalize()
                        .with_context(|| format!("cannot search {}", p.display()))
                })
                .collect::<Result<_>>()?
        } else {
            let roots = config.roots.iter().map(|r| env.expand(r));
            roots.filter_map(|r| r.canonicalize().ok()).collect()
        };
        let mut ignore: Vec<PathBuf> = config
            .ignore
            .iter()
            .map(|i| env.expand(i))
            .map(|i| i.canonicalize().unwrap_or(i))
            .collect();
        ignore.extend(cache_roots);
        let found = projects::discover(
            &roots,
            &Search {
                home: &env.home,
                ignore: &ignore,
                cross_filesystems: false,
            },
        );

        let measure = Measure {
            count_reads: config.count_reads,
            shared_times: SharedTimes::Ignore,
        };
        for path in found.leftovers {
            report.items.push(Item {
                category: "leftover",
                kind: "leftover".to_string(),
                bytes: Some(fsmeta::measure_tree(&path, measure).bytes),
                path,
                days: None,
                detail: "left behind by an interrupted run".to_string(),
                regen: String::new(),
                mode: Mode::Auto,
                action: Action::Leftover,
            });
        }

        let wanted: Vec<_> = found
            .candidates
            .into_iter()
            .filter(|c| config.project(c.kind).mode != Mode::Off)
            .collect();
        let days = |kind: &str| request.project_days.unwrap_or(config.project(kind).days);
        let cwds = fsmeta::process_cwds();
        let judged = projects::evaluate(
            wanted,
            &Judge {
                now: env.now,
                days: &days,
                count_reads: config.count_reads,
                measure_all: request.all,
                cwds: &cwds,
            },
        );
        for item in judged {
            let bytes = item.usage.map(|u| u.bytes);
            let reason = match item.verdict {
                Verdict::Unused if bytes == Some(0) => continue,
                Verdict::Unused => {
                    report.items.push(Item {
                        category: "project",
                        kind: item.candidate.kind.to_string(),
                        path: item.candidate.path,
                        bytes,
                        days: Some(item.idle_days),
                        detail: String::new(),
                        regen: item.candidate.regen,
                        mode: config.project(item.candidate.kind).mode,
                        action: Action::Tree {
                            container: item.candidate.unit,
                        },
                    });
                    continue;
                }
                Verdict::InUse => "in use",
                Verdict::Running => "in use by a running process",
                Verdict::Tracked => "holds files tracked by git",
                Verdict::Unverified => "git could not confirm it is untracked",
            };
            report.kept.push(Kept {
                kind: item.candidate.kind.to_string(),
                path: item.candidate.path,
                bytes,
                days: item.idle_days,
                reason,
            });
        }
        if config.count_reads
            && let Some(mount) = fsmeta::noatime_mount(&env.home)
        {
            report.notes.push(format!(
                "{} is mounted noatime, so reads leave no trace: only changes count as use.",
                mount.display()
            ));
        }
    }

    let rank = |item: &Item| match item.category {
        "leftover" => 0,
        "project" => 1,
        "cache" => 2,
        "prune" => 3,
        _ => 4,
    };
    report
        .items
        .sort_by(|a, b| (rank(a), b.bytes, &a.path).cmp(&(rank(b), a.bytes, &b.path)));
    report
        .kept
        .sort_by(|a, b| (b.bytes, &a.path).cmp(&(a.bytes, &b.path)));
    report.reclaimable_bytes = report.items.iter().filter_map(|i| i.bytes).sum();
    Ok(report)
}
