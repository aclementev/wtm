# wtm design specification

`wtm` (worktree manager) creates, lists and removes git worktrees for very
large monorepos, fast enough to give every agent task its own full checkout.
This document pins every mechanism and interface decision needed to
implement it. The evidence behind each decision was produced in a research phase whose
notes (`research/`, benchmarks, filesystem experiments, a survey of existing
tools) are kept out of this repository; this document is self-contained and
states the conclusions, not the measurements. References to `research/`
below point at those local notes.

Target: macOS on APFS first, Linux on btrfs/XFS/bcachefs second, both with
the same code paths. Windows is out of scope. Implementation language: Rust.

## 1. Vocabulary

- **repo**: a git repository. Its **main worktree** is the directory holding
  the real `.git` directory. `wtm` never modifies the main worktree's files.
- **worktree**: a linked git worktree created by `wtm`, one per task. It has
  a full working tree: every tracked file is present and readable. No sparse
  checkout, ever.
- **source**: the existing worktree whose files are cloned to make a new
  one. Default: the main worktree. Override: `--from`.
- **base**: the commit the new branch starts from. Default: the remote
  default branch (`origin/HEAD`, resolved). Override: `--base`.
- **clone**: a copy-on-write copy at the filesystem level: `clonefile(2)` on
  APFS, `ioctl(FICLONE)` (a "reflink") on Linux. Both share data blocks
  until written. Never confuse with `git clone`.
- **data dir**: where worktrees live. **state dir**: where `wtm` keeps its
  own records. **trash**: a directory inside the data dir where removed
  worktrees wait to be unlinked.
- **init hook**: a per-project script run inside a new worktree after
  creation.

## 2. Layout on disk

```
$XDG_DATA_HOME/wtm/                     default ~/.local/share/wtm
  worktrees/                            the data root
    <repo-id>/
      <name>/                           one worktree; <name> may contain "/"
      .trash/                           removed worktrees awaiting unlink
        <name>-<uuid>/
$XDG_CONFIG_HOME/wtm/config.toml        global config
<repo>/.wtm/config.toml                 project config (tracked or ignored, project's choice)
<repo>/wtm-init.sh                      default init hook
<repo>/.worktreeinclude                 gitignore-syntax list of untracked files to carry
```

That is the whole footprint. `wtm` writes no state files, keeps no
registry, has no state directory, no lock file and no daemon.

### 2.1 Repo identity

`<repo-id>` is `<basename of the main worktree>-<first 8 hex chars of
sha256(canonical absolute path of the main worktree)>`, for example
`monorepo-3f9a1c2e`. The basename keeps the directory human-readable; the
hash separates two repos with the same name (a fork beside the original, a
second clone). The path is canonicalized (symlinks resolved) before hashing
so `~/code/repo` and `/Users/me/code/repo` agree.

The function computing this must carry the above as its doc comment,
including the consequence: the id is one-way, so the repo a `<repo-id>`
directory belongs to is recovered by reading the `.git` file of any
worktree inside it, never by reversing the hash. Moving or renaming a repo
therefore yields a new id, its old worktrees keep pointing at the old path,
and `wtm ls --all` reports that directory as orphaned. Recovering a moved
repo is `git worktree repair`'s job, not ours.

### 2.2 Everything is derived

`wtm` stores nothing because git and the filesystem already answer every
question it asks. This table is the contract; no command may depend on a
fact that is not in it.

| question | answered by |
|---|---|
| which worktrees exist, with branch and head | `git worktree list --porcelain` |
| which of them are ours | their path is under the data root |
| which repo a `<repo-id>` directory belongs to | the `gitdir:` line of the `.git` file in any worktree inside it, or `git rev-parse --git-common-dir` run there |
| when a worktree was created | birth time (`st_birthtime`, `statx` `STATX_BTIME`) of git's metadata directory for it, which git creates at `worktree add` and nothing routinely touches; fall back to mtime where birth time is unavailable |
| what a worktree branched from | `git merge-base <head> <default branch>` |
| which trash entries exist | `readdir` of `<root>/<repo-id>/.trash` |
| whether cloning would work here, and why not | probed live by `wtm doctor`, never remembered |
| whether the init hook succeeded | reported in band by `wtm new`: exit 3 and a message. Not persisted; nothing reads it later |

