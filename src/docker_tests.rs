use super::*;
use crate::proc::testutil::script;
use chrono::{TimeZone, Utc};
use std::fs;
use std::path::Path;

const DAY: i64 = 86_400;
const NOW: i64 = 1_790_000_000;

fn rfc3339(days_ago: i64) -> String {
    Utc.timestamp_opt(NOW - days_ago * DAY, 0)
        .unwrap()
        .to_rfc3339()
}

struct Image<'a> {
    id: &'a str,
    tags: &'a [&'a str],
    digests: &'a [&'a str],
    created_days_ago: i64,
    tagged_days_ago: Option<i64>,
    unique: &'a str,
}

/// A fake docker answering from fixtures.
fn fake(
    dir: &Path,
    images: &[Image],
    containers_use: &[&str],
    build_reclaimable: &str,
    volumes: &str,
) -> PathBuf {
    let inspect: Vec<String> = images
        .iter()
        .map(|i| {
            serde_json::json!({
                "Id": i.id,
                "RepoTags": i.tags,
                "RepoDigests": i.digests,
                "Created": rfc3339(i.created_days_ago),
                "Size": 999_999_999u64,
                "Metadata": {"LastTagTime": i.tagged_days_ago.map_or("0001-01-01T00:00:00Z".to_string(), rfc3339)},
            })
            .to_string()
        })
        .collect();
    let df_images: Vec<serde_json::Value> = images
        .iter()
        .map(|i| serde_json::json!({"ID": i.id, "UniqueSize": i.unique, "Size": "1GB"}))
        .collect();
    let verbose = serde_json::json!({"Images": df_images, "Containers": [], "Volumes": serde_json::from_str::<serde_json::Value>(volumes).unwrap(), "BuildCache": []});
    let totals = [
        serde_json::json!({"Type": "Images", "Reclaimable": "1GB (50%)"}),
        serde_json::json!({"Type": "Build Cache", "Reclaimable": build_reclaimable}),
    ];
    fs::write(
        dir.join("ids"),
        images
            .iter()
            .map(|i| format!("{}\n", i.id))
            .collect::<String>(),
    )
    .unwrap();
    fs::write(dir.join("inspect"), inspect.join("\n") + "\n").unwrap();
    fs::write(dir.join("verbose"), verbose.to_string()).unwrap();
    fs::write(
        dir.join("totals"),
        totals.iter().map(|t| format!("{t}\n")).collect::<String>(),
    )
    .unwrap();
    let containers: String = (0..containers_use.len())
        .map(|n| format!("c{n}\n"))
        .collect();
    fs::write(dir.join("containers"), containers).unwrap();
    fs::write(
        dir.join("container-images"),
        containers_use
            .iter()
            .map(|i| format!("{i}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let d = dir.display();
    script(
        dir,
        "docker",
        &format!(
            r#"case "$*" in
  "info --format {{{{.ServerVersion}}}}") echo 29.0 ;;
  "image ls --all --no-trunc --quiet") /bin/cat {d}/ids ;;
  "image inspect --format {{{{json .}}}} "*) /bin/cat {d}/inspect ;;
  "container ls --all --no-trunc --quiet") /bin/cat {d}/containers ;;
  "container inspect --format {{{{.Image}}}} "*) /bin/cat {d}/container-images ;;
  "system df --verbose --format {{{{json .}}}}") /bin/cat {d}/verbose ;;
  "system df --format {{{{json .}}}}") /bin/cat {d}/totals ;;
  *) echo "unexpected: $*" >&2; exit 9 ;;
esac"#
        ),
    )
}

fn scan_with(docker: PathBuf, days: &(dyn Fn(&str) -> Option<u32> + Sync)) -> Report {
    scan(&Scan {
        docker,
        now: NOW,
        days,
    })
    .expect("docker answered")
}

fn defaults(kind: &str) -> Option<u32> {
    Some(match kind {
        "dangling-images" => 7,
        "build-cache" => 30,
        _ => 60,
    })
}

fn kinds(report: &Report) -> Vec<(&str, &str)> {
    report
        .findings
        .iter()
        .map(|f| (f.kind, f.name.as_str()))
        .collect()
}

const PULLED: Image = Image {
    id: "sha256:aaa",
    tags: &["postgres:16"],
    digests: &["postgres@sha256:111"],
    created_days_ago: 400,
    tagged_days_ago: Some(90),
    unique: "297.2MB",
};

#[test]
fn sizes_are_read_in_docker_s_decimal_units() {
    assert_eq!(parse_size("0B"), Some(0));
    assert_eq!(parse_size("63B"), Some(63));
    assert_eq!(parse_size("1.5kB"), Some(1500));
    assert_eq!(parse_size("297.2MB"), Some(297_200_000));
    assert_eq!(parse_size("2GB"), Some(2_000_000_000));
    assert_eq!(parse_size("354.1MB (79%)"), Some(354_100_000));
    assert_eq!(parse_size("1.1TB"), Some(1_100_000_000_000));
    assert_eq!(parse_size("N/A"), None);
    assert_eq!(parse_size("12XB"), None);
}

