//! Turning a report into text a person can scan.

use crate::env::Env;
use crate::style::{Style, fit, visible_width};
use crate::survey::{Item, Report};

/// A size the way `du -h` rounds it, in units of 1024.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn size(bytes_known: Option<u64>) -> String {
    bytes_known.map_or_else(|| "?".to_string(), bytes)
}

/// The parts of one line: how long unused, what it is, and how to get it back.
struct Cells {
    age: String,
    main: String,
    tail: String,
}

fn cells(item: &Item, env: &Env) -> Cells {
    let path = env.tilde(&item.path);
    match item.category {
        "project" => Cells {
            age: format!("{}d", item.days.unwrap_or(0)),
            main: path,
            tail: item.regen.clone(),
        },
        "cache" => Cells {
            age: String::new(),
            main: format!("{} cache · {} · {path}", item.kind, item.detail),
            tail: item.regen.clone(),
        },
        "docker" => Cells {
            age: item.days.map(|d| format!("{d}d")).unwrap_or_default(),
            main: format!("{} · {}", item.kind, item.detail),
            tail: item.regen.clone(),
        },
        "prune" => Cells {
            age: String::new(),
            main: format!("{} cache · {} · {path}", item.kind, item.detail),
            tail: String::new(),
        },
        _ => Cells {
            age: String::new(),
            main: path,
            tail: item.detail.clone(),
        },
    }
}

/// One line describing an item, without its size.
pub fn describe(item: &Item, env: &Env) -> String {
    let c = cells(item, env);
    let age = if c.age.is_empty() {
        String::new()
    } else {
        format!("{}  ", c.age)
    };
    let tail = if c.tail.is_empty() {
        String::new()
    } else {
        format!("  ·  {}", c.tail)
    };
    format!("{age}{}{tail}", c.main)
}

const GIB: u64 = 1 << 30;

/// A table row: size, age, what, and the way back, cut to the terminal width.
fn row(style: &Style, lead: &str, bytes_known: Option<u64>, c: &Cells) -> String {
    let mut main = c.main.clone();
    let mut tail = if c.tail.is_empty() {
        String::new()
    } else {
        format!("  ·  {}", c.tail)
    };
    if let Some(width) = style.width {
        // lead, size (8), gap (2), age (6), gap (2)
        let room = width.saturating_sub(visible_width(lead) + 18);
        let (m, t) = (visible_width(&main), visible_width(&tail));
        if m + t > room {
            // The way back may take up to a third of the line; the rest belongs to the
            // path, which is shortened in the middle so its end stays readable.
            let main_room = room - t.min(room / 3);
            if m > main_room {
                main = fit(&main, main_room);
            }
            let left = room.saturating_sub(visible_width(&main));
            tail = if t <= left {
                tail
            } else if left >= 8 {
                let kept: String = tail.chars().take(left - 1).collect();
                format!("{kept}…")
            } else {
                String::new()
            };
        }
    }
    let size_text = format!("{:>8}", size(bytes_known));
    let size_text = if bytes_known.is_some_and(|b| b >= GIB) {
        style.warn(&style.bold(&size_text))
    } else {
        style.bold(&size_text)
    };
    format!(
        "{lead}{size_text}  {}  {main}{}",
        style.dim(&format!("{:>6}", c.age)),
        style.dim(&tail)
    )
}

fn cut(style: &Style, text: String) -> String {
    match style.width {
        Some(width) => fit(&text, width),
        None => text,
    }
}

/// The text of each item as the picker shows it.
pub fn picker_text(item: &Item, env: &Env) -> String {
    describe(item, env)
}

/// The list of deletable items, numbered, for choosing by typing line numbers.
pub fn items(report: &Report, env: &Env) -> String {
    let mut out = String::new();
    for (i, item) in report.items.iter().enumerate() {
        out.push_str(&format!(
            "{:>3}  {:>8}  {}\n",
            i + 1,
            size(item.bytes),
            describe(item, env)
        ));
    }
    out
}

