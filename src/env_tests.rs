use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn home() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().display().to_string();
    (dir, path)
}

#[test]
fn files_live_in_the_xdg_default_places() {
    let (_dir, h) = home();
    let env = Env::from_vars(vars(&[("HOME", &h)]), 5).unwrap();

    assert_eq!(
        env.config_file,
        Path::new(&h).join(".config/relief/config.toml")
    );
    assert_eq!(env.state_dir, Path::new(&h).join(".local/state/relief"));
    assert_eq!(env.cache_home(), Path::new(&h).join(".cache"));
    assert_eq!(env.data_home(), Path::new(&h).join(".local/share"));
    assert_eq!(env.now, 5);
}

#[test]
fn xdg_variables_and_the_config_override_are_honoured() {
    let (_dir, h) = home();
    let env = Env::from_vars(
        vars(&[
            ("HOME", &h),
            ("XDG_CONFIG_HOME", "/x/config"),
            ("XDG_STATE_HOME", "/x/state"),
            ("XDG_CACHE_HOME", "/x/cache"),
            ("XDG_DATA_HOME", ""),
        ]),
        0,
    )
    .unwrap();
    assert_eq!(env.config_file, Path::new("/x/config/relief/config.toml"));
    assert_eq!(env.state_dir, Path::new("/x/state/relief"));
    assert_eq!(env.cache_home(), Path::new("/x/cache"));
    // Set but empty means unset.
    assert_eq!(env.data_home(), Path::new(&h).join(".local/share"));

    let env = Env::from_vars(vars(&[("HOME", &h), ("RELIEF_CONFIG", "/etc/r.toml")]), 0).unwrap();
    assert_eq!(env.config_file, Path::new("/etc/r.toml"));
}

#[test]
fn a_missing_home_is_an_error() {
    assert!(Env::from_vars(vars(&[]), 0).is_err());
    assert!(Env::from_vars(vars(&[("HOME", "/definitely/not/here")]), 0).is_err());
}

#[test]
fn the_test_clock_replaces_the_real_one() {
    let (_dir, h) = home();
    let env = Env::from_vars(vars(&[("HOME", &h), ("RELIEF_TEST_NOW", "1234")]), 99).unwrap();
    assert_eq!(env.now, 1234);
}

#[test]
fn which_finds_the_first_executable_on_path() {
    let (dir, h) = home();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
    }
    // Present but not executable in the first directory, executable in the second.
    fs::write(a.join("tool"), "").unwrap();
    fs::write(b.join("tool"), "").unwrap();
    fs::set_permissions(b.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", a.display(), b.display());
    let env = Env::from_vars(vars(&[("HOME", &h), ("PATH", &path)]), 0).unwrap();

    assert_eq!(env.which("tool"), Some(b.join("tool")));
    assert_eq!(env.which("missing"), None);
}

#[test]
fn configured_paths_expand_the_home_directory() {
    let (_dir, h) = home();
    let env = Env::from_vars(vars(&[("HOME", &h)]), 0).unwrap();

    assert_eq!(env.expand("~"), Path::new(&h));
    assert_eq!(env.expand("~/code"), Path::new(&h).join("code"));
    assert_eq!(env.expand("/opt/x"), Path::new("/opt/x"));
    assert_eq!(env.expand("~other/x"), Path::new("~other/x"));
}

#[test]
fn shown_paths_abbreviate_the_home_directory() {
    let (_dir, h) = home();
    let env = Env::from_vars(vars(&[("HOME", &h)]), 0).unwrap();

    assert_eq!(env.tilde(&Path::new(&h).join("code/app")), "~/code/app");
    assert_eq!(env.tilde(Path::new(&h)), "~");
    assert_eq!(env.tilde(Path::new("/opt/x")), "/opt/x");
}
