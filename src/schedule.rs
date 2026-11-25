//! Running the unattended clean regularly: a systemd user timer where there is one, the
//! user's crontab otherwise.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::env::Env;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Every {
    Daily,
    Weekly,
    Monthly,
}

pub struct Plan {
    pub every: Every,
    pub hour: u8,
    pub minute: u8,
    /// The relief binary to run.
    pub exe: PathBuf,
    /// Environment the scheduled run needs to find the same tools and caches as now.
    pub vars: Vec<(String, String)>,
}

/// Variables that decide where tools and their caches are. A scheduled run does not see
/// what the shell profile exports, so they are captured when the schedule is set up.
pub const CAPTURED: &[&str] = &[
    "PATH",
    "GOPATH",
    "GOCACHE",
    "GOMODCACHE",
    "UV_CACHE_DIR",
    "npm_config_cache",
    "NPM_CONFIG_CACHE",
    "YARN_CACHE_FOLDER",
    "PIP_CACHE_DIR",
    "BUN_INSTALL",
    "BUN_INSTALL_CACHE_DIR",
    "PLAYWRIGHT_BROWSERS_PATH",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "RELIEF_CONFIG",
    "CARGO_HOME",
    "CYPRESS_CACHE_FOLDER",
    "PUPPETEER_CACHE_DIR",
    "HF_HOME",
    "HF_HUB_CACHE",
    "COMPOSER_CACHE_DIR",
    "DENO_DIR",
    // Where the docker daemon is, for rootless setups and remote contexts.
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "XDG_RUNTIME_DIR",
];

impl Plan {
    pub fn new(env: &Env, every: Every, at: (u8, u8)) -> Result<Plan> {
        let exe = std::env::current_exe().context("cannot find the relief binary")?;
        Ok(Plan {
            every,
            hour: at.0,
            minute: at.1,
            exe: exe.canonicalize().unwrap_or(exe),
            vars: CAPTURED
                .iter()
                .filter_map(|key| Some((key.to_string(), env.var(key)?.to_string())))
                .collect(),
        })
    }
}

/// Parse a time of day written as `HH:MM`.
pub fn parse_at(text: &str) -> Result<(u8, u8)> {
    let parsed = text.split_once(':').and_then(|(hour, minute)| {
        let digits =
            |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(hour) || !digits(minute) {
            return None;
        }
        let (hour, minute) = (hour.parse::<u8>().ok()?, minute.parse::<u8>().ok()?);
        (hour < 24 && minute < 60).then_some((hour, minute))
    });
    parsed.with_context(|| format!("`{text}` is not a time of day like 09:30"))
}

/// When the timer fires, in systemd's calendar syntax.
pub fn on_calendar(plan: &Plan) -> String {
    let time = format!("{:02}:{:02}:00", plan.hour, plan.minute);
    match plan.every {
        Every::Daily => format!("*-*-* {time}"),
        Every::Weekly => format!("Mon *-*-* {time}"),
        Every::Monthly => format!("*-*-01 {time}"),
    }
}

/// Quote a value for a systemd unit file.
fn systemd_quote(value: &str, command: bool) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' if command => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The systemd service that runs one unattended clean.
pub fn service_unit(plan: &Plan) -> String {
    let mut unit = String::from(
        "[Unit]\nDescription=relief: delete unused developer build output and caches\n\n\
         [Service]\nType=oneshot\n",
    );
    unit.push_str(&format!(
        "ExecStart={} clean --auto\n",
        systemd_quote(&plan.exe.to_string_lossy(), true)
    ));
    for (key, value) in &plan.vars {
        unit.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{key}={value}"), false)
        ));
    }
    // Stay out of the way of whatever the machine is doing.
    unit.push_str("Nice=19\nIOSchedulingClass=idle\n");
    unit
}

/// The systemd timer that starts the service.
pub fn timer_unit(plan: &Plan) -> String {
    format!(
        "[Unit]\nDescription=Run relief on a schedule\n\n\
         [Timer]\nOnCalendar={}\n# Catch up after the machine was off at the scheduled time.\nPersistent=true\n\n\
         [Install]\nWantedBy=timers.target\n",
        on_calendar(plan)
    )
}

const CRON_MARK: &str = "# relief schedule";

/// The crontab line that runs the unattended clean.
pub fn cron_line(plan: &Plan) -> String {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    let when = match plan.every {
        Every::Daily => "* * *",
        Every::Weekly => "* * 1",
        Every::Monthly => "1 * *",
    };
    let mut command = String::new();
    for (key, value) in &plan.vars {
        command.push_str(&format!("{key}={} ", quote(value)));
    }
    command.push_str(&format!(
        "{} clean --auto >/dev/null 2>&1",
        quote(&plan.exe.to_string_lossy())
    ));
    // cron turns a bare % into a newline.
    let command = command.replace('%', "\\%");
    format!("{} {} {when} {command} {CRON_MARK}", plan.minute, plan.hour)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Systemd,
    Cron,
}

const WAIT: Duration = Duration::from_secs(30);

fn unit_dir(env: &Env) -> PathBuf {
    env.config_home().join("systemd/user")
}

fn systemctl(env: &Env, args: &[&str]) -> Option<bool> {
    let program = env.which("systemctl")?;
    let mut all = vec!["--user".to_string()];
    all.extend(args.iter().map(|a| a.to_string()));
    crate::proc::run(&program, &all, WAIT).ok()
}

/// The scheduler this machine offers.
pub fn backend(env: &Env) -> Option<Backend> {
    if systemctl(env, &["show-environment"]) == Some(true) {
        Some(Backend::Systemd)
    } else if env.which("crontab").is_some() {
        Some(Backend::Cron)
    } else {
        None
    }
}

