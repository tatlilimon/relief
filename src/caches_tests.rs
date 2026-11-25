use super::*;
use crate::fsmeta::testutil::{DAY, now, set_times, write};
use crate::proc::testutil::script;
use crate::remove::TRASH_PREFIX;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    vars: HashMap<String, String>,
}

impl Machine {
    /// A home directory with nothing on PATH, seen 100 days from now.
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        fs::create_dir_all(home.join("bin")).unwrap();
        let mut vars = HashMap::new();
        vars.insert("HOME".to_string(), home.display().to_string());
        vars.insert("PATH".to_string(), home.join("bin").display().to_string());
        Machine {
            _dir: dir,
            home,
            vars,
        }
    }

    fn set(&mut self, key: &str, value: &str) -> &mut Machine {
        self.vars.insert(key.to_string(), value.to_string());
        self
    }

    /// Create files that will look 100 days old.
    fn old(&self, paths: &[&str]) -> &Machine {
        for p in paths {
            write(&self.home.join(p), 3000);
        }
        self
    }

    /// Create files that will look 5 days old.
    fn fresh(&self, paths: &[&str]) -> &Machine {
        let t = now() + 95 * DAY;
        for p in paths {
            write(&self.home.join(p), 3000);
            set_times(&self.home.join(p), t, t);
        }
        self
    }

    fn tool(&self, name: &str, body: &str) -> PathBuf {
        script(&self.home.join("bin"), name, body)
    }

    fn scan_with(&self, days: &(dyn Fn(&str) -> Option<u32> + Sync)) -> Vec<Finding> {
        let env = Env::from_vars(self.vars.clone(), now() + 100 * DAY).unwrap();
        scan(&Scan {
            env: &env,
            days,
            count_reads: true,
        })
    }

    fn scan(&self) -> Vec<Finding> {
        self.scan_with(&|_| Some(60))
    }

    fn only(&self, kind: &str) -> Finding {
        let mut found: Vec<Finding> = self.scan().into_iter().filter(|f| f.kind == kind).collect();
        assert_eq!(found.len(), 1, "expected one {kind} cache");
        found.remove(0)
    }

    /// Stale entries of a cache, relative to the home directory, sorted.
    fn stale(&self, kind: &str) -> Vec<String> {
        let mut paths: Vec<String> = self
            .only(kind)
            .stale
            .iter()
            .map(|p| p.strip_prefix(&self.home).unwrap().display().to_string())
            .collect();
        paths.sort();
        paths
    }
}

#[test]
fn go_build_entries_expire_one_by_one() {
    let m = Machine::new();
    m.old(&[
        ".cache/go-build/README",
        ".cache/go-build/ab/old-a",
        ".cache/go-build/cd/old-d",
    ]);
    m.fresh(&[".cache/go-build/ab/new-a"]);

    assert_eq!(
        m.stale("go-build"),
        [".cache/go-build/ab/old-a", ".cache/go-build/cd/old-d"]
    );
}

#[test]
fn a_go_module_is_dropped_whole_or_not_at_all() {
    let m = Machine::new();
    m.old(&[
        "go/pkg/mod/cache/download/example.com/old/@v/v1.0.0.zip",
        "go/pkg/mod/example.com/old@v1.0.0/a.go",
        "go/pkg/mod/example.com/old@v1.0.0/sub/b.go",
        "go/pkg/mod/example.com/half@v2.0.0/stale.go",
    ]);
    m.fresh(&[
        "go/pkg/mod/example.com/half@v2.0.0/used.go",
        "go/pkg/mod/cache/download/example.com/half/@v/v2.0.0.zip",
    ]);

    assert_eq!(
        m.stale("go-mod"),
        [
            "go/pkg/mod/cache/download/example.com/old/@v/v1.0.0.zip",
            "go/pkg/mod/example.com/old@v1.0.0",
        ]
    );
}

#[test]
fn npx_installs_expire_singly_but_the_npm_content_store_only_as_a_whole() {
    let m = Machine::new();
    m.old(&[
        ".npm/_npx/aaa/node_modules/x/index.js",
        ".npm/_cacache/content-v2/sha512/00/old",
        ".npm/_cacache/index-v5/00/old",
    ]);
    m.fresh(&[
        ".npm/_npx/bbb/package.json",
        ".npm/_cacache/content-v2/sha512/11/new",
    ]);

    assert_eq!(m.stale("npm"), [".npm/_npx/aaa"]);
}

#[test]
fn an_abandoned_npm_content_store_is_emptied() {
    let m = Machine::new();
    m.old(&[
        ".npm/_cacache/content-v2/sha512/00/old",
        ".npm/_cacache/index-v5/00/old",
    ]);

    assert_eq!(
        m.stale("npm"),
        [".npm/_cacache/content-v2", ".npm/_cacache/index-v5"]
    );
}