Two consequences, accepted deliberately:

- A failed init hook leaves no durable trace. A caller that ignores exit
  code 3 gets no later reminder. The alternative was a metadata file whose
  only consumer was a column in `wtm ls`.
- A plain `git worktree add` into the data root is indistinguishable from
  one of ours, and `wtm` will manage it. That is harmless and arguably
  correct.

Adding state later is a one-file change; removing it is a migration, so the
design starts at zero.

### 2.3 Git's directory name is not our worktree name

Git derives the name of `<common dir>/worktrees/<x>` from the basename of
the worktree path and appends a digit on collision, so a worktree named
`feat` can have its metadata in `worktrees/feat1`, and a name containing a
slash uses only the last component. The gitdir path must always be read
from `git worktree list --porcelain` or from the worktree's `.git` file. It
must never be constructed by joining the worktree name.

## 3. Configuration

TOML, same keys at every level. Precedence, highest first:

1. command-line flag
2. environment variable `WTM_<KEY>` (upper case, e.g. `WTM_DIR`, `WTM_INIT`)
3. project config `<repo>/.wtm/config.toml`, read from the **main worktree**
4. global config `$XDG_CONFIG_HOME/wtm/config.toml`
5. built-in default

```toml
dir = "~/.local/share/wtm/worktrees"           # data dir root; "~" expanded
init = "wtm-init.sh"                           # path relative to repo root, or absolute
base = "origin/HEAD"                           # any ref; "origin/HEAD" means resolve the remote default
branch_prefix = ""                             # "alvaro/" turns `wtm new foo` into branch alvaro/foo
fetch = false                                  # `git fetch` the base's remote before creating
include = ".worktreeinclude"                   # path of the include file, relative to repo root

[clone]
mode = "auto"        # "auto": CoW when possible, else checkout. "cow": fail if impossible. "checkout": never clone.
workers = 0          # checkout.workers for the fallback; 0 = number of cores

[rm]
wait = false         # unlink synchronously instead of renaming to trash
```

Unknown keys are an error naming the file and key. `wtm config` prints the
effective merged config with the origin of each value.

## 4. Commands

Global behaviour for every command:

- stdout carries only machine-readable output (a path, a list, JSON).
  Progress, warnings and errors go to stderr. `--quiet` silences progress.
- `--json` on `ls`, `doctor` and `config` emits JSON instead of text.
- Every git subprocess is spawned with `GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_INDEX_FILE`, `GIT_COMMON_DIR` and `GIT_OBJECT_DIRECTORY` removed from
  the environment and explicit `-C <path>` arguments. Inheriting them from
  the caller has caused false conflicts in other tools.
- The repo is found from the current directory via `git rev-parse
  --git-common-dir` and `--show-toplevel`; running inside an existing `wtm`
  worktree resolves to the same repo. `--repo <path>` overrides.
- Before doing its own work, every command except `shell` and `agent`
  checks the current repo's `.trash` and, if it is non-empty, spawns a
  detached reaper for it rather than blocking. Other repos' trash is swept
  when a command runs against them, or by `wtm gc`.

Exit codes: 0 success; 1 failure; 2 usage error; 3 worktree created but the
init hook failed; 4 refused because the worktree is dirty.

### 4.1 `wtm new <name> [flags]`

Creates a worktree and prints its absolute path on stdout, nothing else.

Flags: `--base <ref>`, `--from <worktree-name-or-path>`, `--dir <path>`,
`--init <path>`, `--no-init`, `--fetch`, `--clone-mode auto|cow|checkout`,
`--branch <name>` (branch name if different from `<name>`), `--force`
(reuse an existing branch even if it is checked out elsewhere is never
allowed; `--force` only permits a `<name>` whose directory exists in trash).

