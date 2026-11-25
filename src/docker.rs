//! Docker: build cache, dangling images and images no container uses.
//!
//! Everything goes through the docker command, never through Docker's files, and nothing is
//! ever forced: an image a container still needs makes `docker image rm` fail, and that
//! failure is reported rather than overridden. Volumes hold data, so they are never deleted;
//! relief only says how much space unused ones take.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::DateTime;
use serde_json::Value;

/// The three kinds, for configuration.
pub const KINDS: &[&str] = &["build-cache", "dangling-images", "images"];

/// Something docker can delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub kind: &'static str,
    /// What it is, for people: `postgres:16`, `3 dangling images`, `build cache`.
    pub name: String,
    /// Space it frees; `None` when docker cannot say in advance.
    pub bytes: Option<u64>,
    /// Days since it was created or pulled, where docker records that.
    pub days: Option<u64>,
    /// The docker commands that delete it, one argument list each.
    pub commands: Vec<Vec<String>>,
    /// How to get it back.
    pub regen: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
}

pub struct Scan<'a> {
    pub docker: PathBuf,
    pub now: i64,
    /// Days after which a kind counts as unused; `None` skips the kind.
    pub days: &'a (dyn Fn(&str) -> Option<u32> + Sync),
}

/// Parse a size as docker prints it (`297.2MB`, `0B`, `1.5kB`), in decimal units.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.split_whitespace().next()?;
    let split = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = text.split_at(split);
    let factor: f64 = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    let value: f64 = number.parse().ok()?;
    Some((value * factor).round() as u64)
}

const ASK: Duration = Duration::from_secs(30);
const DAY: i64 = 86_400;

