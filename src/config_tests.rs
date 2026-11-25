use super::*;

fn policy(mode: Mode, days: u32) -> Policy {
    Policy { mode, days }
}

#[test]
fn without_a_file_the_defaults_apply() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::load(&dir.path().join("nope/config.toml")).unwrap();

    assert_eq!(config.roots, ["~"]);
    assert!(config.ignore.is_empty());
    assert!(config.count_reads);
    assert_eq!(config.project("node"), policy(Mode::Auto, 30));
    assert_eq!(config.cache("npm"), policy(Mode::Auto, 60));
}

#[test]
fn a_file_overrides_only_what_it_mentions() {
    let config = Config::parse(
        r#"
        roots = ["~/code", "/srv/work"]
        ignore = ["~/code/keep"]
        count_reads = false

        [projects]
        mode = "ask"

        [caches]
        unused_days = 90
        "#,
    )
    .unwrap();

    assert_eq!(config.roots, ["~/code", "/srv/work"]);
    assert_eq!(config.ignore, ["~/code/keep"]);
    assert!(!config.count_reads);
    assert_eq!(config.project("rust"), policy(Mode::Ask, 30));
    assert_eq!(config.cache("uv"), policy(Mode::Auto, 90));
}

#[test]
fn one_kind_can_have_its_own_rule() {
    let config = Config::parse(
        r#"
        [projects]
        idle_days = 45

        [projects.kind.rust]
        idle_days = 120

        [projects.kind.python]
        mode = "off"

        [caches.kind.uv]
        mode = "ask"
        "#,
    )
    .unwrap();

    assert_eq!(config.project("rust"), policy(Mode::Auto, 120));
    assert_eq!(config.project("python"), policy(Mode::Off, 45));
    assert_eq!(config.project("node"), policy(Mode::Auto, 45));
    assert_eq!(config.cache("uv"), policy(Mode::Ask, 60));
    assert_eq!(config.cache("npm"), policy(Mode::Auto, 60));
}

#[test]
fn a_misspelled_setting_is_an_error_not_a_silent_default() {
    let err = Config::parse("[projects]\nidle_dayz = 400\n").unwrap_err();
    assert!(format!("{err:#}").contains("idle_dayz"), "{err:#}");

    assert!(Config::parse("rootz = []\n").is_err());
    assert!(Config::parse("[projects]\nmode = \"automatic\"\n").is_err());
}

#[test]
fn a_rule_for_an_unknown_kind_is_an_error() {
    let err = Config::parse("[projects.kind.rsut]\nidle_days = 5\n").unwrap_err();
    assert!(format!("{err:#}").contains("rsut"), "{err:#}");
    // A cache kind is not a project kind.
    assert!(Config::parse("[projects.kind.npm]\nmode = \"off\"\n").is_err());
    assert!(Config::parse("[caches.kind.npm]\nmode = \"off\"\n").is_ok());
}

#[test]
fn zero_days_is_refused() {
    // It would delete things used today.
    assert!(Config::parse("[projects]\nidle_days = 0\n").is_err());
    assert!(Config::parse("[caches.kind.npm]\nunused_days = 0\n").is_err());
    assert!(Config::parse("[projects]\nidle_days = 1\n").is_ok());
}

#[test]
fn a_broken_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, "roots = [").unwrap();

    let err = Config::load(&file).unwrap_err();
    assert!(format!("{err:#}").contains("config.toml"), "{err:#}");
}

#[test]
fn the_template_is_a_valid_file_that_changes_nothing() {
    assert!(TEMPLATE.contains("[projects]"));
    let (template, empty) = (Config::parse(TEMPLATE).unwrap(), Config::parse("").unwrap());
    assert_eq!(template.roots, empty.roots);
    assert_eq!(template.ignore, empty.ignore);
    assert_eq!(template.count_reads, empty.count_reads);
    for kind in crate::rules::kinds() {
        assert_eq!(template.project(kind), empty.project(kind));
    }
    for kind in crate::caches::KINDS {
        assert_eq!(template.cache(kind), empty.cache(kind));
    }
    for kind in crate::docker::KINDS {
        assert_eq!(template.docker(kind), empty.docker(kind));
    }
}

#[test]
fn docker_defaults_delete_only_pure_waste_unattended() {
    let config = Config::parse("").unwrap();

    assert_eq!(config.docker("build-cache"), policy(Mode::Auto, 30));
    assert_eq!(config.docker("dangling-images"), policy(Mode::Auto, 7));
    // A pulled image is cheap to get back but slow on a bad connection: ask first.
    assert_eq!(config.docker("images"), policy(Mode::Ask, 60));
}

#[test]
fn large_downloads_are_asked_about_by_default() {
    let config = Config::parse("").unwrap();

    assert_eq!(config.cache("huggingface").mode, Mode::Ask);
    assert_eq!(config.cache("cargo").mode, Mode::Auto);
}

#[test]
fn the_docker_section_overrides_like_the_others() {
    let config = Config::parse(
        "[docker]\nmode = \"off\"\n[docker.kind.images]\nmode = \"auto\"\nunused_days = 120\n",
    )
    .unwrap();

    assert_eq!(config.docker("images"), policy(Mode::Auto, 120));
    assert_eq!(config.docker("build-cache"), policy(Mode::Off, 30));
    assert!(Config::parse("[docker.kind.volumes]\nmode = \"auto\"\n").is_err());
}
