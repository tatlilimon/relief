# relief

Finds developer build output and caches that are no longer used, and deletes them, by hand
or on a schedule.

```
$ relief scan

PROJECTS  projects nobody touched
      SIZE  UNUSED  WHERE  ·  HOW TO GET IT BACK
    3.1 GB     47d  ~/code/raytracer/target  ·  cargo build
    412 MB    163d  ~/code/shop-frontend/node_modules  ·  npm ci

CACHES  entries nothing has used
      SIZE          WHAT
    576 MB          go-mod cache · 360 entries unused for 60+ days · ~/go/pkg/mod  ·  downloaded again when needed

Reclaimable  4.1 GB  in 3 items
Run `relief clean` to choose what to delete.
2 more are kept (in use, or not provably generated) · relief scan --all
```

`relief clean` opens a list to tick items off, with a running total:

```
Select what to delete
> [x]   3.1 GB  47d  ~/code/raytracer/target  ·  cargo build
  [ ]   412 MB  163d  ~/code/shop-frontend/node_modules  ·  npm ci
  [ ]   576 MB  go-mod cache · 360 entries unused for 60+ days · ~/go/pkg/mod  ·  downloaded again when needed
1 of 3 selected  ·  3.1 GB
↑↓ move · space select · a all · n none · p projects · c caches · d docker · enter delete · q cancel
```

Two rules hold everywhere:

- **Only what can be produced again is deleted.** Every line shows the command that brings
  the directory back, and `relief history --verbose` keeps that record.
- **"Unused" is measured, not guessed.** Size and names decide nothing; the filesystem does.

Linux only for now.

## Install

From a checkout (needs Rust 1.89 or newer):

```sh
./install.sh            # builds and copies the binary to ~/.local/bin
# or
cargo install --path .
```

On another machine, once a release has been published:

```sh
curl -fsSL https://raw.githubusercontent.com/tatlilimon/relief/main/install.sh | sh
```

That downloads a statically linked binary for x86_64 or aarch64 and verifies its checksum.
The binary has no dependencies, so copying `relief` to another Linux machine also works.

## Use

| Command | What it does |
|---|---|
| `relief` or `relief scan` | Shows what is unused. Deletes nothing. |
| `relief scan --all` | Also lists what is kept, and why. |
| `relief clean` | Opens the list to tick items, then asks once more before deleting. |
| `relief clean --plain` | Same choice by typing line numbers; no full-screen list. |
| `relief clean --yes` | Deletes everything listed. |
| `relief clean --dry-run` | Shows what would be deleted. |
| `relief clean ~/code/old` | Limits the run to projects below a directory. |
| `relief clean --only docker` | Limits the run to `projects`, `caches` or `docker`. |
| `relief schedule on --every weekly --at 12:00` | Runs the unattended clean regularly. |
| `relief schedule off` / `status` | Stops it, or shows when it runs next. |
| `relief history --verbose` | Lists what was deleted, with the way back. |
| `relief config init` / `show` | Writes a commented configuration file, or prints the rules in effect. |

## Reading the output

- Colour is used on a terminal only, never when piped. `NO_COLOR` turns it off, and so does
  `--color never`; `--color always` forces it.
- Nothing is said by colour alone: ticked rows show `[x]`, the cursor row starts with `>`,
  and results read `✓ removed` or `✗ failed`.
- Lines are fitted to the terminal width. A long path loses its middle, not its end.
- `relief clean --plain` avoids the full-screen list, for screen readers and simple
  terminals. `relief scan --json` is there for scripts.

## What it looks at

**Projects.** A directory is a candidate only next to the manifest that can rebuild it:

| Kind | Directories | Needs |
|---|---|---|
| node | `node_modules`, `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.parcel-cache`, `.angular` | `package.json`; `node_modules` also needs a lockfile |
| rust | `target` | `Cargo.toml` and cargo's `CACHEDIR.TAG` inside |
| python | `.venv`, `venv`, `env`, `.tox`, `.nox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache` | a dependency file; virtualenvs need `pyvenv.cfg` inside |
| gradle, maven | `build`, `.gradle`, `target` | the build file |
| php | `vendor` | `composer.json` and `composer.lock` |
| dotnet, elixir, dart, zig, swift, haskell | their build directories | their manifest |

Hidden directories, `node_modules`, virtualenvs and anything on another filesystem are never
searched for projects. Directories with files tracked by git are never deleted.

**Caches.** By age, entry by entry where entries are independent: Go build and module
caches, cargo's registry and git checkouts, npx installs, yarn, Playwright, Cypress and
Puppeteer browsers, Electron downloads, node-gyp headers, Prisma engines and Hugging Face
models (`ask` by default, as they are large). Only as a whole, once nothing in them was used:
npm's content store, pip, bun, Composer and Deno. Through the tool's own `prune` command:
uv and pnpm, which do not allow their cache to be edited by hand.

**Docker**, through the `docker` command and only when its daemon answers:

| Kind | What | Unattended by default |
|---|---|---|
| `build-cache` | build cache docker has not used for 30 days, via `docker builder prune` | auto |
| `dangling-images` | untagged images older than 7 days that no container uses | auto |
| `images` | pulled images that no container uses (stopped ones count) and were last pulled 60+ days ago | ask |

Images built on this machine are never offered, since a registry cannot give them back.
Nothing is forced: if docker refuses to remove an image, the refusal is reported. Volumes
can hold data and are never deleted; relief only says how much unused ones take.

## How "unused" is decided

A project counts as used when, within the configured number of days, any of these happened:

- one of its files changed;
- something read its build output or installed dependencies (running or building does);
- git recorded a commit, checkout, pull or fetch;
- a running process has its working directory inside it.

Reading source files does not count: searches, indexers and coding agents read projects
nobody works on. A monorepo is judged as one unit, so one busy package keeps the others'
dependencies.

A cache entry counts as used when it was read, changed or hard-linked into a project.

Limits worth knowing:

- On a filesystem mounted `noatime`, reads leave no trace and only changes count. `relief`
  says so when it notices.
- A program that has been running for longer than the threshold without touching its files
  again (a container with the project mounted, for example) looks unused.
- Docker does not record when an image was last used. relief counts from the last pull or
  tag, and keeps any image a container (running or stopped) refers to.
- Reported sizes are what `du` would say. Hard-linked files are counted only when deleting
  really frees them; snapshots and reflink copies can keep the space allocated.

## Unattended runs

`relief schedule on` installs a systemd user timer (or a crontab line where there is no
systemd) that runs `relief clean --auto`. That run deletes whatever the configuration marks
`auto`, leaves `ask` kinds for the next `relief clean`, and does not look at `off` kinds.

```toml
# ~/.config/relief/config.toml
roots = ["~"]
ignore = ["~/archive"]

[projects]
mode = "auto"
idle_days = 30

[projects.kind.rust]     # rebuilding is slow: wait longer and ask first
idle_days = 90
mode = "ask"

[caches]
mode = "auto"
unused_days = 60

[docker.kind.images]     # delete unused pulled images without asking
mode = "auto"
unused_days = 90
```

A file named `.reliefignore` in a directory keeps relief out of it.

Deleting is atomic per directory: it is renamed first and removed afterwards, so an
interrupted run never leaves a half-deleted `node_modules`. The next run finishes the job.

## Development

```sh
cargo test
```

The tests run the real binary against throwaway home directories; nothing outside them is
touched.

## License

MIT
