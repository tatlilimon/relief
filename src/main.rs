use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone};
use clap::{Args, Parser, Subcommand};

use relief::config::{Config, Mode, TEMPLATE};
use relief::env::Env;
use relief::picker::Row;
use relief::schedule::{self, Backend, Every, Plan};
use relief::style::{ColorChoice, Style};
use relief::survey::{Item, Only, Report, Request, survey};
use relief::ui::{self, Spinner};
use relief::{caches, clean, history, render, rules};

/// Finds developer build output and caches that are no longer used, and deletes them.
///
/// Only things that can be produced again are ever deleted, and every deletion is recorded
/// with the command that brings it back (see `relief history`).
#[derive(Parser)]
#[command(version, max_term_width = 100)]
struct Cli {
    /// Use this configuration file instead of ~/.config/relief/config.toml
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// When to use colour
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        value_name = "WHEN"
    )]
    color: ColorChoice,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show what is unused and could be deleted (the default; deletes nothing)
    Scan {
        #[command(flatten)]
        scope: Scope,
        /// Also list what is kept, and why
        #[arg(long, short)]
        all: bool,
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete what is unused, after asking which
    Clean {
        #[command(flatten)]
        scope: Scope,
        /// Delete everything listed without asking
        #[arg(long, short)]
        yes: bool,
        /// Unattended: delete what the configuration marks "auto", ask about nothing
        #[arg(long, conflicts_with = "yes")]
        auto: bool,
        /// Show what would be deleted and stop
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Choose by typing line numbers instead of the full-screen list (works with screen
        /// readers and very simple terminals)
        #[arg(long)]
        plain: bool,
    },
    /// Run the unattended clean regularly
    Schedule {
        #[command(subcommand)]
        action: ScheduleCmd,
    },
    /// Show what earlier runs deleted
    History {
        /// How many runs to show
        #[arg(long, short = 'n', default_value_t = 10)]
        last: usize,
        /// List every deleted item with the command that brings it back
        #[arg(long, short)]
        verbose: bool,
    },
    /// Show or create the configuration
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
}

#[derive(Args)]
struct Scope {
    /// Look only at projects below these directories (caches are left alone)
    paths: Vec<PathBuf>,
    /// Look only at projects, caches or docker
    #[arg(long, value_enum)]
    only: Option<Only>,
    /// Treat projects as unused after this many days, for this run
    #[arg(long, value_name = "DAYS", value_parser = clap::value_parser!(u32).range(1..))]
    project_days: Option<u32>,
    /// Treat cache entries as unused after this many days, for this run
    #[arg(long, value_name = "DAYS", value_parser = clap::value_parser!(u32).range(1..))]
    cache_days: Option<u32>,
}

impl Scope {
    fn request(self, all: bool) -> Request {
        Request {
            paths: self.paths,
            only: self.only,
            all,
            project_days: self.project_days,
            cache_days: self.cache_days,
        }
    }
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Start running `relief clean --auto` regularly
    On {
        /// How often
        #[arg(long, value_enum, default_value = "weekly")]
        every: Every,
        /// Time of day, HH:MM
        #[arg(long, default_value = "12:00")]
        at: String,
        /// Do not ask for confirmation
        #[arg(long, short)]
        yes: bool,
    },
    /// Stop
    Off,
    /// Show whether and when relief runs
    Status,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print where the configuration file is
    Path,
    /// Print the rules in effect
    Show,
    /// Write a commented configuration file with the defaults
    Init,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("relief: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        bail!("refusing to run as root: relief cleans your own files and needs no privileges");
    }
    let mut env = Env::from_process()?;
    if let Some(file) = cli.config {
        env.config_file = file;
    }
    let style = Style::decide(cli.color, &env, ui::terminal_width());
    let look = |config: &Config, request: &Request| {
        let _spinner = Spinner::start("Scanning projects and caches…");
        survey(&env, config, request)
    };
    let scan_default = Cmd::Scan {
        scope: Scope {
            paths: Vec::new(),
            only: None,
            project_days: None,
            cache_days: None,
        },
        all: false,
        json: false,
    };
    match cli.command.unwrap_or(scan_default) {
        Cmd::Config { action } => config_cmd(&env, action.unwrap_or(ConfigCmd::Show)),
        Cmd::History { last, verbose } => history_cmd(&env, last, verbose),
        Cmd::Scan { scope, all, json } => {
            let config = Config::load(&env.config_file)?;
            let report = look(&config, &scope.request(all))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render::scan(&report, &env, all, &style));
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Clean {
            scope,
            yes,
            auto,
            dry_run,
            plain,
        } => {
            let config = Config::load(&env.config_file)?;
            let report = look(&config, &scope.request(false))?;
            let how = How {
                yes,
                auto,
                dry_run,
                plain: plain || env.var("TERM") == Some("dumb"),
            };
            clean_cmd(&env, &report, how, &style)
        }
        Cmd::Schedule { action } => schedule_cmd(&env, action),
    }
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn ask(question: &str) -> Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer)
}