/// The full scan output.
pub fn scan(report: &Report, env: &Env, all: bool, style: &Style) -> String {
    let mut out = String::new();
    let heading = |out: &mut String, title: &str, about: &str, columns: &str| {
        out.push_str(&format!(
            "\n{}  {}\n{}\n",
            style.bold(title),
            style.dim(&cut(style, about.to_string())),
            style.dim(columns)
        ));
    };
    if report.items.is_empty() {
        out.push_str("Nothing unused found.\n");
    }
    let mut section = "";
    for item in &report.items {
        let here = match item.category {
            "project" => "PROJECTS",
            "cache" | "prune" => "CACHES",
            "docker" => "DOCKER",
            _ => "LEFTOVERS",
        };
        if here != section {
            section = here;
            match here {
                "PROJECTS" => heading(
                    &mut out,
                    here,
                    "projects nobody touched",
                    "      SIZE  UNUSED  WHERE  ·  HOW TO GET IT BACK",
                ),
                "CACHES" => heading(
                    &mut out,
                    here,
                    "entries nothing has used",
                    "      SIZE          WHAT",
                ),
                _ => heading(
                    &mut out,
                    here,
                    "from an interrupted run",
                    "      SIZE          WHERE",
                ),
            }
        }
        out.push_str(&row(style, "  ", item.bytes, &cells(item, env)));
        out.push('\n');
    }
    if !report.items.is_empty() {
        let unknown = report.items.iter().any(|i| i.bytes.is_none());
        out.push_str(&format!(
            "\n{}{}\n{}\n",
            style.good(&style.bold(&format!(
                "Reclaimable  {}  in {} items",
                bytes(report.reclaimable_bytes),
                report.items.len()
            ))),
            if unknown {
                style.dim("  (plus whatever pruning frees)")
            } else {
                String::new()
            },
            style.dim("Run `relief clean` to choose what to delete.")
        ));
    }
    if all && !report.kept.is_empty() {
        heading(
            &mut out,
            "KEPT",
            "could be regenerated, but stays",
            "      SIZE  UNUSED  WHERE  ·  WHY",
        );
        for kept in &report.kept {
            let c = Cells {
                age: format!("{}d", kept.days),
                main: env.tilde(&kept.path),
                tail: kept.reason.to_string(),
            };
            out.push_str(&row(style, "  ", kept.bytes, &c));
            out.push('\n');
        }
    } else if !report.kept.is_empty() {
        out.push_str(&style.dim(&cut(
            style,
            format!(
                "{} more are kept (in use, or not provably generated) · relief scan --all",
                report.kept.len()
            ),
        )));
        out.push('\n');
    }
    for note in &report.notes {
        out.push_str(&format!("{} {note}\n", style.warn("note:")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use crate::style::visible_width;
    use crate::survey::{Action, Kept};
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn sizes_are_rounded_like_du() {
        let cases = [
            (0, "0 B"),
            (1023, "1023 B"),
            (1536, "1.5 KB"),
            (20 * 1024, "20 KB"),
            (733_000_000, "699 MB"),
            (3_758_096_384, "3.5 GB"),
            (10 * 1024 * 1024 * 1024, "10 GB"),
            (3 * 1024_u64.pow(4), "3.0 TB"),
        ];
        for (n, want) in cases {
            assert_eq!(bytes(n), want, "{n}");
        }
    }

    fn fixture() -> (tempfile::TempDir, Env, Report) {
        let dir = tempfile::tempdir().unwrap();
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), dir.path().display().to_string());
        let env = Env::from_vars(vars, 0).unwrap();
        let deep = "code/clients/some-very-long-client-name/platform/services/frontend-web";
        let item = |category: &'static str,
                    kind: &str,
                    path: PathBuf,
                    bytes,
                    days,
                    detail: &str,
                    regen: &str| Item {
            category,
            kind: kind.to_string(),
            path,
            bytes,
            days,
            detail: detail.to_string(),
            regen: regen.to_string(),
            mode: Mode::Auto,
            action: Action::Leftover,
        };
        let report = Report {
            items: vec![
                item(
                    "project",
                    "node",
                    env.home.join(deep).join("node_modules"),
                    Some(3 << 30),
                    Some(41),
                    "",
                    "npm ci",
                ),
                item(
                    "cache",
                    "go-mod",
                    env.home.join("go/pkg/mod"),
                    Some(5 << 20),
                    None,
                    "782 entries unused for 60+ days",
                    "downloaded again when needed",
                ),
                item(
                    "prune",
                    "uv",
                    env.home.join(".cache/uv"),
                    None,
                    None,
                    "`uv cache prune`",
                    "",
                ),
            ],
            kept: vec![Kept {
                kind: "rust".to_string(),
                path: env.home.join("code/busy/target"),
                bytes: Some(1 << 20),
                days: 2,
                reason: "in use",
            }],
            notes: vec![],
            reclaimable_bytes: (3 << 30) + (5 << 20),
        };
        (dir, env, report)
    }

    #[test]
    fn the_report_names_its_columns_and_totals_what_can_go() {
        let (_d, env, report) = fixture();

        let text = scan(&report, &env, false, &Style::PLAIN);

        assert!(text.contains("SIZE") && text.contains("UNUSED"), "{text}");
        assert!(
            text.contains("3.0 GB") && text.contains("41d") && text.contains("npm ci"),
            "{text}"
        );
        assert!(text.contains("782 entries unused for 60+ days"), "{text}");
        assert!(text.contains("uv cache prune"), "{text}");
        assert!(text.contains("3 items"), "{text}");
        assert!(
            !text.contains("code/busy/target"),
            "kept items need --all: {text}"
        );
    }

    #[test]
    fn plain_output_has_no_colour_codes_and_coloured_output_says_the_same() {
        let (_d, env, report) = fixture();
        let colored = Style {
            color: true,
            width: None,
        };

        let plain = scan(&report, &env, true, &Style::PLAIN);
        let color = scan(&report, &env, true, &colored);

        assert!(!plain.contains('\x1b'));
        assert!(color.contains('\x1b'));
        let strip = |text: &str| -> String {
            text.lines()
                .map(|l| format!("{}\n", " ".repeat(0) + &strip_codes(l)))
                .collect()
        };
        assert_eq!(strip(&color), plain);
    }

    fn strip_codes(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn lines_fit_a_narrow_terminal_and_still_end_in_the_part_that_matters() {
        let (_d, env, report) = fixture();
        let narrow = Style {
            color: true,
            width: Some(70),
        };

        let text = scan(&report, &env, true, &narrow);

        for line in text.lines() {
            assert!(
                visible_width(line) <= 70,
                "{} columns: {line}",
                visible_width(line)
            );
        }
        assert!(text.contains("frontend-web/node_modules"), "{text}");
        assert!(text.contains("npm ci"), "{text}");
    }

    #[test]
    fn kept_items_say_why_they_stay() {
        let (_d, env, report) = fixture();

        let text = scan(&report, &env, true, &Style::PLAIN);

        let line = text
            .lines()
            .find(|l| l.contains("~/code/busy/target"))
            .expect(&text);
        assert!(line.contains("in use") && line.contains("2d"), "{line}");
    }
}
