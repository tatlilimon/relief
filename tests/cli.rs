//! The program as a user meets it. Every test runs the real binary against a throwaway home
//! directory, with nothing on PATH but the fake tools the test puts there.

use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

const DAY: u64 = 86_400;

struct World {
    _dir: tempfile::TempDir,
    home: PathBuf,
    now: u64,
}

struct Ran {
    out: String,
    err: String,
    code: i32,
}

impl World {
    /// A home directory seen 100 days from now, so files the test creates look 100 days old.
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        fs::create_dir_all(home.join("bin")).unwrap();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        World {
            _dir: dir,
            home,
            now,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    fn old(&self, files: &[&str]) -> &World {
        for f in files {
            let path = self.path(f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![b'x'; 20_000]).unwrap();
        }
        self
    }

    /// Files that will look 5 days old.
    fn fresh(&self, files: &[&str]) -> &World {
        self.old(files);
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(self.now + 95 * DAY);
        for f in files {
            File::options()
                .write(true)
                .open(self.path(f))
                .unwrap()
                .set_times(FileTimes::new().set_accessed(t).set_modified(t))
                .unwrap();
        }
        self
    }

    /// A node project with installed dependencies.
    fn node(&self, dir: &str) -> &World {
        self.old(&[
            &format!("{dir}/package.json"),
            &format!("{dir}/package-lock.json"),
            &format!("{dir}/src/index.js"),
            &format!("{dir}/node_modules/dep/index.js"),
        ])
    }

    fn config(&self, text: &str) -> &World {
        let file = self.path(".config/relief/config.toml");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
        self
    }

    fn tool(&self, name: &str, body: &str) -> &World {
        let file = self.path("bin").join(name);
        fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_relief"));
        cmd.args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", self.path("bin"))
            .env("RELIEF_TEST_NOW", (self.now + 100 * DAY).to_string())
            .current_dir(&self.home);
        cmd
    }

    fn run(&self, args: &[&str]) -> Ran {
        ran(self.command(args).stdin(Stdio::null()).output().unwrap())
    }

    fn run_typing(&self, args: &[&str], input: &str) -> Ran {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        ran(child.wait_with_output().unwrap())
    }

    fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap_or_default()
    }
}

fn ran(output: Output) -> Ran {
    Ran {
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code().unwrap_or(-1),
    }
}

fn disk(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).unwrap().blocks() * 512
}

// --- scan --------------------------------------------------------------------------------