struct How {
    yes: bool,
    auto: bool,
    dry_run: bool,
    plain: bool,
}

/// Ask which items to delete. `None` when the user backs out.
fn choose<'a>(
    env: &Env,
    report: &'a Report,
    plain: bool,
    style: &Style,
) -> Result<Option<Vec<&'a Item>>> {
    let lines = if plain {
        print!("{}", render::items(report, env));
        loop {
            let answer = ask(
                "\nDelete which?  a = all · p = projects · c = caches · d = docker · lines like 1,3-5 · q = nothing\n> ",
            )?;
            match clean::parse_selection(&answer, &report.items) {
                Ok(lines) => break lines,
                Err(problem) => println!("{problem}"),
            }
        }
    } else {
        let rows = report
            .items
            .iter()
            .map(|item| Row {
                category: item.category,
                bytes: item.bytes,
                text: render::picker_text(item, env),
            })
            .collect();
        let Some(lines) = ui::pick(rows, style)? else {
            return Ok(None);
        };
        // The list is gone from the screen; say what was ticked before asking.
        let bytes: u64 = lines.iter().filter_map(|i| report.items[*i].bytes).sum();
        for i in &lines {
            let item = &report.items[*i];
            let size = item.bytes.map_or_else(|| "?".to_string(), render::bytes);
            println!(
                "  {}  {}",
                style.bold(&format!("{size:>8}")),
                render::describe(item, env)
            );
        }
        if lines.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let question = format!(
            "Delete these {} items ({})? [y/N] ",
            lines.len(),
            render::bytes(bytes)
        );
        if !ask(&style.bold(&question))?
            .trim()
            .eq_ignore_ascii_case("y")
        {
            return Ok(None);
        }
        lines
    };
    Ok(Some(lines.into_iter().map(|i| &report.items[i]).collect()))
}