Name rules: `<name>` matches `^[A-Za-z0-9._][A-Za-z0-9._/-]*$`, no `..`
component, no leading `-`, at most 200 bytes. It doubles as the branch name
after `branch_prefix` is applied; the directory is `<data dir>/<repo-id>/<name>`.

Branch semantics: if `<prefix><name>` does not exist, create it at the base.
If it exists and is not checked out anywhere, check it out and ignore
`--base` with a warning. If it is checked out in another worktree, fail with
that worktree's path (git refuses this and so do we).

Algorithm: section 6. On hook failure: worktree stays, path is still printed,
exit 3.

### 4.2 `wtm ls [--all] [--json]`

Text output, one line per worktree: name, branch, short base commit, age
and path, each derived per section 2.2. There is no init-status column;
nothing records it. `--all` walks the data root, resolves each `<repo-id>` directory to its
repo through a worktree's `.git` file, and groups by repo, marking
worktrees whose directory no longer exists as `missing` and `<repo-id>`
directories whose repo is gone (or that hold no worktrees) as `orphaned`.
A root other than the default is only visited when `--dir` or the config
points at it. The source of truth is `git worktree list --porcelain` filtered to paths
under the repo's data dir; age and base are derived per section 2.2.

### 4.3 `wtm rm <name> [--force] [--wait] [-d|--delete-branch] [-D|--force-delete-branch]`

Removes a worktree. Refuses (exit 4) if `git status --porcelain` in it is
non-empty, unless `--force`. Also refuses if the worktree is the caller's
current directory or an ancestor of it, with a hint to `cd` out first.
`-d`/`--delete-branch` deletes the branch after removal with `git branch
-d` semantics: it refuses an unmerged branch and says so, the worktree is
still removed. `-D`/`--force-delete-branch` uses `git branch -D`. The default
keeps the branch, since deleting one is not reversible the way the trash
rename is. Algorithm: section 8. Prints nothing on stdout.

### 4.4 `wtm cd <name>`

Prints the worktree path on stdout. With the shell wrapper installed, the
wrapper changes directory to it. With no argument, prints the main worktree.

### 4.5 `wtm init [<name>] [--init <path>]`

Reruns the init hook in the named worktree (default: the current one).
Exit 3 on failure. It has all the provenance the hook needs without any
stored state: the worktree path, its branch, and the repo.

### 4.6 `wtm gc [--wait] [--dir <path>] [--orphans]`

Sweeps every `.trash` under the data root (section 8.4), runs
`git worktree prune` for every repo reachable from it, reports orphaned
`<repo-id>` directories and deletes them with `--orphans`, and removes
empty `<repo-id>` directories. `--dir <path>` sweeps a non-default root. `--wait` runs the sweep
in the foreground instead of spawning a reaper.

### 4.7 `wtm doctor [--json]`

Reports, for the current repo: git version; main worktree path and volume;
data dir path and volume; whether they share a filesystem (`st_dev`);
whether a probe clone succeeds between them; index version and whether
split index is in use; sparse checkout, submodules, LFS presence; fsmonitor
and untracked-cache settings; which creation method `wtm new` would use and
why. Exit 1 if `clone.mode = "cow"` would fail.

### 4.8 `wtm config [--json]`

Prints the effective configuration and where each value came from.

### 4.9 `wtm shell <zsh|bash|fish>`

Prints a shell function to be evaluated: `eval "$(wtm shell zsh)"`. The
function wraps the binary; for `new` and `cd` it captures stdout, forwards
stderr, and on success `cd`s to the printed path; for every other subcommand
it execs the binary unchanged. It must preserve the binary's exit code.

### 4.10 `wtm agent skill`

Prints a markdown skill file (section 10). `wtm agent` with no subcommand
prints usage.

