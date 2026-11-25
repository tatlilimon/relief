//! What the filesystem knows about a tree: how much disk it holds and when it was last used.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;

/// Totals for one directory tree (or one file).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Disk space that deleting the tree gives back.
    pub bytes: u64,
    /// Disk space that stays allocated after deletion, because hard links outside the tree
    /// point at the same data (pnpm, bun and uv install packages this way).
    pub shared_bytes: u64,
    /// Number of regular files.
    pub files: u64,
    /// Latest use of any file, in seconds since the epoch. 0 when no file counted.
    pub last_use: i64,
}

impl Usage {
    pub fn merge(self, other: Usage) -> Usage {
        Usage {
            bytes: self.bytes + other.bytes,
            shared_bytes: self.shared_bytes + other.shared_bytes,
            files: self.files + other.files,
            last_use: self.last_use.max(other.last_use),
        }
    }
}

/// Whose use a hard-linked file's timestamps describe.
///
/// A file with links outside the tree shares its timestamps with every other link, so they
/// cannot say whether *this* tree was used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedTimes {
    /// Leave them out: right for a project, which should not look busy because another
    /// project installed the same package.
    Ignore,
    /// Count them: right for a cache, where a link from a project means the entry is in use.
    Count,
}

#[derive(Clone, Copy, Debug)]
pub struct Measure {
    /// Whether reading a file counts as using it (needs access times on the filesystem).
    pub count_reads: bool,
    pub shared_times: SharedTimes,
}

/// Latest moment a file was modified, had its status changed or (optionally) was read.
pub fn last_use(md: &Metadata, count_reads: bool) -> i64 {
    let written = md.mtime().max(md.ctime());
    if count_reads {
        written.max(md.atime())
    } else {
        written
    }
}

/// Space a file occupies on disk, as `du` reports it.
pub fn disk_bytes(md: &Metadata) -> u64 {
    md.blocks() * 512
}

/// A file that has more than one hard link, as seen so far during one walk.
struct Link {
    seen: u64,
    nlink: u64,
    bytes: u64,
    last_use: i64,
}

struct Walk<'a> {
    m: Measure,
    dev: u64,
    skip: &'a (dyn Fn(&Path) -> bool + Sync),
    stop_at: i64,
    stop: AtomicBool,
    links: Mutex<HashMap<(u64, u64), Link>>,
}

impl Walk<'_> {
    fn dir(&self, dir: &Path) -> Usage {
        if self.stop.load(Ordering::Relaxed) {
            return Usage::default();
        }
        let Ok(read) = fs::read_dir(dir) else {
            return Usage::default();
        };
        let entries: Vec<_> = read.filter_map(Result::ok).collect();
        entries
            .par_iter()
            .map(|entry| {
                // DirEntry::metadata does not follow symbolic links.
                let Ok(md) = entry.metadata() else {
                    return Usage::default();
                };
                if md.is_dir() {
                    let path = entry.path();
                    if md.dev() != self.dev || (self.skip)(&path) {
                        return Usage::default();
                    }
                    let mut usage = self.dir(&path);
                    usage.bytes += disk_bytes(&md);
                    usage
                } else {
                    self.leaf(&md)
                }
            })
            .reduce(Usage::default, Usage::merge)
    }

    fn leaf(&self, md: &Metadata) -> Usage {
        if !md.is_file() {
            // Symbolic links, sockets and the like: only their own small allocation.
            return Usage {
                bytes: disk_bytes(md),
                ..Usage::default()
            };
        }
        let used = last_use(md, self.m.count_reads);
        if md.nlink() > 1 {
            // Whether this is reclaimable depends on where the other links are; decided in
            // `finish` once the whole tree has been seen.
            let mut links = self.links.lock().unwrap();
            let link = links.entry((md.dev(), md.ino())).or_insert(Link {
                seen: 0,
                nlink: md.nlink(),
                bytes: disk_bytes(md),
                last_use: used,
            });
            link.seen += 1;
            return Usage {
                files: 1,
                ..Usage::default()
            };
        }
        if used >= self.stop_at {
            self.stop.store(true, Ordering::Relaxed);
        }
        Usage {
            bytes: disk_bytes(md),
            shared_bytes: 0,
            files: 1,
            last_use: used,
        }
    }

    fn finish(self, mut usage: Usage) -> Usage {
        for link in self.links.into_inner().unwrap().into_values() {
            if link.seen >= link.nlink {
                usage.bytes += link.bytes;
                usage.last_use = usage.last_use.max(link.last_use);
            } else {
                usage.shared_bytes += link.bytes;
                if self.m.shared_times == SharedTimes::Count {
                    usage.last_use = usage.last_use.max(link.last_use);
                }
            }
        }
        usage
    }
}