fn clean_cmd(env: &Env, report: &Report, how: How, style: &Style) -> Result<ExitCode> {
    let How {
        yes,
        auto,
        dry_run,
        plain,
    } = how;
    let all: Vec<&Item> = report.items.iter().collect();
    let chosen: Vec<&Item> = if auto {
        // Leftovers were already being deleted when a run was interrupted.
        let deletable = |i: &&Item| i.mode == Mode::Auto || i.category == "leftover";
        all.iter().copied().filter(deletable).collect()
    } else if yes || dry_run {
        all.clone()
    } else {
        if report.items.is_empty() {
            println!("Nothing unused found.");
            return Ok(ExitCode::SUCCESS);
        }
        if !interactive() {
            bail!(
                "refusing to delete without confirmation: pass --yes to delete everything \
                 listed, or --auto to follow the configuration"
            );
        }
        match choose(env, report, plain, style)? {
            Some(chosen) => chosen,
            None => {
                println!("Nothing deleted.");
                return Ok(ExitCode::SUCCESS);
            }
        }
    };

    if dry_run {
        println!("Would delete:");
        for item in &chosen {
            let size = item.bytes.map_or_else(|| "?".to_string(), render::bytes);
            println!("  {size:>8}  {}", render::describe(item, env));
        }
        return Ok(ExitCode::SUCCESS);
    }
    let waiting: Vec<&Item> = all
        .iter()
        .copied()
        .filter(|i| !chosen.iter().any(|c| std::ptr::eq(*c, *i)))
        .collect();
    if chosen.is_empty() {
        println!("Nothing deleted.");
    } else {
        let outcome = clean::execute(env, &chosen, if auto { "auto" } else { "manual" }, style)?;
        println!(
            "\n{}  ·  {} of {} items  ·  {}",
            style.good(&style.bold(&format!("Freed {}", render::bytes(outcome.run.freed)))),
            chosen.len() - outcome.failures,
            chosen.len(),
            style.dim("`relief history --verbose` lists them with the way back")
        );
        if outcome.failures > 0 {
            return Ok(ExitCode::from(2));
        }
    }
    if auto && !waiting.is_empty() {
        let bytes: u64 = waiting.iter().filter_map(|i| i.bytes).sum();
        println!(
            "{} more item(s) ({}) are set to \"ask\": run `relief clean` to decide about them.",
            waiting.len(),
            render::bytes(bytes)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn history_cmd(env: &Env, last: usize, verbose: bool) -> Result<ExitCode> {
    let runs = history::load(&env.state_dir)?;
    if runs.is_empty() {
        println!("Nothing has been deleted yet.");
        return Ok(ExitCode::SUCCESS);
    }
    for run in runs.iter().rev().take(last).rev() {
        let when = Local.timestamp_opt(run.time, 0).single().map_or_else(
            || run.time.to_string(),
            |t| t.format("%Y-%m-%d %H:%M").to_string(),
        );
        println!(
            "{when}  {:<6}  freed {:>8}  {} item(s)",
            run.mode,
            render::bytes(run.freed),
            run.items.len()
        );
        if verbose {
            for item in &run.items {
                let path = env.tilde(std::path::Path::new(&item.path));
                let back = if item.regen.is_empty() {
                    String::new()
                } else {
                    format!("  · {}", item.regen)
                };
                match &item.error {
                    None => println!("    {:>8}  {path}{back}", render::bytes(item.bytes)),
                    Some(e) => println!("      FAILED  {path}: {e}"),
                }
            }
        }
    }
    let total: u64 = runs.iter().map(|r| r.freed).sum();
    println!(
        "Freed {} in {} run(s) so far.",
        render::bytes(total),
        runs.len()
    );
    Ok(ExitCode::SUCCESS)
}

fn config_cmd(env: &Env, action: ConfigCmd) -> Result<ExitCode> {
    match action {
        ConfigCmd::Path => println!("{}", env.config_file.display()),
        ConfigCmd::Init => {
            if env.config_file.exists() {
                bail!(
                    "{} already exists; it was left as it is",
                    env.config_file.display()
                );
            }
            if let Some(dir) = env.config_file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&env.config_file, TEMPLATE)
                .with_context(|| format!("cannot write {}", env.config_file.display()))?;
            println!("Wrote {}", env.config_file.display());
        }
        ConfigCmd::Show => {
            let config = Config::load(&env.config_file)?;
            let source = if env.config_file.exists() {
                ""
            } else {
                " (not there: defaults apply)"
            };
            println!("file:        {}{source}", env.config_file.display());
            println!("roots:       {}", config.roots.join(", "));
            println!("ignore:      {}", config.ignore.join(", "));
            println!("count reads: {}", config.count_reads);
            let mode = |m: Mode| match m {
                Mode::Auto => "auto",
                Mode::Ask => "ask",
                Mode::Off => "off",
            };
            println!("\nprojects     unattended  unused after");
            for kind in rules::kinds() {
                let p = config.project(kind);
                println!("  {kind:<10} {:<11} {} days", mode(p.mode), p.days);
            }
            println!("\ncaches         unattended  unused after");
            for kind in caches::KINDS {
                let p = config.cache(kind);
                println!("  {kind:<12} {:<11} {} days", mode(p.mode), p.days);
            }
            println!("\ndocker             unattended  unused after");
            for kind in relief::docker::KINDS {
                let p = config.docker(kind);
                println!("  {kind:<16} {:<11} {} days", mode(p.mode), p.days);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn schedule_cmd(env: &Env, action: ScheduleCmd) -> Result<ExitCode> {
    match action {
        ScheduleCmd::Status => println!("{}", schedule::status(env)),
        ScheduleCmd::Off => {
            if schedule::remove(env)? {
                println!("The schedule is off.");
            } else {
                println!("Nothing was scheduled.");
            }
        }
        ScheduleCmd::On { every, at, yes } => {
            let plan = Plan::new(env, every, schedule::parse_at(&at)?)?;
            let config = Config::load(&env.config_file)?;
            if !yes {
                if !interactive() {
                    bail!(
                        "refusing to schedule automatic deletion without confirmation: pass --yes"
                    );
                }
                let report = survey(env, &config, &Request::default())?;
                let auto: Vec<&Item> = report
                    .items
                    .iter()
                    .filter(|i| i.mode == Mode::Auto)
                    .collect();
                let bytes: u64 = auto.iter().filter_map(|i| i.bytes).sum();
                println!(
                    "From now on relief will delete, without asking, whatever your configuration \
                     marks \"auto\"\n(see `relief config show`). A run right now would delete {} \
                     item(s), {}; `relief scan` lists them.",
                    auto.len(),
                    render::bytes(bytes)
                );
                if !ask("Schedule it? [y/N] ")?.trim().eq_ignore_ascii_case("y") {
                    println!("Not scheduled.");
                    return Ok(ExitCode::SUCCESS);
                }
            }
            let how = match schedule::install(env, &plan)? {
                Backend::Systemd => "a systemd user timer",
                Backend::Cron => "your crontab",
            };
            println!("Scheduled with {how}. {}", schedule::status(env));
        }
    }
    Ok(ExitCode::SUCCESS)
}
