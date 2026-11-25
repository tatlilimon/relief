//! A record of what was deleted, so that nothing disappears without a trace.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One thing a run deleted, or tried to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// `project`, `cache`, `prune` or `leftover`.
    pub category: String,
    pub kind: String,
    pub path: String,
    pub bytes: u64,
    /// How to get it back.
    pub regen: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub time: i64,
    /// `manual` or `auto`.
    pub mode: String,
    pub freed: u64,
    pub items: Vec<Entry>,
}

/// Runs kept on disk; older ones are dropped.
pub const KEEP: usize = 500;

fn file(state_dir: &Path) -> PathBuf {
    state_dir.join("history.jsonl")
}

pub fn append(state_dir: &Path, run: &Run) -> Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    let path = file(state_dir);
    let mut runs = load(state_dir)?;
    // Rewrite only now and then, so that a normal run is a single appended line.
    if runs.len() >= KEEP + 100 {
        runs.drain(..runs.len() + 1 - KEEP);
        runs.push(run.clone());
        let mut text = String::new();
        for run in &runs {
            text.push_str(&serde_json::to_string(run)?);
            text.push('\n');
        }
        let fresh = path.with_extension("jsonl.new");
        fs::write(&fresh, text)?;
        fs::rename(&fresh, &path)?;
        return Ok(());
    }
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // A damaged last line may lack its newline; never glue a record onto it.
    let ends_clean = fs::read(&path).map(|b| b.is_empty() || b.ends_with(b"\n"))?;
    let lead = if ends_clean { "" } else { "\n" };
    writeln!(out, "{lead}{}", serde_json::to_string(run)?)?;
    Ok(())
}

/// Every recorded run, oldest first.
pub fn load(state_dir: &Path) -> Result<Vec<Run>> {
    let text = match fs::read_to_string(file(state_dir)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("cannot read the history"),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// When the cache of this kind was last pruned successfully.
pub fn last_prune(runs: &[Run], kind: &str) -> Option<i64> {
    runs.iter()
        .filter(|run| {
            run.items
                .iter()
                .any(|e| e.category == "prune" && e.kind == kind && e.error.is_none())
        })
        .map(|run| run.time)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(category: &str, kind: &str, error: Option<&str>) -> Entry {
        Entry {
            category: category.to_string(),
            kind: kind.to_string(),
            path: "/home/dev/x".to_string(),
            bytes: 10,
            regen: "npm ci".to_string(),
            error: error.map(str::to_string),
        }
    }

    fn run(time: i64, items: Vec<Entry>) -> Run {
        Run {
            time,
            mode: "auto".to_string(),
            freed: 10,
            items,
        }
    }

    #[test]
    fn runs_come_back_in_the_order_they_were_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("not/yet/there");
        let (first, second) = (run(1, vec![entry("project", "node", None)]), run(2, vec![]));

        append(&state, &first).unwrap();
        append(&state, &second).unwrap();

        assert_eq!(load(&state).unwrap(), [first, second]);
    }

    #[test]
    fn no_history_yet_is_an_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), []);
    }

    #[test]
    fn a_damaged_line_does_not_hide_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &run(1, vec![])).unwrap();
        let file = dir.path().join("history.jsonl");
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("{\"time\": 2, \"mo\n");
        std::fs::write(&file, text).unwrap();
        append(dir.path(), &run(3, vec![])).unwrap();

        let times: Vec<i64> = load(dir.path()).unwrap().iter().map(|r| r.time).collect();
        assert_eq!(times, [1, 3]);
    }

    #[test]
    fn only_the_most_recent_runs_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for time in 0..(KEEP as i64 + 150) {
            append(dir.path(), &run(time, vec![])).unwrap();
        }

        let runs = load(dir.path()).unwrap();

        assert!(runs.len() <= KEEP + 100, "{} runs kept", runs.len());
        assert!(runs.len() >= KEEP);
        assert_eq!(runs.last().unwrap().time, KEEP as i64 + 149);
        assert_eq!(runs[0].time + runs.len() as i64, KEEP as i64 + 150);
    }

    #[test]
    fn the_last_prune_is_the_latest_one_that_worked() {
        let runs = [
            run(10, vec![entry("prune", "uv", None)]),
            run(
                20,
                vec![entry("prune", "pnpm", None), entry("cache", "uv", None)],
            ),
            run(30, vec![entry("prune", "uv", Some("timed out"))]),
        ];

        assert_eq!(last_prune(&runs, "uv"), Some(10));
        assert_eq!(last_prune(&runs, "pnpm"), Some(20));
        assert_eq!(last_prune(&runs, "npm"), None);
    }
}