### 4.11 `wtm --help`, `wtm <cmd> --help`, `wtm --version`

Standard. The help text and the agent skill are generated from the same
command definitions so they cannot drift.

## 5. Preconditions checked by `wtm new`

In order, each failing with a specific message:

1. `git` is at least 2.31 (parallel checkout) and the directory is a repo.
2. The source worktree is not mid-operation: none of `rebase-merge`,
   `rebase-apply`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
   `BISECT_LOG` exist in its gitdir. Otherwise fail; the caller must finish
   or abort that operation first.
3. The source worktree is not sparse (`core.sparseCheckout` false and no
   `info/sparse-checkout`). If it is, fall back to checkout with a warning;
   a clone would inherit the sparse patterns.
4. The destination does not exist. If a directory of that name is in the
   trash, `--force` renames it out of the way with a new uuid; otherwise
   fail with "removed worktree pending deletion, use --force".
5. The branch rules of 4.1.
6. Method selection (section 6.2) when `clone.mode` is `auto` or `cow`.

## 6. Creating a worktree

### 6.1 Steps

```
resolve config, repo, source, base
fetch if requested
select method                              (6.2)
git worktree add --no-checkout --detach <dest> <source HEAD commit>
read the real gitdir back from git (section 2.3)
if method == cow:
    exclude set                            (6.3)
    clone walk source -> dest              (6.4)
    install index                          (6.5)
    empty submodule directories            (6.6)
    git -C dest reset -q --hard            (drops cloned uncommitted changes)
else:
    git -C dest -c checkout.workers=<n> -c checkout.thresholdForParallelism=100 checkout -q --detach <source HEAD>
git -C dest checkout -q -b <branch> <base>      (or `checkout <branch>` if it exists)
run init hook                              (7)
print dest path
```

`git worktree add` must run before any file is written: git refuses a
non-empty destination even with `--no-checkout`. Detaching at the source's
HEAD commit and switching to the base afterwards means git rewrites only the
files that differ between source and base.

If any step before the init hook fails, the destination directory and the
git metadata are removed (`git worktree remove --force` after
`git worktree unlock` if needed) and the branch is deleted if `wtm` created
it. The failure message names the step.

### 6.2 Method selection

`auto` (default) chooses CoW when all of these hold, else checkout with a
warning explaining which one failed and the override that would fix it:

- source and destination parent have the same `st_dev`;
- a probe succeeds: clone one small regular file from the source (the first
  regular file found in the source's top level, falling back to any tracked
  file) to `<dest parent>/.wtm-probe-<uuid>`, then unlink it. On macOS this
  is `clonefile` with `CLONE_NOFOLLOW`; on Linux `FICLONE` on a freshly
  created file. Errors mean unsupported: `EXDEV`, `ENOTSUP`/`EOPNOTSUPP`,
  `ENOTTY`, `EINVAL`.

`cow` fails instead of warning. `checkout` skips the checks.

### 6.3 Exclude set

The new worktree contains tracked files plus paths matched by
`.worktreeinclude`. Everything else that is untracked or ignored in the
source is left out. Never implement gitignore matching by hand: git is the
only authority on what is ignored, and a directory containing even one
tracked file is never collapsed by git, so a hand-rolled matcher risks
dropping tracked files.

Run in the source, all `-z`:

```
git ls-files -o -i --exclude-standard --directory      -> ignored, collapsed to top-most fully-ignored dirs
git ls-files -o    --exclude-standard --directory      -> untracked, collapsed
git ls-files -o -i --exclude-from=<include file>       -> include set, individual files
```

The include file is read from the source; if it is itself untracked, it is
added to the include set so a worktree cloned from this one keeps it.
The third command only sees untracked files, which is correct: tracked
files are always carried.

Build two path tries: `excluded` = ignored ∪ untracked, `included` from
the third list. Then, for each included path, remove it and mark every
ancestor as "mixed". The walk below consults both. The three commands cost
about 0.6 s at 100k files and are not accelerated by the untracked cache;
an implementation may replace them with one `git status --porcelain=v2 -z
--ignored=matching --untracked-files=all` call, which is, but must produce
the same sets. Verify equivalence with a test.