fn walk(root: &Path, m: Measure, skip: &(dyn Fn(&Path) -> bool + Sync), stop_at: i64) -> Usage {
    let Ok(md) = fs::symlink_metadata(root) else {
        return Usage::default();
    };
    let walk = Walk {
        m,
        dev: md.dev(),
        skip,
        stop_at,
        stop: AtomicBool::new(false),
        links: Mutex::new(HashMap::new()),
    };
    let usage = if md.is_dir() {
        let mut usage = walk.dir(root);
        usage.bytes += disk_bytes(&md);
        usage
    } else {
        walk.leaf(&md)
    };
    walk.finish(usage)
}

/// Size and last use of everything under `root`. Symbolic links are not followed and other
/// filesystems are not entered.
pub fn measure_tree(root: &Path, m: Measure) -> Usage {
    walk(root, m, &|_| false, i64::MAX)
}

/// Latest use of any file under `root`, leaving out directories for which `skip` is true.
/// Stops looking once a use at or after `stop_at` is found, since the caller then knows
/// enough; pass `i64::MAX` to always look at everything.
pub fn newest_use(
    root: &Path,
    m: Measure,
    skip: &(dyn Fn(&Path) -> bool + Sync),
    stop_at: i64,
) -> i64 {
    walk(root, m, skip, stop_at).last_use
}

/// Turn the `\040`-style escapes of mountinfo back into bytes.
fn unescape_mount(field: &str) -> PathBuf {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let octal = std::str::from_utf8(&bytes[i + 1..i + 4]).ok();
            if let Some(value) = octal.and_then(|o| u8::from_str_radix(o, 8).ok()) {
                out.push(value);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(OsString::from_vec(out))
}

/// The mount holding `path` and whether it is mounted `noatime`, from mountinfo text.
pub fn parse_mountinfo(mountinfo: &str, path: &Path) -> Option<(PathBuf, bool)> {
    let mut best: Option<(PathBuf, bool)> = None;
    for line in mountinfo.lines() {
        // id parent major:minor root mount-point options ...
        let mut fields = line.split(' ');
        let (Some(point), Some(options)) = (fields.nth(4), fields.next()) else {
            continue;
        };
        let point = unescape_mount(point);
        if !path.starts_with(&point) {
            continue;
        }
        let deeper = best
            .as_ref()
            .is_none_or(|(b, _)| point.components().count() >= b.components().count());
        if deeper {
            let noatime = options.split(',').any(|o| o == "noatime");
            best = Some((point, noatime));
        }
    }
    best
}

/// The mount point of the filesystem holding `path` when it does not record reads.
pub fn noatime_mount(path: &Path) -> Option<PathBuf> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    match parse_mountinfo(&info, path)? {
        (point, true) => Some(point),
        _ => None,
    }
}