#[test]
fn a_pulled_image_no_container_uses_can_go_and_be_pulled_again() {
    let dir = tempfile::tempdir().unwrap();
    let docker = fake(dir.path(), &[PULLED], &[], "0B", "[]");

    let report = scan_with(docker, &defaults);

    assert_eq!(kinds(&report), [("images", "postgres:16")]);
    let image = &report.findings[0];
    assert_eq!(
        image.bytes,
        Some(297_200_000),
        "unique size, not the shared total"
    );
    assert_eq!(
        image.days,
        Some(90),
        "time since the last pull, not since the build"
    );
    assert_eq!(image.commands, [["image", "rm", "postgres:16"]]);
    assert_eq!(image.regen, "docker pull postgres:16");
}

#[test]
fn an_image_any_container_uses_is_kept_even_when_the_container_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let docker = fake(dir.path(), &[PULLED], &["sha256:aaa"], "0B", "[]");

    assert_eq!(kinds(&scan_with(docker, &defaults)), []);
}

#[test]
fn an_image_used_recently_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = Image {
        tagged_days_ago: Some(10),
        ..PULLED
    };
    let docker = fake(dir.path(), &[fresh], &[], "0B", "[]");

    assert_eq!(kinds(&scan_with(docker, &defaults)), []);
}

#[test]
fn an_image_built_here_is_never_offered_because_it_cannot_be_pulled_back() {
    let dir = tempfile::tempdir().unwrap();
    let built = Image {
        tags: &["myapp:dev"],
        digests: &[],
        ..PULLED
    };
    let docker = fake(dir.path(), &[built], &[], "0B", "[]");

    let report = scan_with(docker, &defaults);

    assert_eq!(kinds(&report), []);
}

#[test]
fn every_tag_of_an_image_is_removed_and_none_is_forced() {
    let dir = tempfile::tempdir().unwrap();
    let twice = Image {
        tags: &["postgres:16", "postgres:latest"],
        ..PULLED
    };
    let docker = fake(dir.path(), &[twice], &[], "0B", "[]");

    let report = scan_with(docker, &defaults);

    assert_eq!(
        report.findings[0].commands,
        [
            ["image", "rm", "postgres:16"],
            ["image", "rm", "postgres:latest"]
        ]
    );
    assert!(
        report.findings[0]
            .commands
            .iter()
            .flatten()
            .all(|a| !a.starts_with('-'))
    );
}

#[test]
fn dangling_images_are_gathered_into_one_finding() {
    let dir = tempfile::tempdir().unwrap();
    let old = |id| Image {
        id,
        tags: &[],
        digests: &[],
        created_days_ago: 30,
        tagged_days_ago: None,
        unique: "100MB",
    };
    let young = Image {
        id: "sha256:new",
        tags: &[],
        digests: &[],
        created_days_ago: 1,
        tagged_days_ago: None,
        unique: "5MB",
    };
    let images = [old("sha256:d1"), old("sha256:d2"), young];
    let docker = fake(dir.path(), &images, &[], "0B", "[]");

    let report = scan_with(docker, &defaults);

    assert_eq!(kinds(&report), [("dangling-images", "2 dangling images")]);
    let dangling = &report.findings[0];
    assert_eq!(dangling.bytes, Some(200_000_000));
    assert_eq!(
        dangling.commands,
        [["image", "rm", "sha256:d1"], ["image", "rm", "sha256:d2"]]
    );
}

#[test]
fn old_build_cache_is_pruned_by_docker_with_the_configured_age() {
    let dir = tempfile::tempdir().unwrap();
    let docker = fake(dir.path(), &[], &[], "1.2GB (100%)", "[]");

    let report = scan_with(docker, &defaults);

    assert_eq!(kinds(&report), [("build-cache", "build cache")]);
    let cache = &report.findings[0];
    assert_eq!(
        cache.bytes, None,
        "docker cannot say how much of it is older than the cutoff"
    );
    assert_eq!(
        cache.commands,
        [["builder", "prune", "--force", "--filter", "until=720h"]]
    );
}

#[test]
fn no_reclaimable_build_cache_means_nothing_to_prune() {
    let dir = tempfile::tempdir().unwrap();
    let docker = fake(dir.path(), &[], &[], "0B", "[]");

    assert_eq!(kinds(&scan_with(docker, &defaults)), []);
}

#[test]
fn a_kind_that_is_off_is_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let docker = fake(dir.path(), &[PULLED], &[], "1GB (100%)", "[]");

    let report = scan_with(docker, &|kind| (kind != "images").then_some(30));

    assert_eq!(kinds(&report), [("build-cache", "build cache")]);
}

#[test]
fn unused_volumes_are_only_mentioned_never_offered() {
    let dir = tempfile::tempdir().unwrap();
    let volumes = r#"[{"Name":"a","Links":"0","Size":"300MB"},{"Name":"b","Links":"0","Size":"54.1MB"},{"Name":"c","Links":"1","Size":"9GB"}]"#;
    let docker = fake(dir.path(), &[], &[], "0B", volumes);

    let report = scan_with(docker, &defaults);

    assert_eq!(kinds(&report), []);
    assert_eq!(report.notes.len(), 1, "{:?}", report.notes);
    assert!(
        report.notes[0].contains("2 docker volumes"),
        "{}",
        report.notes[0]
    );
    assert!(
        report.notes[0].contains("354 MB") || report.notes[0].contains("338 MB"),
        "{}",
        report.notes[0]
    );
}

#[test]
fn a_daemon_that_does_not_answer_means_no_docker() {
    let dir = tempfile::tempdir().unwrap();
    let docker = script(dir.path(), "docker", "echo 'permission denied' >&2; exit 1");

    assert_eq!(
        scan(&Scan {
            docker,
            now: NOW,
            days: &defaults
        }),
        None
    );
}