### 6.4 Clone walk

Input: source root, destination root (exists and is empty apart from the
`.git` file git wrote), the tries. Never touch `.git` in the source: it is a
directory in the main worktree or a file in a linked one, and the
destination already has its own.

```
walk(rel):
    if rel is excluded:               return           (skip entirely)
    if rel has no excluded or mixed descendant:
        clone_tree(src/rel, dst/rel)                    (one call on APFS)
        return
    mkdir dst/rel with src/rel's mode; copy its mtime last
    for child in readdir(src/rel):
        if rel == "" and child == ".git": continue
        walk(rel/child)
```

`clone_tree` on macOS is one `clonefile(src, dst, CLONE_NOFOLLOW)`. The
destination must not exist and its parent must. Symlinks inside the tree
are cloned as symlinks; only the argument itself needs `CLONE_NOFOLLOW`.
On Linux `clone_tree` is a recursive walk: directories via `mkdir`, symlinks
via `symlink`, regular files via `open` + `FICLONE` + `fchmod` +
`futimens` (mtime must match the source for the index to validate), other
file types skipped with a warning. Copy xattrs on Linux only if cheap; git
does not need them. If `FICLONE` fails mid-walk with `EXDEV` or
`EOPNOTSUPP`, abort the creation with the error; the probe should have
caught it. Sockets and fifos in the source are skipped on both platforms.

Cost model to keep in mind: on APFS the number of `clonefile` calls is what
matters, and a cache directory scattered through many packages forces every
ancestor open. An optional optimization, off by default until measured:
when a directory's excluded descendants number fewer than N small entries,
clone it whole and unlink the exclusions afterwards.

### 6.5 Installing the index

Cloned files have new inodes and ctimes, so a copied index would make git
re-hash every file. `wtm` writes the destination index itself:

1. Read the source's index (`<source gitdir>/index`). Parse the header
   (`DIRC`, version 2, 3 or 4, entry count), every entry, and every
   extension. Index v4 uses prefix-compressed paths and must be supported;
   large monorepos use it. If the `link` extension (split index) is present,
   read the shared index it names and merge to a full index; if that is not
   implemented yet, fall back: copy nothing, run `git -C dest read-tree
   HEAD` and set `core.checkStat=minimal` in the worktree config with a
   warning that status will be stat-fuzzy.
2. Object hash size is 20 bytes for SHA-1 repos and 32 for SHA-256
   (`extensions.objectFormat`). The trailing checksum uses the same hash.
3. For each entry, `lstat` the cloned file and rewrite `ctime`, `mtime`
   (seconds and nanoseconds, truncated to u32 like git), `dev`, `ino`
   (low 32 bits), `uid`, `gid`, `size`. Keep `mode`, the object id and the
   flags from the source: `core.fileMode` may be false and mode must not be
   taken from the filesystem. Entries with the skip-worktree or
   intent-to-add flag, and gitlinks (mode `160000`), keep their stat data
   untouched. An entry whose file is missing in the destination (it was
   excluded because untracked-but-listed, which cannot happen for tracked
   files, or a walk bug) is written with zeroed stat data so git re-checks
   it, and a warning is logged.
4. Racy-git rule: any entry whose mtime in whole seconds is greater than or
   equal to the time the index will be written gets `size` set to 0
   ("smudged"), which forces git to verify its content. Cloned files keep
   old mtimes, so in practice none are smudged.
5. Extensions: keep `TREE` (cache tree; valid because the tree contents are
   identical) and `REUC`. Drop `UNTR` (contains stat data and an absolute
   path of the source), `FSMN` (a token for the source's fsmonitor
   session), `EOIE` and `IEOT` (offset tables that would need recomputing),
   `link` and `sdir` (handled in step 1).