/// Working directories of the running processes we are allowed to inspect.
pub fn process_cwds() -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .as_encoded_bytes()
                .iter()
                .all(u8::is_ascii_digit)
        })
        .filter_map(|e| fs::read_link(e.path().join("cwd")).ok())
        .collect()
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::fs::{self, File, FileTimes};
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    pub const DAY: i64 = 86_400;

    pub fn now() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// Create a file (and its parent directories) holding `len` bytes.
    pub fn write(path: &Path, len: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; len]).unwrap();
    }

    fn at(secs: i64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
    }

    /// Set a file's access and modification times, in seconds since the epoch.
    pub fn set_times(path: &Path, accessed: i64, modified: i64) {
        let times = FileTimes::new()
            .set_accessed(at(accessed))
            .set_modified(at(modified));
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    const READS: Measure = Measure {
        count_reads: true,
        shared_times: SharedTimes::Ignore,
    };
    const WRITES_ONLY: Measure = Measure {
        count_reads: false,
        shared_times: SharedTimes::Ignore,
    };

    #[test]
    fn a_read_counts_as_use_only_when_reads_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        write(&file, 10);
        let t = now();
        // Read 30 days from now, modified 20 days from now: both later than the status change.
        set_times(&file, t + 30 * DAY, t + 20 * DAY);
        let md = fs::symlink_metadata(&file).unwrap();

        assert_eq!(last_use(&md, true), t + 30 * DAY);
        assert_eq!(last_use(&md, false), t + 20 * DAY);
    }

    #[test]
    fn a_status_change_counts_as_use_even_when_older_times_were_restored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        let before = now();
        write(&file, 10);
        // A copy that preserves old timestamps still arrived just now.
        set_times(&file, 1_000_000, 1_000_000);
        let md = fs::symlink_metadata(&file).unwrap();

        assert!(last_use(&md, true) >= before);
    }

    #[test]
    fn tree_size_is_the_disk_space_of_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        write(&tree.join("a/one"), 100_000);
        write(&tree.join("a/b/two"), 50_000);
        let on_disk = disk_bytes(&fs::symlink_metadata(tree.join("a/one")).unwrap())
            + disk_bytes(&fs::symlink_metadata(tree.join("a/b/two")).unwrap());

        let usage = measure_tree(&tree, READS);

        assert_eq!(usage.files, 2);
        assert_eq!(usage.shared_bytes, 0);
        // Directories add a little on some filesystems, never megabytes.
        assert!(usage.bytes >= on_disk, "{} < {}", usage.bytes, on_disk);
        assert!(usage.bytes < on_disk + 64 * 1024);
        assert!(on_disk >= 150_000, "files should occupy real blocks");
    }

    #[test]
    fn tree_last_use_is_its_most_recently_used_file() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        write(&tree.join("old"), 10);
        write(&tree.join("deep/er/new"), 10);
        let t = now();
        set_times(&tree.join("deep/er/new"), t + 5 * DAY, t + 40 * DAY);

        assert_eq!(measure_tree(&tree, READS).last_use, t + 40 * DAY);
    }

    #[test]
    fn a_file_linked_from_outside_the_tree_is_shared_and_says_nothing_about_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store/pkg");
        let tree = dir.path().join("project/node_modules");
        write(&store, 100_000);
        write(&tree.join("own"), 10);
        fs::hard_link(&store, tree.join("pkg")).unwrap();
        let t = now();
        set_times(&store, t + 90 * DAY, t + 90 * DAY);
        set_times(&tree.join("own"), t + 3 * DAY, t + 3 * DAY);
        let shared = disk_bytes(&fs::symlink_metadata(&store).unwrap());

        let usage = measure_tree(&tree, READS);

        assert_eq!(usage.shared_bytes, shared);
        assert!(
            usage.bytes < shared,
            "shared space must not count as reclaimable"
        );
        assert_eq!(usage.last_use, t + 3 * DAY);
    }

    #[test]
    fn a_cache_counts_the_use_of_files_linked_into_projects() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        write(&store.join("pkg"), 100);
        fs::create_dir_all(dir.path().join("project")).unwrap();
        fs::hard_link(store.join("pkg"), dir.path().join("project/pkg")).unwrap();
        let t = now();
        set_times(&store.join("pkg"), t + 90 * DAY, t + 90 * DAY);

        let usage = measure_tree(
            &store,
            Measure {
                count_reads: true,
                shared_times: SharedTimes::Count,
            },
        );

        assert_eq!(usage.last_use, t + 90 * DAY);
    }

    #[test]
    fn hard_links_that_all_live_inside_the_tree_are_reclaimable_once() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("target");
        write(&tree.join("deps/lib.rlib"), 200_000);
        fs::hard_link(tree.join("deps/lib.rlib"), tree.join("lib.rlib")).unwrap();
        let t = now();
        set_times(&tree.join("lib.rlib"), t + 7 * DAY, t + 7 * DAY);
        let once = disk_bytes(&fs::symlink_metadata(tree.join("lib.rlib")).unwrap());

        let usage = measure_tree(&tree, READS);

        assert_eq!(usage.shared_bytes, 0);
        assert!(usage.bytes >= once && usage.bytes < 2 * once);
        assert_eq!(usage.last_use, t + 7 * DAY);
    }

    #[test]
    fn symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let tree = dir.path().join("tree");
        write(&outside.join("big"), 500_000);
        write(&tree.join("small"), 10);
        symlink(&outside, tree.join("link")).unwrap();
        let t = now();
        set_times(&outside.join("big"), t + 50 * DAY, t + 50 * DAY);

        let usage = measure_tree(&tree, READS);

        assert_eq!(usage.files, 1);
        assert!(usage.bytes < 100_000);
        assert!(usage.last_use < t + DAY);
    }

    #[test]
    fn a_single_file_can_be_measured() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("blob");
        write(&file, 30_000);
        let t = now();
        set_times(&file, t + 2 * DAY, t + 2 * DAY);

        let usage = measure_tree(&file, WRITES_ONLY);

        assert_eq!(usage.files, 1);
        assert_eq!(
            usage.bytes,
            disk_bytes(&fs::symlink_metadata(&file).unwrap())
        );
        assert_eq!(usage.last_use, t + 2 * DAY);
    }

    #[test]
    fn newest_use_leaves_out_skipped_directories() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("proj");
        write(&unit.join("src/main.rs"), 10);
        write(&unit.join("node_modules/x/index.js"), 10);
        write(&unit.join(".git/index"), 10);
        let t = now();
        set_times(&unit.join("src/main.rs"), t + 4 * DAY, t + 4 * DAY);
        set_times(
            &unit.join("node_modules/x/index.js"),
            t + 70 * DAY,
            t + 70 * DAY,
        );
        set_times(&unit.join(".git/index"), t + 80 * DAY, t + 80 * DAY);
        let skip = |p: &Path| p.ends_with("node_modules") || p.ends_with(".git");

        assert_eq!(newest_use(&unit, READS, &skip, i64::MAX), t + 4 * DAY);
    }

    #[test]
    fn newest_use_may_stop_early_but_never_misses_a_use_past_the_stop_point() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("proj");
        for i in 0..40 {
            write(&unit.join(format!("d{i}/f")), 10);
        }
        let t = now();
        set_times(&unit.join("d7/f"), t + 60 * DAY, t + 60 * DAY);
        let none = |_: &Path| false;

        let found = newest_use(&unit, READS, &none, t + 10 * DAY);

        assert!(
            found >= t + 10 * DAY,
            "a use past the stop point must be reported"
        );
    }

    const MOUNTINFO: &str = "\
25 1 0:23 / / rw,relatime shared:1 - btrfs /dev/dm-0 rw,seclabel,ssd
60 25 0:40 /home /home rw,noatime shared:30 - btrfs /dev/dm-0 rw,seclabel,ssd
71 60 8:17 / /home/dev/My\\040Disk rw,nosuid,relatime shared:40 - ext4 /dev/sdb1 rw
";

    #[test]
    fn the_deepest_mount_holding_a_path_decides_whether_reads_are_recorded() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO, Path::new("/home/dev/code/app")),
            Some((PathBuf::from("/home"), true))
        );
        assert_eq!(
            parse_mountinfo(MOUNTINFO, Path::new("/usr/lib")),
            Some((PathBuf::from("/"), false))
        );
    }

    #[test]
    fn mount_points_with_escaped_spaces_are_decoded() {
        assert_eq!(
            parse_mountinfo(MOUNTINFO, Path::new("/home/dev/My Disk/proj")),
            Some((PathBuf::from("/home/dev/My Disk"), false))
        );
    }

    #[test]
    fn a_mount_point_is_matched_by_whole_path_components() {
        // /home/developer is not inside a mount at /home/dev...
        let info = "70 1 8:1 / /home/dev rw,noatime - ext4 /dev/sda1 rw\n\
                    25 1 0:23 / / rw,relatime - btrfs /dev/dm-0 rw\n";
        assert_eq!(
            parse_mountinfo(info, Path::new("/home/developer/x")),
            Some((PathBuf::from("/"), false))
        );
    }

    #[test]
    fn a_process_working_inside_a_directory_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&cwd)
            .spawn()
            .unwrap();

        let cwds = process_cwds();
        child.kill().unwrap();
        child.wait().unwrap();

        assert!(cwds.contains(&cwd), "{cwd:?} not among {} cwds", cwds.len());
    }
}
