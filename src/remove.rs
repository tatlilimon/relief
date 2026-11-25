//! Deleting, with the guards that keep a wrong path from costing anyone their work.
//!
//! An entry is first renamed to a tombstone next to it and only then deleted. The rename is
//! atomic, so whatever interrupts us, a project never sees a half-deleted `node_modules`:
//! either the directory is whole or it is gone and a clearly named leftover remains.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

/// Start of every tombstone name. Anything carrying it was already on its way out.
pub const TRASH_PREFIX: &str = ".relief-trash.";

pub fn is_trash(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(TRASH_PREFIX.as_bytes())
}

fn refuse(target: &Path, why: &str) -> io::Error {
    io::Error::other(format!("refusing to delete {}: {why}", target.display()))
}

/// Delete `target`, which must lie strictly inside `container` (the project or cache that
/// was scanned) and must not be, or contain, the home directory.
pub fn remove_entry(target: &Path, container: &Path, home: &Path) -> io::Result<()> {
    if !target.is_absolute() || !container.is_absolute() {
        return Err(refuse(target, "path is not absolute"));
    }
    if target
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(refuse(target, "path is not in its plain form"));
    }
    if target == container || !target.starts_with(container) {
        return Err(refuse(target, "it is not inside what was scanned"));
    }
    if home.starts_with(target) {
        return Err(refuse(target, "it holds the home directory"));
    }
    let md = fs::symlink_metadata(target)?;
    if md.file_type().is_symlink() {
        return Err(refuse(target, "it is a symbolic link"));
    }
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(refuse(target, "it has no parent directory"));
    };

    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let mut tombstone = OsString::from(TRASH_PREFIX);
    tombstone.push(name);
    tombstone.push(format!(
        ".{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let tombstone = parent.join(tombstone);
    fs::rename(target, &tombstone)?;
    delete(&tombstone, md.is_dir())
}

/// Finish deleting a tombstone that an interrupted run left behind.
pub fn remove_leftover(path: &Path) -> io::Result<()> {
    if !path.file_name().is_some_and(is_trash) {
        return Err(refuse(path, "it is not a relief leftover"));
    }
    let md = fs::symlink_metadata(path)?;
    delete(path, md.is_dir())
}

fn delete(path: &Path, is_dir: bool) -> io::Result<()> {
    if !is_dir {
        return fs::remove_file(path);
    }
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            // Go's module cache is read-only on purpose; open the directories and retry.
            make_writable(path);
            fs::remove_dir_all(path)
        }
        other => other,
    }
}

fn make_writable(dir: &Path) {
    let Ok(md) = fs::symlink_metadata(dir) else {
        return;
    };
    if !md.is_dir() {
        return;
    }
    let mode = md.permissions().mode();
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode | 0o700));
    if let Ok(read) = fs::read_dir(dir) {
        for entry in read.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                make_writable(&entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsmeta::testutil::write;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        fs::create_dir_all(&home).unwrap();
        Fixture { _dir: dir, home }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn removes_the_directory_and_leaves_its_siblings() {
        let f = fixture();
        let project = f.home.join("proj");
        write(&project.join("node_modules/a/b/index.js"), 10);
        write(&project.join("src/main.js"), 10);

        remove_entry(&project.join("node_modules"), &project, &f.home).unwrap();

        assert_eq!(names(&project), ["src"]);
    }

    #[test]
    fn removes_a_single_file_entry() {
        let f = fixture();
        let cache = f.home.join(".cache/go-build");
        write(&cache.join("ab/blob-d"), 10);
        write(&cache.join("ab/keep-d"), 10);

        remove_entry(&cache.join("ab/blob-d"), &cache, &f.home).unwrap();

        assert_eq!(names(&cache.join("ab")), ["keep-d"]);
    }

    #[test]
    fn refuses_the_container_itself() {
        let f = fixture();
        let project = f.home.join("proj");
        write(&project.join("src/main.js"), 10);

        assert!(remove_entry(&project, &project, &f.home).is_err());
        assert!(project.join("src/main.js").exists());
    }

    #[test]
    fn refuses_a_path_outside_the_container() {
        let f = fixture();
        write(&f.home.join("a/node_modules/x"), 10);
        write(&f.home.join("b/keep"), 10);

        assert!(remove_entry(&f.home.join("a/node_modules"), &f.home.join("b"), &f.home).is_err());
        assert!(f.home.join("a/node_modules/x").exists());
    }

    #[test]
    fn refuses_a_path_that_climbs_out_of_the_container() {
        let f = fixture();
        write(&f.home.join("a/keep"), 10);
        write(&f.home.join("b/victim/x"), 10);
        let sneaky = f.home.join("a/../b/victim");

        assert!(remove_entry(&sneaky, &f.home.join("a"), &f.home).is_err());
        assert!(f.home.join("b/victim/x").exists());
    }

    #[test]
    fn refuses_the_home_directory_and_anything_above_it() {
        let f = fixture();
        write(&f.home.join("doc.txt"), 10);
        let above = f.home.parent().unwrap().to_path_buf();
        let top = above.parent().unwrap().to_path_buf();

        assert!(remove_entry(&f.home, &above, &f.home).is_err());
        assert!(remove_entry(&above, &top, &f.home).is_err());
        assert!(f.home.join("doc.txt").exists());
    }

    #[test]
    fn refuses_a_symlink_and_leaves_what_it_points_at() {
        let f = fixture();
        let project = f.home.join("proj");
        write(&f.home.join("precious/data"), 10);
        fs::create_dir_all(&project).unwrap();
        symlink(f.home.join("precious"), project.join("node_modules")).unwrap();

        assert!(remove_entry(&project.join("node_modules"), &project, &f.home).is_err());
        assert!(f.home.join("precious/data").exists());
    }

    #[test]
    fn a_symlink_inside_the_tree_is_removed_without_touching_its_target() {
        let f = fixture();
        let project = f.home.join("proj");
        write(&f.home.join("precious/data"), 10);
        write(&project.join("node_modules/pkg/index.js"), 10);
        symlink(f.home.join("precious"), project.join("node_modules/link")).unwrap();

        remove_entry(&project.join("node_modules"), &project, &f.home).unwrap();

        assert!(!project.join("node_modules").exists());
        assert!(f.home.join("precious/data").exists());
    }

    #[test]
    fn removes_a_read_only_tree() {
        let f = fixture();
        let cache = f.home.join("go/pkg/mod");
        let module = cache.join("example.com/lib@v1.0.0");
        write(&module.join("sub/lib.go"), 10);
        for dir in [module.join("sub"), module.clone()] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        }

        remove_entry(&module, &cache, &f.home).unwrap();

        assert_eq!(names(&cache.join("example.com")), Vec::<String>::new());
    }

    #[test]
    fn a_leftover_tombstone_is_removed_but_nothing_else_is() {
        let f = fixture();
        let project = f.home.join("proj");
        let tombstone = project.join(format!("{TRASH_PREFIX}node_modules.123-0"));
        write(&tombstone.join("pkg/index.js"), 10);
        write(&project.join("src/main.js"), 10);

        remove_leftover(&tombstone).unwrap();
        assert!(remove_leftover(&project.join("src")).is_err());

        assert_eq!(names(&project), ["src"]);
    }

    #[test]
    fn tombstones_are_recognised_by_name() {
        assert!(is_trash(OsStr::new(".relief-trash.target.9-1")));
        assert!(!is_trash(OsStr::new("relief-trash")));
        assert!(!is_trash(OsStr::new(".relief")));
    }
}