fn lines(docker: &Path, args: &[&str]) -> Option<Vec<String>> {
    let out = crate::proc::output(docker, args, ASK).unwrap_or_default();
    Some(
        out.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn json_lines(docker: &Path, args: &[&str]) -> Vec<Value> {
    lines(docker, args)
        .unwrap_or_default()
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn timestamp(value: &Value) -> Option<i64> {
    let time = DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()?
        .timestamp();
    // Docker writes year 1 for "never".
    (time > 0).then_some(time)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn command(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Ask docker what can go. `None` when docker is not there or its daemon does not answer.
pub fn scan(scan: &Scan) -> Option<Report> {
    let docker = scan.docker.as_path();
    crate::proc::output(docker, &["info", "--format", "{{.ServerVersion}}"], ASK)?;
    let mut report = Report::default();
    let age = |time: i64| ((scan.now - time).max(0) / DAY) as u64;

    let verbose: Value = crate::proc::output(
        docker,
        &["system", "df", "--verbose", "--format", "{{json .}}"],
        ASK,
    )
    .and_then(|text| serde_json::from_str(&text).ok())
    .unwrap_or(Value::Null);

    if (scan.days)("images").is_some() || (scan.days)("dangling-images").is_some() {
        let unique: HashMap<String, u64> = verbose["Images"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                Some((
                    i["ID"].as_str()?.to_string(),
                    parse_size(i["UniqueSize"].as_str()?)?,
                ))
            })
            .collect();
        let containers = lines(
            docker,
            &["container", "ls", "--all", "--no-trunc", "--quiet"],
        )
        .unwrap_or_default();
        let in_use: HashSet<String> = if containers.is_empty() {
            HashSet::new()
        } else {
            let mut args = vec!["container", "inspect", "--format", "{{.Image}}"];
            args.extend(containers.iter().map(String::as_str));
            // If docker cannot say which images containers use, keep every image.
            match lines(docker, &args) {
                Some(used) if used.len() == containers.len() => used.into_iter().collect(),
                _ => return Some(report),
            }
        };
        let mut ids =
            lines(docker, &["image", "ls", "--all", "--no-trunc", "--quiet"]).unwrap_or_default();
        ids.sort();
        ids.dedup();
        let images = if ids.is_empty() {
            Vec::new()
        } else {
            let mut args = vec!["image", "inspect", "--format", "{{json .}}"];
            args.extend(ids.iter().map(String::as_str));
            json_lines(docker, &args)
        };

        let mut dangling: Vec<(String, u64)> = Vec::new();
        for image in &images {
            let Some(id) = image["Id"].as_str() else {
                continue;
            };
            if in_use.contains(id) {
                continue;
            }
            let tags = strings(&image["RepoTags"]);
            let digests = strings(&image["RepoDigests"]);
            let created = timestamp(&image["Created"]).unwrap_or(scan.now);
            let last = timestamp(&image["Metadata"]["LastTagTime"])
                .unwrap_or(created)
                .max(created);
            let bytes = unique.get(id).copied();
            if tags.is_empty() {
                if let Some(days) = (scan.days)("dangling-images")
                    && age(created) >= u64::from(days)
                    // An untagged image with a digest is a pull without a name; still waste.
                    && image["Parent"].as_str().is_none_or(str::is_empty)
                {
                    dangling.push((id.to_string(), bytes.unwrap_or(0)));
                }
                continue;
            }
            // Only what a registry can give back: a locally built image may have no way back.
            let Some(days) = (scan.days)("images") else {
                continue;
            };
            if digests.is_empty() || age(last) < u64::from(days) {
                continue;
            }
            report.findings.push(Finding {
                kind: "images",
                name: tags.join(", "),
                bytes,
                days: Some(age(last)),
                commands: tags.iter().map(|t| command(&["image", "rm", t])).collect(),
                regen: tags
                    .iter()
                    .map(|t| format!("docker pull {t}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            });
        }
        if !dangling.is_empty() {
            report.findings.push(Finding {
                kind: "dangling-images",
                name: format!("{} dangling images", dangling.len()),
                bytes: Some(dangling.iter().map(|(_, b)| b).sum()),
                days: None,
                commands: dangling
                    .iter()
                    .map(|(id, _)| command(&["image", "rm", id]))
                    .collect(),
                regen: "nothing refers to them; rebuilt only if a Dockerfile produces them again"
                    .to_string(),
            });
        }
    }

    if let Some(days) = (scan.days)("build-cache") {
        let totals = json_lines(docker, &["system", "df", "--format", "{{json .}}"]);
        let reclaimable = totals
            .iter()
            .find(|t| t["Type"].as_str() == Some("Build Cache"))
            .and_then(|t| parse_size(t["Reclaimable"].as_str()?));
        if reclaimable.is_some_and(|b| b > 0) {
            let hours = format!("until={}h", u64::from(days) * 24);
            report.findings.push(Finding {
                kind: "build-cache",
                name: "build cache".to_string(),
                bytes: None,
                days: None,
                commands: vec![command(&[
                    "builder", "prune", "--force", "--filter", &hours,
                ])],
                regen: "rebuilt by the next docker build".to_string(),
            });
        }
    }

    let unused: Vec<u64> = verbose["Volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["Links"].as_str() == Some("0") || v["Links"].as_u64() == Some(0))
        .filter_map(|v| parse_size(v["Size"].as_str()?))
        .collect();
    let bytes: u64 = unused.iter().sum();
    if bytes > 0 {
        report.notes.push(format!(
            "{} docker volumes no container uses hold {}. Volumes can hold data, so relief never \
             deletes them; review them with `docker volume ls --filter dangling=true`.",
            unused.len(),
            crate::render::bytes(bytes)
        ));
    }

    let rank = |f: &Finding| KINDS.iter().position(|k| *k == f.kind).unwrap_or(9);
    report
        .findings
        .sort_by(|a, b| (rank(a), b.bytes).cmp(&(rank(b), a.bytes)));
    Some(report)
}

#[cfg(test)]
#[path = "docker_tests.rs"]
mod tests;
