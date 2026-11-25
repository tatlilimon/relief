//! Carrying out deletions and recording them.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::env::Env;
use crate::fsmeta::{self, Measure, SharedTimes};
use crate::history::{self, Entry, Run};
use crate::remove;
use crate::render;
use crate::style::Style;
use crate::survey::{Action, Item};

/// Turn what the user typed into the items they chose (as indexes into `items`).
///
/// `a` is everything, `p` all project items, `c` all cache items, numbers and ranges such as
/// `1,3-5` pick single lines, and `q` or an empty line picks nothing.
pub fn parse_selection(input: &str, items: &[Item]) -> Result<Vec<usize>, String> {
    let tokens: Vec<String> = input
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect();
    let quit = |t: &String| matches!(t.as_str(), "q" | "n" | "none" | "quit");
    if tokens.iter().any(quit) {
        return if tokens.len() == 1 {
            Ok(Vec::new())
        } else {
            Err("`q` cannot be combined with a choice".to_string())
        };
    }
    let group = |wanted: &[&str]| -> Vec<usize> {
        (0..items.len())
            .filter(|i| wanted.contains(&items[*i].category))
            .collect()
    };
    let line = |text: &str| -> Result<usize, String> {
        match text.parse::<usize>() {
            Ok(n) if (1..=items.len()).contains(&n) => Ok(n - 1),
            _ => Err(format!(
                "`{text}` is not a line between 1 and {}",
                items.len()
            )),
        }
    };
    let mut chosen = BTreeSet::new();
    for token in &tokens {
        match token.as_str() {
            "a" | "all" => chosen.extend(0..items.len()),
            "p" => chosen.extend(group(&["project"])),
            "c" => chosen.extend(group(&["cache", "prune"])),
            "d" => chosen.extend(group(&["docker"])),
            range => match range.split_once('-') {
                Some((first, last)) => {
                    let (first, last) = (line(first)?, line(last)?);
                    if first > last {
                        return Err(format!("`{range}` runs backwards"));
                    }
                    chosen.extend(first..=last);
                }
                None => {
                    chosen.insert(line(range)?);
                }
            },
        }
    }
    Ok(chosen.into_iter().collect())
}

pub struct Outcome {
    pub run: Run,
    pub failures: usize,
}

/// How long a tool may take to prune its own cache.
const PRUNE_TIME: Duration = Duration::from_secs(15 * 60);