6. Write to `<gitdir>/worktrees/<name>/index.wtm-tmp`, fsync, rename over
   `index`. Set the file's mtime to now.

Validation in tests: after installation, `git status --porcelain` must be
empty and must not read file contents (measure time; a 100k-file repo must
report clean in well under a second), and planted edits (append, same-size
overwrite, delete, chmod when `core.fileMode` is true) must all show up.

### 6.6 Submodules

`git worktree add` leaves submodule directories empty, and so does `wtm`:
after the walk, for every gitlink entry in the index, remove whatever the
clone put there and leave an empty directory. Cloning submodule contents
would carry `.git` files pointing at the source's gitdir and break every
git command in the new worktree. Initializing submodules is the init hook's
job (`git submodule update --init`).

### 6.7 Checkout fallback

`git checkout --detach <source HEAD>` with `checkout.workers` set to
`clone.workers` (0 means core count) and `checkout.thresholdForParallelism`
100. Then the same branch step and hook as the CoW path.

## 7. Init hook

Resolution: `--init`, else `WTM_INIT`, else project config, else global
config, else `wtm-init.sh`. Relative paths are relative to the **source
worktree's** root, so a gitignored hook in the main checkout still runs and
so the new worktree needs no copy of it. Absolute paths are used as is. If
the resolved file does not exist and it was the default, skip silently
; if it was configured explicitly, fail with
exit 3 after creating the worktree.

Execution: the file must be executable; it is run directly (respecting its
shebang), not through `sh`. Working directory: the new worktree. Stdout and
stderr are passed through to `wtm`'s stderr, prefixed with `init: ` only
when stderr is not a terminal. Environment: the caller's, plus

```
WTM_ROOT      absolute path of the new worktree
WTM_NAME      worktree name
WTM_BRANCH    branch checked out
WTM_BASE      base ref as given
WTM_BASE_SHA  resolved base commit
WTM_SOURCE    absolute path of the source worktree
WTM_MAIN      absolute path of the main worktree
WTM_REPO_ID   repo id
WTM_METHOD    "cow" or "checkout"
```

Failure: non-zero exit keeps the worktree and prints "init hook failed (exit N); worktree
kept at <path>; rerun with: wtm init <name>", and `wtm new` exits 3 after printing the path. The
outcome is reported here and nowhere else; it is not written down. A hook that needs no shell can still be a small script;
there is no inline-command form in v1.

`--no-init` skips the hook.

## 8. Removing a worktree

### 8.1 Steps

```
resolve worktree by name (must be under the repo's data dir)
refuse if cwd is inside it
refuse if dirty and not --force            (git status --porcelain non-empty)
if --wait or rm.wait: delete synchronously, then prune, done
trash = <root>/<repo-id>/.trash            (mkdir -p)
rename(<worktree>, <trash>/<name-with-slashes-replaced-by-->-<uuid>)
    EXDEV or EBUSY -> delete synchronously instead, with a note
git -C <main> worktree prune
delete the branch only with -d (git branch -d) or -D (git branch -D); default keeps it
spawn reaper (8.3)
```

`rename(2)` is called directly, never `mv`, so a cross-filesystem move
fails instead of silently copying. `git worktree prune` is cheap: it only
checks whether each registered path exists, and it takes git's own
per-worktree metadata directory with it.

### 8.2 Synchronous delete

Used for `--wait` and for the rename fallbacks. On macOS, first
`chflags -R nouchg` equivalent (clear `UF_IMMUTABLE`) because cloned
locked files are locked too; on both platforms, `chmod u+rwx` on
directories that refuse. Remove with a recursive unlink that treats
`ENOENT` as success. Report paths it could not remove and exit 1 for those.

### 8.3 The reaper

`wtm` re-executes itself as `wtm gc --reap --detach` (hidden flags):

- child is created with `setsid()` in the pre-exec hook so terminal close
  and shell exit cannot signal it;