/// The user's crontab without our line.
fn other_cron_lines(crontab: &Path) -> Vec<String> {
    crate::proc::output(crontab, &["-l"], WAIT)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.contains(CRON_MARK))
        .map(str::to_string)
        .collect()
}

fn our_cron_line(crontab: &Path) -> Option<String> {
    crate::proc::output(crontab, &["-l"], WAIT)?
        .lines()
        .find(|line| line.contains(CRON_MARK))
        .map(str::to_string)
}

fn write_crontab(crontab: &Path, lines: &[String]) -> Result<()> {
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    let mut child = Command::new(crontab)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("cannot run crontab")?;
    child
        .stdin
        .take()
        .context("crontab takes no input")?
        .write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        bail!("crontab refused the new schedule");
    }
    Ok(())
}

/// Set up the schedule, replacing an earlier one.
pub fn install(env: &Env, plan: &Plan) -> Result<Backend> {
    let Some(backend) = backend(env) else {
        bail!(
            "no scheduler found: neither a systemd user session nor cron is available. \
             Run `relief clean --auto` from whatever scheduler you use."
        );
    };
    remove(env)?;
    match backend {
        Backend::Systemd => {
            let dir = unit_dir(env);
            fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
            fs::write(dir.join("relief.service"), service_unit(plan))?;
            fs::write(dir.join("relief.timer"), timer_unit(plan))?;
            systemctl(env, &["daemon-reload"]);
            if systemctl(env, &["enable", "--now", "relief.timer"]) != Some(true) {
                bail!(
                    "systemd did not accept the timer; see `systemctl --user status relief.timer`"
                );
            }
        }
        Backend::Cron => {
            let crontab = env.which("crontab").context("crontab disappeared")?;
            let mut lines = other_cron_lines(&crontab);
            lines.push(cron_line(plan));
            write_crontab(&crontab, &lines)?;
        }
    }
    Ok(backend)
}

/// Remove the schedule wherever it is. `true` when there was one.
pub fn remove(env: &Env) -> Result<bool> {
    let mut removed = false;
    let dir = unit_dir(env);
    let (service, timer) = (dir.join("relief.service"), dir.join("relief.timer"));
    if timer.exists() || service.exists() {
        systemctl(env, &["disable", "--now", "relief.timer"]);
        for unit in [&timer, &service] {
            if unit.exists() {
                fs::remove_file(unit)?;
            }
        }
        systemctl(env, &["daemon-reload"]);
        removed = true;
    }
    if let Some(crontab) = env.which("crontab")
        && our_cron_line(&crontab).is_some()
    {
        write_crontab(&crontab, &other_cron_lines(&crontab))?;
        removed = true;
    }
    Ok(removed)
}

/// A sentence describing the current schedule.
pub fn status(env: &Env) -> String {
    let timer = unit_dir(env).join("relief.timer");
    if let Ok(unit) = fs::read_to_string(&timer) {
        let when = unit
            .lines()
            .find_map(|line| line.strip_prefix("OnCalendar="))
            .unwrap_or("?");
        let next = env
            .which("systemctl")
            .and_then(|program| {
                let args = [
                    "--user",
                    "show",
                    "relief.timer",
                    "-p",
                    "NextElapseUSecRealtime",
                    "--value",
                ];
                crate::proc::output(&program, &args, WAIT)
            })
            .map(|next| format!("; next run {next}"))
            .unwrap_or_default();
        return format!("scheduled with systemd: {when}{next}");
    }
    match env
        .which("crontab")
        .and_then(|crontab| our_cron_line(&crontab))
    {
        Some(line) => format!("scheduled with cron: {line}"),
        None => "not scheduled".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(exe: &str, vars: &[(&str, &str)]) -> Plan {
        Plan {
            every: Every::Weekly,
            hour: 7,
            minute: 5,
            exe: PathBuf::from(exe),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_time_of_day_needs_real_hours_and_minutes() {
        assert_eq!(parse_at("09:30").unwrap(), (9, 30));
        assert_eq!(parse_at("0:05").unwrap(), (0, 5));
        assert_eq!(parse_at("23:59").unwrap(), (23, 59));
        for bad in [
            "24:00", "12:60", "12", "12:", ":30", "a:b", "1:2:3", "-1:30",
        ] {
            assert!(parse_at(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn systemd_sees_special_characters_literally() {
        let unit = service_unit(&plan(
            "/opt/my tools/100%/relief",
            &[("GOPATH", "/a \"b\" \\c $d")],
        ));

        assert!(
            unit.contains(r#"ExecStart="/opt/my tools/100%%/relief" clean --auto"#),
            "{unit}"
        );
        assert!(
            unit.contains(r#"Environment="GOPATH=/a \"b\" \\c $d""#),
            "{unit}"
        );
    }

    #[test]
    fn a_dollar_in_the_program_path_is_not_expanded_by_systemd() {
        let unit = service_unit(&plan("/opt/$HOME/relief", &[]));
        assert!(
            unit.contains(r#"ExecStart="/opt/$$HOME/relief" clean --auto"#),
            "{unit}"
        );
    }

    #[test]
    fn cron_sees_special_characters_literally() {
        let line = cron_line(&plan("/opt/it's 100%/relief", &[("PATH", "/a:/b c")]));

        assert_eq!(
            line,
            r#"5 7 * * 1 PATH='/a:/b c' '/opt/it'\''s 100\%/relief' clean --auto >/dev/null 2>&1 # relief schedule"#
        );
    }
}