#[test]
fn yarn_packages_and_playwright_browsers_expire_one_by_one() {
    let m = Machine::new();
    m.old(&[
        ".cache/yarn/v6/npm-old-1.0.0-integrity/node_modules/old/index.js",
        ".cache/ms-playwright/chromium-1000/chrome",
        ".cache/ms-playwright/.links/abc",
    ]);
    m.fresh(&[
        ".cache/yarn/v6/npm-new-2.0.0-integrity/node_modules/new/index.js",
        ".cache/ms-playwright/chromium-1200/chrome",
    ]);

    assert_eq!(m.stale("yarn"), [".cache/yarn/v6/npm-old-1.0.0-integrity"]);
    assert_eq!(
        m.stale("playwright"),
        [".cache/ms-playwright/chromium-1000"]
    );
}

#[test]
fn one_recent_use_keeps_an_all_or_nothing_cache() {
    let m = Machine::new();
    m.old(&[
        ".cache/pip/http-v2/a/b/old",
        ".bun/install/cache/old@1.0.0/index.js",
    ]);
    m.fresh(&[".cache/pip/wheels/x/new.whl"]);

    assert_eq!(m.stale("pip"), Vec::<String>::new());
    assert_eq!(m.stale("bun"), [".bun/install/cache/old@1.0.0"]);
}

#[test]
fn stale_bytes_are_the_disk_space_of_the_stale_entries() {
    let m = Machine::new();
    m.old(&[".cache/go-build/ab/old-a", ".cache/go-build/cd/old-d"]);
    m.fresh(&[".cache/go-build/ab/new-a"]);
    let one = fs::metadata(m.home.join(".cache/go-build/ab/old-a")).unwrap();

    let found = m.only("go-build");

    assert_eq!(found.stale_bytes, 2 * crate::fsmeta::disk_bytes(&one));
    assert!(found.total.bytes >= 3 * crate::fsmeta::disk_bytes(&one));
    assert_eq!(found.root, m.home.join(".cache/go-build"));
}

#[test]
fn the_number_of_days_decides_what_is_stale() {
    let m = Machine::new();
    m.old(&[".cache/go-build/ab/old-a"]);
    m.fresh(&[".cache/go-build/ab/new-a"]);
    let stale = |days: u32| {
        let found = m.scan_with(&move |_| Some(days));
        found
            .iter()
            .find(|f| f.kind == "go-build")
            .unwrap()
            .stale
            .len()
    };

    assert_eq!(stale(101), 0);
    assert_eq!(stale(60), 1);
    assert_eq!(stale(3), 2);
}

#[test]
fn a_kind_that_is_switched_off_is_not_scanned() {
    let m = Machine::new();
    m.old(&[".cache/go-build/ab/old-a", ".cache/pip/http-v2/a/old"]);

    let found = m.scan_with(&|kind| (kind != "pip").then_some(60));

    let kinds: Vec<&str> = found.iter().map(|f| f.kind).collect();
    assert_eq!(kinds, ["go-build"]);
}

#[test]
fn environment_variables_move_a_cache() {
    let mut m = Machine::new();
    m.old(&["elsewhere/build/ab/old-a", ".cache/go-build/ab/ignored-a"]);
    let moved = m.home.join("elsewhere/build").display().to_string();
    m.set("GOCACHE", &moved);

    assert_eq!(m.stale("go-build"), ["elsewhere/build/ab/old-a"]);
}

#[test]
fn the_tool_itself_is_asked_where_its_cache_is() {
    let m = Machine::new();
    m.old(&[
        "custom/modcache/cache/download/x/@v/v1.zip",
        "go/pkg/mod/cache/download/y/@v/v1.zip",
    ]);
    let custom = m.home.join("custom/modcache");
    m.tool(
        "go",
        &format!(
            "test \"$1 $2\" = \"env GOMODCACHE\" && echo {}",
            custom.display()
        ),
    );

    assert_eq!(m.only("go-mod").root, custom);
}

#[test]
fn a_directory_that_does_not_look_like_the_cache_is_left_alone() {
    let m = Machine::new();
    m.old(&[
        ".cache/pip/my-notes.txt",
        "go/pkg/mod/readme.txt",
        ".cache/go-build/notes/x",
    ]);

    assert_eq!(m.scan().len(), 0);
}

#[test]
fn uv_is_only_ever_pruned_by_uv() {
    let m = Machine::new();
    m.old(&[
        ".cache/uv/CACHEDIR.TAG",
        ".cache/uv/archive-v0/abc/pkg/__init__.py",
    ]);
    m.fresh(&[".cache/uv/wheels-v5/index/x.msgpack"]);

    let without_uv = m.only("uv");
    let uv = m.tool(
        "uv",
        &format!("echo {}", m.home.join(".cache/uv").display()),
    );
    let with_uv = m.only("uv");

    assert_eq!(without_uv.prune, None);
    assert!(
        with_uv.stale.is_empty(),
        "a cache in use is never edited by hand"
    );
    assert_eq!(
        with_uv.prune,
        Some(Prune {
            program: uv,
            args: vec!["cache".to_string(), "prune".to_string()],
            estimate: None,
        })
    );
}