#[test]
fn scan_shows_what_is_unused_and_how_to_get_it_back_without_deleting() {
    let w = World::new();
    w.node("code/old");

    let r = w.run(&["scan"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("~/code/old/node_modules"), "{}", r.out);
    assert!(r.out.contains("npm ci"), "{}", r.out);
    assert!(r.out.contains("100d"), "{}", r.out);
    assert!(w.exists("code/old/node_modules/dep/index.js"));
}

#[test]
fn running_without_a_command_scans() {
    let w = World::new();
    w.node("code/old");

    let r = w.run(&[]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("~/code/old/node_modules"), "{}", r.out);
    assert!(w.exists("code/old/node_modules"));
}

#[test]
fn a_project_in_use_is_only_listed_on_request_and_marked_as_kept() {
    let w = World::new();
    w.node("code/busy");
    w.fresh(&["code/busy/src/index.js"]);

    let brief = w.run(&["scan"]);
    let all = w.run(&["scan", "--all"]);

    assert!(
        !brief.out.contains("code/busy/node_modules"),
        "{}",
        brief.out
    );
    assert!(all.out.contains("~/code/busy/node_modules"), "{}", all.out);
    assert!(all.out.contains("in use"), "{}", all.out);
}

#[test]
fn scan_can_report_as_json() {
    let w = World::new();
    w.node("code/old");
    let bytes = disk(&w.path("code/old/node_modules/dep/index.js"));

    let r = w.run(&["scan", "--json"]);

    let json: serde_json::Value = serde_json::from_str(&r.out).expect(&r.out);
    let item = &json["items"][0];
    assert_eq!(
        item["path"],
        w.path("code/old/node_modules").display().to_string()
    );
    assert_eq!(item["kind"], "node");
    assert_eq!(item["regen"], "npm ci");
    assert!(item["bytes"].as_u64().unwrap() >= bytes);
    assert_eq!(json["reclaimable_bytes"], item["bytes"]);
}

#[test]
fn the_number_of_days_can_be_set_for_one_run() {
    let w = World::new();
    w.node("code/old");

    let r = w.run(&["scan", "--project-days", "150"]);

    assert!(!r.out.contains("code/old/node_modules"), "{}", r.out);
}

// --- clean -------------------------------------------------------------------------------

#[test]
fn clean_deletes_what_is_unused_and_nothing_else() {
    let w = World::new();
    w.node("code/old").node("code/busy");
    w.fresh(&["code/busy/src/index.js"]);

    let r = w.run(&["clean", "--yes"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!w.exists("code/old/node_modules"));
    assert!(w.exists("code/old/src/index.js"));
    assert!(w.exists("code/old/package-lock.json"));
    assert!(w.exists("code/busy/node_modules/dep/index.js"));
}

#[test]
fn clean_refuses_to_delete_when_nobody_can_confirm() {
    let w = World::new();
    w.node("code/old");

    let r = w.run(&["clean"]);

    assert_ne!(r.code, 0);
    assert!(r.err.contains("--yes"), "{}", r.err);
    assert!(w.exists("code/old/node_modules"));
}

#[test]
fn a_dry_run_deletes_nothing() {
    let w = World::new();
    w.node("code/old");

    let r = w.run(&["clean", "--yes", "--dry-run"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("~/code/old/node_modules"), "{}", r.out);
    assert!(w.exists("code/old/node_modules/dep/index.js"));
    assert!(!w.run(&["history"]).out.contains("node_modules"));
}

#[test]
fn what_was_deleted_is_in_the_history_with_the_way_back() {
    let w = World::new();
    w.node("code/old");

    w.run(&["clean", "--yes"]);
    let r = w.run(&["history", "--verbose"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("~/code/old/node_modules"), "{}", r.out);
    assert!(r.out.contains("npm ci"), "{}", r.out);
    assert!(r.out.contains("manual"), "{}", r.out);
}

#[test]
fn clean_limited_to_a_path_leaves_everything_else() {
    let w = World::new();
    w.node("code/a").node("code/b");
    w.old(&[".cache/go-build/ab/old-a"]);

    let r = w.run(&["clean", "--yes", "code/a"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!w.exists("code/a/node_modules"));
    assert!(w.exists("code/b/node_modules"));
    assert!(w.exists(".cache/go-build/ab/old-a"));
}

#[test]
fn clean_drops_stale_cache_entries_and_keeps_the_ones_in_use() {
    let w = World::new();
    w.old(&[".cache/go-build/ab/old-a"]);
    w.fresh(&[".cache/go-build/ab/new-a"]);

    let r = w.run(&["clean", "--yes"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!w.exists(".cache/go-build/ab/old-a"));
    assert!(w.exists(".cache/go-build/ab/new-a"));
}

#[test]
fn the_category_filter_limits_what_is_cleaned() {
    let w = World::new();
    w.node("code/old");
    w.old(&[".cache/go-build/ab/old-a"]);

    w.run(&["clean", "--yes", "--only", "caches"]);

    assert!(w.exists("code/old/node_modules"));
    assert!(!w.exists(".cache/go-build/ab/old-a"));
}

// --- unattended runs ---------------------------------------------------------------------

#[test]
fn an_unattended_run_follows_the_rules_in_the_configuration() {
    let w = World::new();
    w.node("code/old");
    w.old(&[
        "code/crate/Cargo.toml",
        "code/crate/target/CACHEDIR.TAG",
        "code/py/requirements.txt",
        "code/py/.venv/pyvenv.cfg",
        ".cache/go-build/ab/old-a",
    ]);
    w.config(
        "[projects.kind.rust]\nmode = \"ask\"\n\
         [projects.kind.python]\nmode = \"off\"\n\
         [caches]\nmode = \"ask\"\n",
    );

    let r = w.run(&["clean", "--auto"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!w.exists("code/old/node_modules"), "auto kinds are deleted");
    assert!(w.exists("code/crate/target"), "ask kinds wait for a person");
    assert!(w.exists("code/py/.venv"), "off kinds are not touched");
    assert!(w.exists(".cache/go-build/ab/old-a"));
    assert!(
        r.out.contains("relief clean"),
        "should say what is waiting: {}",
        r.out
    );
    assert!(w.run(&["history"]).out.contains("auto"));
}

#[test]
fn a_kind_that_is_off_is_not_even_offered_interactively() {
    let w = World::new();
    w.old(&["code/py/requirements.txt", "code/py/.venv/pyvenv.cfg"]);
    w.config("[projects.kind.python]\nmode = \"off\"\n");

    let r = w.run(&["clean", "--yes"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(w.exists("code/py/.venv"));
}

#[test]
fn an_unattended_run_finishes_what_an_interrupted_one_started() {
    let w = World::new();
    w.old(&[
        "code/app/package.json",
        "code/app/.relief-trash.node_modules.9-0/dep/index.js",
    ]);

    let r = w.run(&["clean", "--auto"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!w.exists("code/app/.relief-trash.node_modules.9-0"));
    assert!(w.exists("code/app/package.json"));
}

#[test]
fn a_tool_prunes_its_own_cache_and_is_not_asked_again_until_the_cache_is_used() {
    let w = World::new();
    w.old(&[".cache/uv/CACHEDIR.TAG"]);
    w.fresh(&[".cache/uv/archive-v0/abc/mod.py"]);
    let uv_cache = w.path(".cache/uv").display().to_string();
    let log = w.path("uv.log").display().to_string();
    w.tool(
        "uv",
        &format!("if [ \"$1 $2\" = \"cache dir\" ]; then echo {uv_cache}; else echo \"$1 $2\" >> {log}; fi"),
    );

    let first = w.run(&["clean", "--auto"]);
    let second = w.run(&["clean", "--auto"]);

    assert_eq!(first.code, 0, "{}", first.err);
    assert_eq!(second.code, 0, "{}", second.err);
    assert_eq!(w.read("uv.log"), "cache prune\n", "pruned exactly once");
    assert!(
        w.exists(".cache/uv/archive-v0/abc/mod.py"),
        "relief itself never edits uv's cache"
    );
}

#[test]
fn a_failed_deletion_is_reported_and_fails_the_run() {
    let w = World::new();
    w.node("code/old");
    fs::set_permissions(w.path("code/old"), fs::Permissions::from_mode(0o555)).unwrap();

    let r = w.run(&["clean", "--yes"]);
    fs::set_permissions(w.path("code/old"), fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(r.code, 2, "{}{}", r.out, r.err);
    assert!(w.exists("code/old/node_modules/dep/index.js"));
    assert!(r.err.contains("code/old/node_modules"), "{}", r.err);
}

// --- configuration -----------------------------------------------------------------------

#[test]
fn config_init_writes_a_file_once() {
    let w = World::new();

    let first = w.run(&["config", "init"]);
    fs::write(
        w.path(".config/relief/config.toml"),
        "roots = [\"~/mine\"]\n",
    )
    .unwrap();
    let second = w.run(&["config", "init"]);

    assert_eq!(first.code, 0, "{}", first.err);
    assert_ne!(second.code, 0, "an existing file must not be overwritten");
    assert_eq!(
        w.read(".config/relief/config.toml"),
        "roots = [\"~/mine\"]\n"
    );
    assert!(
        w.run(&["config", "path"])
            .out
            .contains(".config/relief/config.toml")
    );
}

#[test]
fn a_mistake_in_the_configuration_stops_everything() {
    let w = World::new();
    w.node("code/old");
    w.config("[projects]\nidle_dayz = 1\n");

    let r = w.run(&["clean", "--yes"]);

    assert_ne!(r.code, 0);
    assert!(r.err.contains("config.toml"), "{}", r.err);
    assert!(w.exists("code/old/node_modules"));
}

#[test]
fn configured_roots_and_ignores_bound_the_search() {
    let w = World::new();
    w.node("work/in").node("work/skip").node("elsewhere/out");
    w.config("roots = [\"~/work\"]\nignore = [\"~/work/skip\"]\n");

    w.run(&["clean", "--yes"]);

    assert!(!w.exists("work/in/node_modules"));
    assert!(w.exists("work/skip/node_modules"));
    assert!(w.exists("elsewhere/out/node_modules"));
}

// --- schedule ----------------------------------------------------------------------------

const FAKE_SYSTEMCTL: &str = "echo \"$@\" >> \"$HOME/systemctl.log\"";

#[test]
fn schedule_on_installs_a_timer_that_runs_the_unattended_clean() {
    let w = World::new();
    w.tool("systemctl", FAKE_SYSTEMCTL);

    let r = w.run(&[
        "schedule", "on", "--every", "daily", "--at", "09:30", "--yes",
    ]);

    assert_eq!(r.code, 0, "{}", r.err);
    let service = w.read(".config/systemd/user/relief.service");
    let timer = w.read(".config/systemd/user/relief.timer");
    let exe = env!("CARGO_BIN_EXE_relief");
    assert!(
        service.contains(&format!("\"{exe}\" clean --auto")),
        "{service}"
    );
    assert!(timer.contains("OnCalendar=*-*-* 09:30:00"), "{timer}");
    assert!(timer.contains("Persistent=true"), "{timer}");
    let log = w.read("systemctl.log");
    assert!(log.contains("--user daemon-reload"), "{log}");
    assert!(log.contains("--user enable --now relief.timer"), "{log}");
}

#[test]
fn the_schedule_keeps_the_environment_it_was_set_up_in() {
    let w = World::new();
    w.tool("systemctl", FAKE_SYSTEMCTL);

    let r = ran(w
        .command(&["schedule", "on", "--yes"])
        .env("GOPATH", "/opt/go path")
        .stdin(Stdio::null())
        .output()
        .unwrap());

    assert_eq!(r.code, 0, "{}", r.err);
    let service = w.read(".config/systemd/user/relief.service");
    assert!(
        service.contains(&format!("PATH={}", w.path("bin").display())),
        "{service}"
    );
    assert!(service.contains("\"GOPATH=/opt/go path\""), "{service}");
}

#[test]
fn schedule_off_removes_the_timer() {
    let w = World::new();
    w.tool("systemctl", FAKE_SYSTEMCTL);
    w.run(&["schedule", "on", "--yes"]);

    let r = w.run(&["schedule", "off"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!w.exists(".config/systemd/user/relief.timer"));
    assert!(!w.exists(".config/systemd/user/relief.service"));
    assert!(
        w.read("systemctl.log")
            .contains("--user disable --now relief.timer")
    );
}

#[test]
fn schedule_needs_a_yes_when_nobody_can_confirm() {
    let w = World::new();
    w.tool("systemctl", FAKE_SYSTEMCTL);

    let r = w.run(&["schedule", "on"]);

    assert_ne!(r.code, 0);
    assert!(!w.exists(".config/systemd/user/relief.timer"));
}

const FAKE_CRONTAB: &str = "store=\"$HOME/crontab.txt\"\n\
    if [ \"$1\" = \"-l\" ]; then\n  [ -f \"$store\" ] || exit 1\n  /bin/cat \"$store\"\n\
    elif [ \"$1\" = \"-\" ]; then\n  /bin/cat > \"$store\"\nfi";

#[test]
fn without_systemd_the_schedule_goes_into_the_crontab_next_to_what_is_there() {
    let w = World::new();
    w.tool("crontab", FAKE_CRONTAB);
    fs::write(w.path("crontab.txt"), "5 4 * * * /usr/bin/backup\n").unwrap();

    let on = w.run(&[
        "schedule", "on", "--every", "weekly", "--at", "07:05", "--yes",
    ]);
    let installed = w.read("crontab.txt");
    let status = w.run(&["schedule", "status"]);
    let off = w.run(&["schedule", "off"]);

    assert_eq!(on.code, 0, "{}", on.err);
    assert!(
        installed.contains("5 4 * * * /usr/bin/backup\n"),
        "{installed}"
    );
    let ours = installed
        .lines()
        .find(|l| l.contains("clean --auto"))
        .expect(&installed);
    assert!(ours.starts_with("5 7 * * 1 "), "{ours}");
    assert!(ours.contains(env!("CARGO_BIN_EXE_relief")), "{ours}");
    assert!(status.out.contains("cron"), "{}", status.out);
    assert_eq!(off.code, 0, "{}", off.err);
    assert_eq!(w.read("crontab.txt"), "5 4 * * * /usr/bin/backup\n");
}

#[test]
fn installing_the_schedule_twice_leaves_one_entry() {
    let w = World::new();
    w.tool("crontab", FAKE_CRONTAB);

    w.run(&["schedule", "on", "--yes"]);
    w.run(&["schedule", "on", "--every", "monthly", "--yes"]);

    let lines = w.read("crontab.txt");
    assert_eq!(lines.matches("clean --auto").count(), 1, "{lines}");
    assert!(lines.contains("0 12 1 * * "), "{lines}");
}

#[test]
fn without_any_scheduler_the_user_is_told() {
    let w = World::new();

    let r = w.run(&["schedule", "on", "--yes"]);

    assert_ne!(r.code, 0);
    assert!(
        r.err.contains("systemd") && r.err.contains("cron"),
        "{}",
        r.err
    );
}

#[test]
fn status_says_when_nothing_is_scheduled() {
    let w = World::new();
    w.tool("systemctl", FAKE_SYSTEMCTL);

    let r = w.run(&["schedule", "status"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("not scheduled"), "{}", r.out);
}

// --- choosing interactively --------------------------------------------------------------

#[test]
fn typing_does_not_count_as_a_terminal() {
    // Piped input is not a person at a keyboard; deleting still needs --yes.
    let w = World::new();
    w.node("code/old");

    let r = w.run_typing(&["clean"], "a\n");

    assert_ne!(r.code, 0);
    assert!(w.exists("code/old/node_modules"));
}

// --- how it looks ------------------------------------------------------------------------

#[test]
fn output_is_plain_when_piped_and_coloured_on_request() {
    let w = World::new();
    w.node("code/old");

    let piped = w.run(&["scan"]);
    let forced = w.run(&["--color", "always", "scan"]);
    let quiet = ran(w
        .command(&["--color", "always", "scan"])
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap());

    assert!(!piped.out.contains('\x1b'), "{}", piped.out);
    assert!(forced.out.contains('\x1b'), "{}", forced.out);
    // An explicit --color always wins over NO_COLOR, as the flag is the more specific wish.
    assert!(quiet.out.contains('\x1b'));
    assert!(
        !piped.err.contains("Scanning"),
        "no progress noise when piped: {}",
        piped.err
    );
}

/// Run relief inside a pseudo-terminal, typing `keys`.
fn in_terminal(w: &World, args: &str, keys: &str) -> Option<Ran> {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: needs /usr/bin/script for a pseudo-terminal");
        return None;
    }
    let command = format!("{} {args}", env!("CARGO_BIN_EXE_relief"));
    let mut child = Command::new("/usr/bin/script")
        .args(["-qec", &command, "/dev/null"])
        .env_clear()
        .env("HOME", &w.home)
        .env("PATH", w.path("bin"))
        .env("TERM", "xterm")
        .env("RELIEF_TEST_NOW", (w.now + 100 * DAY).to_string())
        .current_dir(&w.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // Type one key at a time, slowly enough for the program to be reading when it arrives.
    // The program may exit before the last key (after `q`, say); that is not an error.
    for key in keys.chars() {
        std::thread::sleep(Duration::from_millis(300));
        let mut buffer = [0u8; 4];
        if stdin
            .write_all(key.encode_utf8(&mut buffer).as_bytes())
            .is_err()
            || stdin.flush().is_err()
        {
            break;
        }
    }
    std::thread::sleep(Duration::from_millis(400));
    drop(stdin);
    Some(ran(child.wait_with_output().unwrap()))
}

#[test]
fn in_a_terminal_ticked_items_are_deleted_after_a_yes() {
    let w = World::new();
    w.node("code/a").node("code/b");
    // Tick the first row, confirm the list, then say yes.
    let Some(r) = in_terminal(&w, "clean", " \ry\n") else {
        return;
    };

    let gone = [
        w.exists("code/a/node_modules"),
        w.exists("code/b/node_modules"),
    ];
    assert_eq!(
        gone.iter().filter(|kept| !**kept).count(),
        1,
        "exactly one deleted: {}",
        r.out
    );
    assert!(r.out.contains("Select what to delete"), "{}", r.out);
}

#[test]
fn in_a_terminal_quitting_or_declining_deletes_nothing() {
    let w = World::new();
    w.node("code/a");

    let Some(_) = in_terminal(&w, "clean", "aq") else {
        return;
    };
    assert!(w.exists("code/a/node_modules"), "q cancels");

    in_terminal(&w, "clean", "a\rn\n").unwrap();
    assert!(
        w.exists("code/a/node_modules"),
        "no at the final question cancels"
    );
}

#[test]
fn the_plain_prompt_works_in_a_terminal_without_taking_over_the_screen() {
    let w = World::new();
    w.node("code/a");

    let Some(r) = in_terminal(&w, "clean --plain", "1\n") else {
        return;
    };

    assert!(!w.exists("code/a/node_modules"), "{}", r.out);
    assert!(
        !r.out.contains("\x1b[?1049h"),
        "plain mode must not switch screens"
    );
}

// --- docker ------------------------------------------------------------------------------

/// A fake docker with one unused pulled image, one dangling image, reclaimable build cache
/// and an unused volume. Every call is logged.
fn fake_docker(w: &World) {
    let old = "2020-01-01T00:00:00Z";
    let inspect = format!(
        "{{\"Id\":\"sha256:img\",\"RepoTags\":[\"redis:7\"],\"RepoDigests\":[\"redis@sha256:1\"],\"Created\":\"{old}\",\"Metadata\":{{\"LastTagTime\":\"{old}\"}}}}\n\
         {{\"Id\":\"sha256:dang\",\"RepoTags\":[],\"RepoDigests\":[],\"Created\":\"{old}\",\"Metadata\":{{}}}}\n"
    );
    let verbose = r#"{"Images":[{"ID":"sha256:img","UniqueSize":"40MB"},{"ID":"sha256:dang","UniqueSize":"10MB"}],"Volumes":[{"Name":"v","Links":"0","Size":"5MB"}],"BuildCache":[]}"#;
    let totals = "{\"Type\":\"Build Cache\",\"Reclaimable\":\"2GB (100%)\"}\n";
    fs::write(w.path("docker-inspect"), inspect).unwrap();
    fs::write(w.path("docker-verbose"), verbose).unwrap();
    fs::write(w.path("docker-totals"), totals).unwrap();
    w.tool(
        "docker",
        r#"echo "$*" >> "$HOME/docker.log"
case "$*" in
  "info --format {{.ServerVersion}}") echo 29.0 ;;
  "image ls --all --no-trunc --quiet") printf 'sha256:img\nsha256:dang\n' ;;
  "image inspect --format {{json .}} "*) /bin/cat "$HOME/docker-inspect" ;;
  "container ls --all --no-trunc --quiet") ;;
  "system df --verbose --format {{json .}}") /bin/cat "$HOME/docker-verbose" ;;
  "system df --format {{json .}}") /bin/cat "$HOME/docker-totals" ;;
  "image rm "*|"builder prune "*) ;;
  *) exit 9 ;;
esac"#,
    );
}

fn docker_calls(w: &World, prefix: &str) -> Vec<String> {
    w.read("docker.log")
        .lines()
        .filter(|l| l.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

#[test]
fn scan_shows_what_docker_could_free_and_says_volumes_are_left_alone() {
    let w = World::new();
    fake_docker(&w);

    let r = w.run(&["scan"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("DOCKER"), "{}", r.out);
    assert!(
        r.out.contains("redis:7") && r.out.contains("docker pull redis:7"),
        "{}",
        r.out
    );
    assert!(r.out.contains("1 dangling images"), "{}", r.out);
    assert!(r.out.contains("build cache"), "{}", r.out);
    assert!(r.out.contains("volumes"), "{}", r.out);
    assert_eq!(
        docker_calls(&w, "image rm"),
        Vec::<String>::new(),
        "scan deletes nothing"
    );
}

#[test]
fn an_unattended_run_drops_docker_waste_but_asks_about_images() {
    let w = World::new();
    fake_docker(&w);

    let r = w.run(&["clean", "--auto"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(docker_calls(&w, "image rm"), ["image rm sha256:dang"]);
    assert_eq!(
        docker_calls(&w, "builder prune"),
        ["builder prune --force --filter until=720h"]
    );
    assert!(
        r.out.contains("relief clean"),
        "the image waits for a person: {}",
        r.out
    );
}

#[test]
fn clean_can_be_limited_to_docker() {
    let w = World::new();
    fake_docker(&w);
    w.node("code/old");

    let r = w.run(&["clean", "--yes", "--only", "docker"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(w.exists("code/old/node_modules"));
    assert_eq!(
        docker_calls(&w, "image rm"),
        ["image rm redis:7", "image rm sha256:dang"]
    );
    assert!(
        w.run(&["history", "--verbose"])
            .out
            .contains("docker pull redis:7")
    );
}

#[test]
fn without_docker_nothing_about_docker_is_said() {
    let w = World::new();
    w.node("code/old");

    let r = w.run(&["scan"]);

    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!r.out.contains("DOCKER"), "{}", r.out);
}
