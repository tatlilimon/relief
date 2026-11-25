//! Running other programs without letting them hang us.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Start a program. A file that was written a moment ago can still be open for writing in
/// another process for an instant, which makes the kernel refuse to run it; wait that out.
fn spawn(command: &mut Command) -> io::Result<Child> {
    let mut tries = 0;
    loop {
        match command.spawn() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

/// Wait for a child, killing it when it takes longer than `timeout`.
fn wait(mut child: Child, timeout: Duration) -> io::Result<(Child, ExitStatus)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((child, status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "program did not finish in time",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Run a program and return its standard output, or `None` when it fails, prints nothing
/// useful, or does not finish in time.
pub fn output(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    output_of(Command::new(program).args(args), timeout)
}

/// [`output`], with the program started in `dir` instead of wherever relief was started.
pub fn output_in(dir: &Path, program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    output_of(Command::new(program).args(args).current_dir(dir), timeout)
}

fn output_of(command: &mut Command, timeout: Duration) -> Option<String> {
    let child = spawn(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .ok()?;
    // The answers we ask for are a single path, far below what a pipe buffers.
    let (mut child, status) = wait(child, timeout).ok()?;
    if !status.success() {
        return None;
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Run a program to completion, discarding its output. `Ok(true)` when it succeeded.
pub fn run(program: &Path, args: &[String], timeout: Duration) -> io::Result<bool> {
    run_of(Command::new(program).args(args), timeout)
}

/// [`run`], with the program started in `dir` instead of wherever relief was started.
pub fn run_in(dir: &Path, program: &Path, args: &[String], timeout: Duration) -> io::Result<bool> {
    run_of(Command::new(program).args(args).current_dir(dir), timeout)
}

fn run_of(command: &mut Command, timeout: Duration) -> io::Result<bool> {
    let child = spawn(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )?;
    Ok(wait(child, timeout)?.1.success())
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Put an executable shell script named `name` into `dir`.
    pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let file = dir.join(name);
        fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        file
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::script;
    use super::*;
    use std::time::Instant;

    #[test]
    fn returns_what_the_program_printed() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "tool", "echo \"got $1 $2\"");

        let out = output(&tool, &["a", "b"], Duration::from_secs(5));

        assert_eq!(out.as_deref(), Some("got a b"));
    }

    #[test]
    fn a_failing_program_gives_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "tool", "echo partial; exit 3");

        assert_eq!(output(&tool, &[], Duration::from_secs(5)), None);
        assert_eq!(
            output(&dir.path().join("missing"), &[], Duration::from_secs(5)),
            None
        );
    }

    #[test]
    fn a_program_that_hangs_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let tool = script(dir.path(), "tool", "sleep 30");
        let started = Instant::now();

        assert_eq!(output(&tool, &[], Duration::from_millis(200)), None);
        assert!(run(&tool, &[], Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn run_reports_success_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "ok", "test \"$1\" = prune");
        let args = ["prune".to_string()];

        assert!(run(&ok, &args, Duration::from_secs(5)).unwrap());
        assert!(!run(&ok, &[], Duration::from_secs(5)).unwrap());
    }
}