#[test]
fn pnpm_prune_is_estimated_from_the_files_no_project_links_to() {
    let m = Machine::new();
    let store = m.home.join(".local/share/pnpm/store/v3");
    m.old(&[".local/share/pnpm/store/v3/files/00/orphan"]);
    m.fresh(&[".local/share/pnpm/store/v3/files/11/linked"]);
    fs::create_dir_all(m.home.join("proj")).unwrap();
    fs::hard_link(store.join("files/11/linked"), m.home.join("proj/linked")).unwrap();
    m.tool("pnpm", &format!("echo {}", store.display()));
    let orphan = crate::fsmeta::disk_bytes(&fs::metadata(store.join("files/00/orphan")).unwrap());

    let found = m.only("pnpm");
    let estimate = found.prune.unwrap().estimate.unwrap();

    assert_eq!(found.root, store);
    assert!(
        estimate >= orphan && estimate < 2 * orphan,
        "{estimate} vs {orphan}"
    );
}

#[test]
fn a_leftover_tombstone_in_a_cache_is_always_stale() {
    let m = Machine::new();
    let tombstone = format!("go/pkg/mod/example.com/{TRASH_PREFIX}lib@v1.0.0.7-0/a.go");
    m.old(&["go/pkg/mod/cache/download/x/@v/list"]);
    m.fresh(&[&tombstone]);

    assert!(
        m.stale("go-mod").contains(
            &Path::new(&tombstone)
                .parent()
                .unwrap()
                .display()
                .to_string()
        )
    );
}

// --- more tools --------------------------------------------------------------------------

#[test]
fn cargo_crates_and_their_sources_expire_one_by_one() {
    let m = Machine::new();
    m.old(&[
        ".cargo/registry/index/index.crates.io-1/config.json",
        ".cargo/registry/cache/index.crates.io-1/old-1.0.0.crate",
        ".cargo/registry/src/index.crates.io-1/old-1.0.0/src/lib.rs",
        ".cargo/git/checkouts/dep-abc/rev1/lib.rs",
    ]);
    m.fresh(&[
        ".cargo/registry/cache/index.crates.io-1/new-2.0.0.crate",
        ".cargo/registry/src/index.crates.io-1/new-2.0.0/src/lib.rs",
    ]);

    assert_eq!(
        m.stale("cargo"),
        [
            ".cargo/git/checkouts/dep-abc",
            ".cargo/registry/cache/index.crates.io-1/old-1.0.0.crate",
            ".cargo/registry/src/index.crates.io-1/old-1.0.0",
        ]
    );
}

#[test]
fn cargo_home_can_be_moved() {
    let mut m = Machine::new();
    m.old(&["rust/cargo/registry/cache/idx/old-1.0.0.crate"]);
    let moved = m.home.join("rust/cargo").display().to_string();
    m.set("CARGO_HOME", &moved);

    assert_eq!(
        m.stale("cargo"),
        ["rust/cargo/registry/cache/idx/old-1.0.0.crate"]
    );
}

#[test]
fn downloaded_browsers_and_headers_expire_per_version() {
    let m = Machine::new();
    m.old(&[
        ".cache/Cypress/13.1.0/Cypress/cypress",
        ".cache/puppeteer/chrome/linux-120.0/chrome",
        ".cache/node-gyp/18.19.0/include/node/node.h",
        ".cache/prisma/master/abc123/linux/query-engine",
        ".cache/electron/electron-v28.0.0-linux-x64.zip",
    ]);
    m.fresh(&[
        ".cache/Cypress/14.0.0/Cypress/cypress",
        ".cache/puppeteer/chrome/linux-131.0/chrome",
        ".cache/node-gyp/22.11.0/include/node/node.h",
        ".cache/prisma/master/def456/linux/query-engine",
        ".cache/electron/electron-v33.0.0-linux-x64.zip",
    ]);

    assert_eq!(m.stale("cypress"), [".cache/Cypress/13.1.0"]);
    assert_eq!(
        m.stale("puppeteer"),
        [".cache/puppeteer/chrome/linux-120.0"]
    );
    assert_eq!(m.stale("node-gyp"), [".cache/node-gyp/18.19.0"]);
    assert_eq!(m.stale("prisma"), [".cache/prisma/master/abc123"]);
    assert_eq!(
        m.stale("electron"),
        [".cache/electron/electron-v28.0.0-linux-x64.zip"]
    );
}

#[test]
fn hugging_face_models_expire_one_by_one() {
    let m = Machine::new();
    m.old(&[
        ".cache/huggingface/hub/models--old--bert/blobs/abc",
        ".cache/huggingface/hub/version.txt",
    ]);
    m.fresh(&[".cache/huggingface/hub/datasets--new--set/blobs/def"]);

    assert_eq!(
        m.stale("huggingface"),
        [".cache/huggingface/hub/models--old--bert"]
    );
}

#[test]
fn composer_and_deno_caches_go_only_as_a_whole() {
    let m = Machine::new();
    m.old(&[
        ".cache/composer/files/vendor/pkg/a.zip",
        ".cache/deno/remote/https/x/old.ts",
    ]);
    m.fresh(&[".cache/deno/npm/registry.npmjs.org/y/new.json"]);

    assert_eq!(m.stale("composer"), [".cache/composer/files"]);
    assert_eq!(m.stale("deno"), Vec::<String>::new());
}