/// Delete the items, say what happened to each, and record the run.
pub fn execute(env: &Env, items: &[&Item], mode: &str, style: &Style) -> Result<Outcome> {
    fs::create_dir_all(&env.state_dir)
        .with_context(|| format!("cannot create {}", env.state_dir.display()))?;
    let lock = File::create(env.state_dir.join("lock"))?;
    if lock.try_lock().is_err() {
        bail!("another relief clean is running");
    }
    let measure = Measure {
        count_reads: false,
        shared_times: SharedTimes::Count,
    };
    let after = |item: &Item, before: u64| {
        before.saturating_sub(fsmeta::measure_tree(&item.path, measure).bytes)
    };

    let mut run = Run {
        time: env.now,
        mode: mode.to_string(),
        freed: 0,
        items: Vec::new(),
    };
    let mut failures = 0;
    for item in items {
        let (freed, error) = match &item.action {
            Action::Tree { container } => {
                match remove::remove_entry(&item.path, container, &env.home) {
                    Ok(()) => (item.bytes.unwrap_or(0), None),
                    Err(e) => (0, Some(e.to_string())),
                }
            }
            Action::Leftover => match remove::remove_leftover(&item.path) {
                Ok(()) => (item.bytes.unwrap_or(0), None),
                Err(e) => (0, Some(e.to_string())),
            },
            Action::Entries { paths, before } => {
                let failed = paths
                    .iter()
                    .filter(|p| remove::remove_entry(p, &item.path, &env.home).is_err())
                    .count();
                let error = (failed > 0)
                    .then(|| format!("{failed} of {} entries could not be removed", paths.len()));
                (after(item, *before), error)
            }
            Action::Docker { program, commands } => {
                let mut error = None;
                let mut done = 0;
                for command in commands {
                    match crate::proc::run(program, command, PRUNE_TIME) {
                        Ok(true) => done += 1,
                        Ok(false) => {
                            error.get_or_insert(format!("`docker {}` failed", command.join(" ")));
                        }
                        Err(e) => {
                            error.get_or_insert(format!("`docker {}`: {e}", command.join(" ")));
                        }
                    }
                }
                // Docker reports no exact figure; count the estimate only when all went.
                let freed = if done == commands.len() {
                    item.bytes.unwrap_or(0)
                } else {
                    0
                };
                (freed, error)
            }
            Action::Prune { prune, before } => {
                // From the home directory, like the question that located the cache.
                let pruned =
                    crate::proc::run_in(&env.home, &prune.program, &prune.args, PRUNE_TIME);
                let error = match pruned {
                    Ok(true) => None,
                    Ok(false) => Some(format!("{} reported a failure", item.detail)),
                    Err(e) => Some(format!("{}: {e}", item.detail)),
                };
                (after(item, *before), error)
            }
        };
        let what = render::describe(item, env);
        match &error {
            // The word carries the meaning; the mark and the colour only repeat it.
            None => println!(
                "{} {}  {what}",
                style.good("✓ removed"),
                style.bold(&format!("{:>8}", render::bytes(freed)))
            ),
            Some(e) => {
                failures += 1;
                eprintln!(
                    "{}            {}: {e}",
                    style.bad("✗ failed"),
                    env.tilde(&item.path)
                );
            }
        }
        run.freed += freed;
        run.items.push(Entry {
            category: item.category.to_string(),
            kind: item.kind.clone(),
            path: item.path.display().to_string(),
            bytes: freed,
            regen: item.regen.clone(),
            error,
        });
    }
    history::append(&env.state_dir, &run)?;
    Ok(Outcome { run, failures })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use std::path::PathBuf;

    fn items(categories: &[&'static str]) -> Vec<Item> {
        categories
            .iter()
            .map(|category| Item {
                category,
                kind: "node".to_string(),
                path: PathBuf::from("/x"),
                bytes: Some(1),
                days: None,
                detail: String::new(),
                regen: String::new(),
                mode: Mode::Auto,
                action: Action::Leftover,
            })
            .collect()
    }

    #[test]
    fn numbers_and_ranges_pick_single_lines() {
        let list = items(&["project"; 6]);
        assert_eq!(parse_selection("2", &list), Ok(vec![1]));
        assert_eq!(parse_selection("1,3-5", &list), Ok(vec![0, 2, 3, 4]));
        assert_eq!(parse_selection(" 6  2 2 ", &list), Ok(vec![1, 5]));
    }

    #[test]
    fn letters_pick_groups() {
        let list = items(&["leftover", "project", "project", "cache", "prune"]);
        assert_eq!(parse_selection("a", &list), Ok(vec![0, 1, 2, 3, 4]));
        assert_eq!(parse_selection("P", &list), Ok(vec![1, 2]));
        assert_eq!(parse_selection("c", &list), Ok(vec![3, 4]));
        assert_eq!(parse_selection("c,2", &list), Ok(vec![1, 3, 4]));
    }

    #[test]
    fn nothing_typed_picks_nothing() {
        let list = items(&["project"; 3]);
        assert_eq!(parse_selection("", &list), Ok(vec![]));
        assert_eq!(parse_selection("q\n", &list), Ok(vec![]));
    }

    #[test]
    fn anything_unclear_is_refused_rather_than_guessed() {
        let list = items(&["project"; 3]);
        for input in ["4", "0", "2-9", "3-1", "yes", "1;2", "-1", "a b q"] {
            assert!(
                parse_selection(input, &list).is_err(),
                "{input:?} was accepted"
            );
        }
    }
}