- stdin, stdout and stderr are redirected to `/dev/null` (or to
  `$XDG_CACHE_HOME/wtm/reaper.log` when `WTM_DEBUG` is set). Leaving the
  caller's stdout open makes `$(wtm rm x)` hang until the reaper exits;
- the parent does not wait; on parent exit the child is reparented to
  `launchd`/`init`;
- the child lowers itself: `setpriority(PRIO_PROCESS, 0, 19)`; on macOS
  `setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_THROTTLE)`;
  on Linux `ioprio_set` to the idle class;
- it then runs the sweep (8.4) and exits.

The reaper dies with the user session (logout, container exit, agent
sandboxes that kill descendants). Nothing depends on it finishing.

### 8.4 The sweep

For the root in hand (the current repo's during an opportunistic sweep,
the whole data root during `wtm gc`), for every `<repo-id>/.trash`
directory under it, for every entry in that directory:

```
fd = open(entry, O_RDONLY | O_DIRECTORY)
if flock(fd, LOCK_EX | LOCK_NB) fails with EWOULDBLOCK: skip (another sweeper owns it)
delete entry synchronously (8.2), ENOENT is success
rmdir entry; close(fd)
```

The lock is on the entry's own inode: no lock files, released by the kernel
if the sweeper dies, so a crashed sweep leaves a half-deleted tree that the
next sweep simply resumes. Concurrent sweeps partition the entries between
them. `fcntl` record locks are not used (released when any descriptor to
the file closes). Entries that are not directories are never touched, and a `.trash`
directory is only ever looked for directly under a `<repo-id>` directory
of a known root.

## 9. Shell integration

`wtm shell zsh` prints roughly:

```zsh
wtm() {
  case "$1" in
    new|cd)
      local out; out="$(command wtm "$@")" || { local rc=$?; [ -n "$out" ] && printf '%s\n' "$out"; return $rc; }
      [ -d "$out" ] && cd "$out" || printf '%s\n' "$out" ;;
    *) command wtm "$@" ;;
  esac
}
```

Exit code 3 from `new` (hook failed) still changes directory, because the
worktree exists; the wrapper prints the path and returns 3 only when the
directory is missing. Bash is the same; fish uses its own syntax. Completion
scripts are a later addition.

## 10. Agent skill

`wtm agent skill` prints a markdown document with YAML front matter
(`name: wtm`, `description:` one line) followed by: what a worktree is and
when to create one (one per task, never work in the main worktree); the
loop `wtm new <task> -> cd "$(...)" -> work -> commit -> wtm rm <task>`;
that stdout is only the path; that `wtm rm` refuses dirty trees and what
to do (commit, or `--force` to discard); the exit codes; the hook
environment variables; and `wtm ls --json` for orientation. It is generated
from the command definitions, with the prose kept in one Rust source file
next to them, so `--help` and the skill share one description per flag.

## 11. Concurrency and safety

- Two `wtm new` in one repo at the same time: git serializes
  `worktree add` with its own locks, and the loser of a race for the same
  name fails at the destination-exists check. `wtm` takes no locks of its
  own because it writes no shared files. Names are unique per repo, so the second call for the same name
  fails at the destination-exists check.
- `wtm rm` while a hook or an agent is still running inside the worktree:
  the dirty check catches most cases; a clean tree with a live process is
  renamed under it, the process keeps working in the trash until the sweep
  unlinks the files. Accepted; same as `git worktree remove`.
- Never delete anything outside a `.trash` directory under a known root or
  the destination of a failed creation. Never run destructive git commands in the source.
- Secrets: nothing untracked is carried unless `.worktreeinclude` names it.
  `wtm doctor` lists what the include file matches.
- `wtm-init.sh` from the repo is executed as the user; running a hook from a
  freshly checked-out branch is the same trust model as any repo script.
  No trust prompt in v1; noted as a future option.

## 12. Platform notes

macOS: `clonefile(const char*, const char*, int)` in `<sys/clonefile.h>`;
flags `CLONE_NOFOLLOW` 0x0001, `CLONE_NOOWNERCOPY` 0x0002, `CLONE_ACL`
0x0004. Cloning across APFS volumes fails with `EXDEV`. Cloned files keep
mtime, xattrs, flags and permissions; ACLs only with `CLONE_ACL` (not
needed). `/tmp` is a different volume from `/Users`; tests must not use it.

Linux: `ioctl(dst_fd, FICLONE, src_fd)` from `<linux/fs.h>`. Same
filesystem and mount required. Data only; the caller copies mode and
mtime. Supported on btrfs, XFS (reflink=1), bcachefs, ZFS 2.2+ when block
cloning is enabled; not ext4 or tmpfs. `copy_file_range` is the fallback
for a single file that refuses to clone mid-walk, but a whole-tree
fallback is the checkout path, not a userspace copy.

Both: `rename(2)` is atomic within a filesystem and O(1) in tree size.
`flock(2)` works on directory descriptors on both.

## 13. Testing

The strategy is in `TESTING.md`; this section keeps the acceptance criteria.

- Unit: index parser and writer round-trip on v2, v3 and v4 fixtures
  (generate with `git update-index --index-version N`), including
  extensions and SHA-256 repos; config precedence; name validation; trie
  logic for the exclude walk.
- Integration (macOS CI on APFS, Linux CI on a btrfs or XFS loop mount,
  plus an ext4 job that must take the checkout path): build a synthetic
  repo with `research/scripts/mkrepo.sh`-like generation at 5k files with
  planted ignored files, `.worktreeinclude`, a submodule, a symlink to a
  directory at the top level, and a dirty tracked file in the source. Assert:
  `git worktree list` shows the worktree; `git status --porcelain` is empty;
  the tree equals `git worktree add`'s tree plus the included files
  (compare sorted `find` output); excluded paths are absent; the dirty
  change did not carry; `git checkout <other-branch>` works; planted
  mutations after creation are detected; `wtm rm` returns in under 100 ms
  and the path is gone; the trash entry disappears after `wtm gc --wait`;
  two concurrent `wtm gc --wait` runs on a populated trash both exit 0.
- Reference oracles: `research/git/clone-worktree-excl.sh` and
  `research/git/verify.sh` implement the same procedure in bash and Python
  and can be run against the Rust binary's output for comparison.
- Performance smoke: creation of a 100k-file synthetic repo must not be
  slower than `git worktree add -c checkout.workers=8` by more than 30%,
  and first `git status` must take under one second.

## 14. Not in v1

Carrying uncommitted changes into the new worktree (`--dirty`), inline hook
commands in config, symlinked shared caches, closest-worktree source
selection, trust prompts for repo hooks, btrfs subvolume snapshots, Windows,
shell completions, `wtm agent install`, and anything about macOS Spotlight
indexing: a user who minds it can exclude the directory themselves, from
the init hook or from the Spotlight Privacy settings.

## 15. Crate layout

See `ARCHITECTURE.md` for modules, types and signatures. Summary:

```
src/main.rs            clap definitions; help and skill prose live here
src/config.rs          TOML loading and precedence
src/repo.rs            git discovery, subprocess wrapper with scrubbed env
src/exclude.rs         exclude/include sets and tries
src/clone/mod.rs       walk; clone/macos.rs (clonefile), clone/linux.rs (FICLONE)
src/index/             parser, writer, stat rewrite
src/create.rs          `wtm new` orchestration and rollback
src/remove.rs          rename, synchronous delete
src/reaper.rs          detach, priorities, sweep with flock
src/shell.rs           wrapper text
```

Dependencies worth using: `clap`, `serde` + `toml`, `libc` (or `rustix`) for
`clonefile`, `FICLONE`, `setsid`, `flock`, priorities; `sha1`/`sha2` for the
index checksum; `uuid`. Avoid a git library: every git operation here is a
subprocess call whose behaviour must match the user's git exactly.
