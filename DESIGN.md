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
  A **bare** repository has no working tree; git reports the repository
  directory in the main worktree's place, and `wtm` manages its linked
  worktrees normally. Nothing can be cloned from a repository with no files,
  so creation there always takes the checkout path (section 6.2).
- **worktree**: a linked git worktree created by `wtm`, one per task. It has
  a full working tree: every tracked file is present and readable. No sparse
  checkout, ever.
- **source**: the existing worktree whose files are cloned to make a new
  one. Always the main worktree in v1; `--from` is listed in section 14.
- **base**: the commit the new branch starts from. Default: the remote
  default branch (`origin/HEAD`, resolved). A repository with no remote
  falls back to `origin/main`, `origin/master`, then the main worktree's
  current branch. Override: `--base`.
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
        <name>-<pid>-<nanos>/
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

The id only decides where new worktrees go. It is never used to decide
which worktrees are ours (section 2.2), so moving or renaming a repo, which
yields a new id, loses nothing: new worktrees go under the new id and the
old ones stay where they are, still listed and managed.

Git keeps the link from the repo to each worktree, which survives a move,
and a link back from each worktree, which names the repo's old path and
breaks. Every git command inside the worktree then fails until `git
worktree repair` runs. `wtm` runs it on the worktree before `cd`, `init`
and `rm` act on one; it rewrites only a broken link, so a moved repo heals
without anyone asking.

A repo that is deleted, rather than moved, takes git's records of its
worktrees with it. Nothing can find those worktrees afterwards but a look
at the data root, and removing them is `rm -rf`.

### 2.2 Everything is derived

`wtm` stores nothing because git and the filesystem already answer every
question it asks. This table is the contract; no command may depend on a
fact that is not in it.

| question | answered by |
|---|---|
| which worktrees exist, with branch and head | `git worktree list --porcelain` |
| which of them are ours, and their names | their path is `<data root>/<any repo-id>/<name>`. `git worktree list` only reports this repo's worktrees, so the repo-id directory need not be the current one, and after a move it is not |
| when a worktree was created | birth time (`st_birthtime`, `statx` `STATX_BTIME`) of the worktree directory, which git creates at `worktree add`. Where the filesystem has no birth time this falls back to mtime, which moves on any top-level write, so the age is unreliable there; every filesystem wtm targets has birth times |
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
back, either with `git -C <worktree> rev-parse --path-format=absolute
--git-dir` or from the `gitdir:` line of the worktree's `.git` file.
`git worktree list --porcelain` does not report it. It must never be
constructed by joining the worktree name.

## 3. Configuration

TOML. Every setting is scoped to whoever owns the decision, and a setting is
absent from a layer on purpose rather than by omission.

| key | flag | env | project | global | default |
|---|---|---|---|---|---|
| `dir` | `--dir` | `WTM_DIR` | — | yes | `~/.local/share/wtm/worktrees` |
| `base` | `--base` | `WTM_BASE` | yes | — | `origin/HEAD` |
| `branch_prefix` | — | `WTM_BRANCH_PREFIX` | — | yes | `""` |
| `fetch` | `--fetch` | `WTM_FETCH` | — | yes | `false` |
| `init` | `--init` | `WTM_INIT` | yes | yes | `wtm-init.sh` |

Higher layers win, left to right. Project config is `<repo>/.wtm/config.toml`,
read from the **main worktree**; global config is
`$XDG_CONFIG_HOME/wtm/config.toml`.

`dir`, `branch_prefix` and `fetch` describe this machine and this person, so
a repository cannot set them: cloning a repo must never relocate your
worktrees, rename your branches, or add a network round-trip to every
creation. A project file that sets one is an error naming the file and the
key. `base` describes the repository, so a global default for it would be
meaningless. `init` is the one key both files may set. Which hook a project
needs is the project's business, and where someone keeps their own is
theirs. Like `dir`, a leading `~` in it is expanded.

```toml
# ~/.config/wtm/config.toml
dir = "~/.local/share/wtm/worktrees"   # "~" expanded
branch_prefix = "alvaro/"              # `wtm new foo` creates branch alvaro/foo
fetch = false                          # git fetch before creating

# <repo>/.wtm/config.toml
base = "origin/HEAD"                   # any ref; "origin/HEAD" resolves the remote default
```

Everything else is a flag, because it is a per-invocation decision:
`--no-init` to skip the hook, `--clone-mode` for the creation method,
`--wait` for synchronous removal. The two per-repository behaviours
that would otherwise want settings already have their own files at the repo
root: `wtm-init.sh` and `.worktreeinclude`.

Parallel checkout always uses the core count; git's own `checkout.workers`
default is one worker, and anyone who wants a different number sets it in
their git config, where it already exists.

Unknown keys are an error naming the file and key. `wtm config` prints the
effective merged config with the origin of each value.

`WTM_<KEY>` names are reserved for configuration, which is input to `wtm`.
Two exceptions carry no configuration and name themselves: `WTM_DEBUG` sends
the reaper's output to `$XDG_CACHE_HOME/wtm/reaper.log` instead of
`/dev/null`, and `WTM_NO_REAPER` stops wtm spawning background reapers at
all. The tests need the second one: `wtm rm` spawns a reaper for the entry
it has just made, so what is in the trash cannot otherwise be asserted.
Anything `wtm` exports to a child process is named `WTM_HOOK_<...>` instead,
so that setting one can never be mistaken for the other (section 7).

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
init hook failed; 4 refused, and `--force` would override it (the worktree
is dirty, or locked).

### 4.1 `wtm new <name> [flags]`

Creates a worktree and prints its absolute path on stdout, nothing else.

Flags: `--base <ref>`, `--dir <path>`,
`--init <path>`, `--no-init`, `--fetch`, `--clone-mode auto|cow|checkout`,
`--branch <name>` (branch name if different from `<name>`). Reusing a branch
that is checked out elsewhere is never allowed, and there is no flag for it.

Name rules: `<name>` matches `^[A-Za-z0-9._][A-Za-z0-9._/-]*$`, no `.` or
`..` component, no empty component, no leading `-`, at most 200 bytes. It doubles as the branch name
after `branch_prefix` is applied; the directory is `<data dir>/<repo-id>/<name>`.

Branch semantics: if `<prefix><name>` does not exist, create it at the base.
If it exists and is not checked out anywhere, check it out and ignore
`--base` with a warning. If it is checked out in another worktree, fail with
that worktree's path (git refuses this and so do we).

Algorithm: section 6. On hook failure: worktree stays, path is still printed,
exit 3.

### 4.2 `wtm ls [--json]`

Text output, one line per worktree: name, branch, short base commit, age,
status and path, each derived per section 2.2. Status is empty for a healthy
worktree, `missing` when git reports it prunable (its directory is gone or
its gitdir pointer is broken) and `locked` when it is locked; an all-empty
column is not printed. There is no init-status column;
nothing records it. The source of truth is `git worktree list --porcelain`
filtered to the worktrees that are ours (section 2.2); age and base are
derived per section 2.2.

### 4.3 `wtm rm <name> [--force] [--wait] [-d|--delete-branch] [-D|--force-delete-branch]`

Removes a worktree. Refuses (exit 4) if `git status --porcelain` in it is
non-empty, or if git reports it `locked`, unless `--force`. Git needs
`--force` twice to remove a locked worktree; `wtm` supplies the second one,
having already been told to force. Also refuses if the worktree is the caller's
current directory or an ancestor of it, with a hint to `cd` out first.
`-d`/`--delete-branch` deletes the branch after removal with `git branch
-d` semantics: it refuses an unmerged branch and says so, the worktree is
still removed and the exit code is 1, since not everything asked for was
done. `-D`/`--force-delete-branch` uses `git branch -D`. The default
keeps the branch, since deleting one is not reversible the way the trash
rename is. Algorithm: section 8. Prints nothing on stdout.

### 4.4 `wtm cd <name>`

Prints the worktree path on stdout. With the shell wrapper installed, the
wrapper changes directory to it. With no argument, prints the main worktree.

### 4.5 `wtm init [<name>] [--init <path>]`

Reruns the init hook in the named worktree. With no name it uses the one the
caller is standing in, and refuses with exit 2 outside one, since the main
worktree is not ours. Exit 3 on failure.

It has all the provenance the hook needs without any stored state: the
worktree path, its branch, and the repo. `WTM_HOOK_BASE_SHA` comes from the
derivation `ls` uses (section 2.2). `WTM_HOOK_METHOD` is empty, since nothing
records how the tree was first populated. When no hook resolves at all it
says so and exits 0, rather than looking like it did something.

### 4.6 `wtm gc [--wait]`

Sweeps every `.trash` under the data root (section 8.4), runs
`git worktree prune` for the current repo, and removes empty `<repo-id>`
directories. The global `--dir` sweeps a non-default root. `--wait` runs
the sweep in the foreground instead of spawning a reaper, and reports how
many entries it deleted and how many it left to another sweep.

### 4.7 `wtm doctor [--json]`

Reports, for the current repo: git version; main worktree path and volume;
data dir path and volume; whether they share a filesystem (`st_dev`);
whether a probe clone succeeds between them; whether the source is a sparse
checkout; its index version and whether split index is in use; how many
submodules it has; the `.worktreeinclude` in effect and how many paths it
matches; and which creation method `wtm new` would use and why.

Every line either changes what `wtm new` does or is needed to explain why
it did. A machine where cloning is unavailable is a supported machine, not
a broken one, so `doctor` exits 0 there as everywhere; it exits non-zero
only when it could not look.

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

1. `git` is at least 2.36 and the directory is a repo. 2.31 brought parallel
   checkout, which the fallback path uses; 2.36 brought `worktree list
   --porcelain -z`, without which a worktree path containing a newline is
   misparsed.
2. The init hook resolves to a file that can be run, unless `--no-init`
   (section 7). Checked before anything is made, so a mistyped path costs
   nothing. A hook that runs and fails is a different outcome with its own
   exit code.
3. The branch name is one git accepts (`git check-ref-format`). A worktree
   name can be valid and still make a branch git refuses, such as `x.lock`,
   and git would otherwise say so only after the whole tree was cloned.
4. The source worktree is not mid-operation: none of `rebase-merge`,
   `rebase-apply`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
   `BISECT_LOG` exist in its gitdir. Otherwise fail; the caller must finish
   or abort that operation first.
5. The source worktree is not sparse (`core.sparseCheckout` false and no
   `info/sparse-checkout`). If it is, fall back to checkout with a warning;
   a clone would inherit the sparse patterns.
6. The name is free: no worktree of ours has it, whichever `<repo-id>`
   directory it is in, and nothing exists at the destination. A removed
   worktree never holds one: it is renamed under the trash with a suffix
   nothing asks for, so the name is free the moment `wtm rm` returns.
7. The branch rules of 4.1.
8. Method selection (section 6.2) when `--clone-mode` is `auto` or `cow`.

## 6. Creating a worktree

### 6.1 Steps

```
resolve config, repo, source, base
fetch if requested
select method                              (6.2)
git worktree add --no-checkout --detach <dest> <source HEAD commit>
read the real gitdir back from git (section 2.3)
if method == cow:
    since = now
    exclude set                            (6.3)
    clone walk source -> dest              (6.4)
    git -C dest read-tree HEAD
    empty submodule directories            (6.6)
    fill the index's stat data             (6.5)
    if the fill did not happen:
        git -C dest update-index --refresh -q  (exit status ignored)
    git -C dest reset -q --hard            (rewrites only what still differs)
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

Stat data in the index is what keeps the clone. Checkout decides whether to
write a file from the index's cached stat data and never by hashing content
first, so a `reset --hard` over the zeroed index `read-tree` writes would
rewrite every file from the object store and waste the clone. Section 6.5
fills the stat data in directly. When it cannot, `update-index --refresh`
spends the hashing once, finds the content already correct, and writes the
true stat back; it exits non-zero when a file needs updating, which is the
ordinary case here and not a failure. After a fill the refresh would find
nothing to do: every entry left zeroed is one the reset should write from
the object store anyway. Either way the reset then touches only files that
genuinely differ.

If any step before the init hook fails, the destination directory and the
git metadata are removed (`git worktree remove --force`, then `git worktree
prune`), and so are the directories creation made above it that are now
empty. No branch needs deleting: creating it is the last step that can
fail, and a `checkout -b` that fails creates none.

### 6.2 Method selection

`auto` (default) chooses CoW when all of these hold, else checkout:

- the source has a working tree: a bare repository has no files to clone,
  and this is a property of the repository rather than a failure, so `cow`
  reports it as unsupported rather than as an error to work around;
- source and destination parent have the same `st_dev`;
- a probe succeeds: write a few bytes to `<dest parent>/.wtm-probe-<pid>-<nanos>`,
  clone it to a second name beside it, unlink both. The `st_dev` check above
  has already established that source and destination share a filesystem, so
  a clone within the destination's directory answers the same question
  without searching the source for a file to copy or touching anything the
  user owns. On macOS the clone is `clonefile` with `CLONE_NOFOLLOW`; on
  Linux `FICLONE` on a freshly created file. Errors mean unsupported:
  `EXDEV`, `ENOTSUP`/`EOPNOTSUPP`, `ENOTTY`, `EINVAL`.

Falling back is ordinary operation, not a problem: on a filesystem without
cloning there is nothing the caller could do differently, and a warning on
every `wtm new` for the life of the machine would be noise. `auto` reports
its choice and the reason at progress level, which `--quiet` silences, and
`wtm doctor` is where the reason is explained at length. A warning is
reserved for a condition that is surprising and fixable: a sparse source
worktree (section 5), or a probe that fails where it should have worked.

`cow` fails instead of falling back. `checkout` skips the checks.

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
git diff-index --name-only HEAD                        -> tracked files dirty in the source
```

The include file is read from the source, which is always the main
worktree, so a copy sitting in a linked worktree has no effect; `wtm
doctor` names the one in effect. It is not carried into the new worktree
unless it names itself, because carrying an untracked file that nothing is
ignoring leaves a `??` entry in `git status`, and `wtm rm` refuses a
worktree whose status is not empty (section 4.3). Tracking it, which is
the ordinary case, or ignoring it both avoid that. The third command only
sees untracked files, which is correct: tracked files are always carried.

Build one path trie: `excluded` = ignored ∪ untracked ∪ dirty, `included`
from the third list. Then, for each included path, remove it and mark every
ancestor as "mixed". The walk below consults both. The four commands cost
about 0.6 s at 100k files.

The fourth query is about correctness, not speed. A tracked file modified
in the source carries the source's uncommitted content, while the index
entry that describes it still names HEAD's object. Cloning it and then
writing stat data taken from the clone would produce an entry git believes
is clean and whose content is not what it says (section 6.5). Leaving those
files out of the walk means the destination has no file there, the entry
gets zeroed stat data, and the `reset --hard` of 6.1 writes the committed
content from the object store. It also saves cloning bytes that would be
overwritten.

`--directory` is what collapses `node_modules` into a single trie node, and
that collapsing is what makes the walk cheap. A single `git status
--porcelain=v2 --ignored` call returns the same facts in one pass, but
getting the collapsed forms back out of it is the hard part, so `wtm` has
one implementation and it is this one.

### 6.4 Clone walk

Input: source root, destination root (exists and is empty apart from the
`.git` file git wrote), the tries. Never touch `.git` in the source: it is a
directory in the main worktree or a file in a linked one, and the
destination already has its own.

```
walk(rel):
    if rel == "":                     recurse           (dst/"" already exists)
    if rel is excluded:               return            (skip entirely)
    if nothing below rel is excluded or kept-inside-an-exclusion:
        clone_tree(src/rel, dst/rel)                    (one call on APFS)
        return
    mkdir dst/rel with src/rel's mode
    for child in readdir(src/rel):
        if rel == "" and child == ".git": continue
        walk(rel/child)
```

The root is always recursed whatever its classification, because git has
already created the destination directory and put a `.git` file in it:
`clone_tree` needs a destination that does not exist, and `.git` has to be
skipped. Directory mtimes are not copied. The only thing that would read
them is the untracked cache, and the index `read-tree` writes has none.

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
ancestor open.

### 6.5 Filling the index

Cloned files have new inodes and ctimes, so no existing index describes
them, and the index `read-tree HEAD` writes has every entry's stat data
zeroed. Either way git would re-hash every file. `wtm` fills in the stat
data of the index `read-tree` just wrote, in place.

Stat data lives in the fixed 40-byte prefix of each entry, so filling it
changes no entry's length and nothing else in the file moves. That is why
this is a patch rather than a rewrite: there is no writer, no
re-encoding of v4 paths, and the extensions (`TREE`, and `IEOT`/`EOIE`
when `index.threads` is set) stay valid as they are. `read-tree` never
writes a split index, an untracked cache or an fsmonitor token, whatever
the repository's configuration says, so none of those need handling.

1. Parse. The header is `DIRC`, version 2, 3 or 4. Entries are ten
   big-endian `u32` stat fields, the object id (20 bytes for SHA-1, 32 for
   SHA-256, taken from the length of the source's HEAD), a `u16` of flags,
   a second `u16` of extended flags when the flags say so (v3 and up), then
   the path. In v2 and v3 the path is NUL-terminated and the entry is
   padded with NULs to a multiple of eight bytes. In v4 the path is git's
   offset varint, the number of bytes to strip from the end of the previous
   path, followed by a NUL-terminated suffix, with no padding. Extensions
   follow, then a trailing checksum.
2. Refuse anything not understood completely, and write nothing: a
   checksum that does not match (an all-zero one is `index.skipHash` and
   is accepted); an unknown version; the extended flag in a v2 index; a
   path length in the flags that disagrees with the decoded path; a
   required extension (one whose signature does not start with an
   uppercase letter, such as `link`); or extensions that do not end
   exactly where the trailer starts. That last check catches any mistake
   in measuring an entry, because every later offset shifts with it.
3. For each entry, `lstat` the clone and the source file. Fill the entry
   from the clone only when the clone's file type matches the entry's
   mode and both the source's ctime and the clone's mtime fall at least
   one whole second before `since`'s second. Write ctime, mtime (seconds and
   nanoseconds), dev, ino, uid, gid and size, each truncated to 32 bits as
   git does. Keep the mode: `core.fileMode` may be false, and then the
   filesystem's executable bit is not the one git records. The `lstat`
   calls run on one thread per core, because the first `lstat` of a
   freshly cloned tree is where APFS finishes the clone, and threads halve
   it (0.63 s against 1.3 s for 100k files).
4. Everything else stays zeroed, and the reset of 6.1 writes it from the
   object store. That covers a missing file (dirty in the source, so the
   walk left it out), a gitlink (the clone has a directory there, so the
   type check fails), and a file changed during creation. Intent-to-add,
   skip-worktree and unmerged entries need no rule: git never trusts stat
   data for them, and `read-tree HEAD` writes none.
5. Recompute the checksum, write to `index.lock` and rename it over
   `index`.

`since` is taken before the dirty query of 6.3. The query says which files
matched HEAD when it ran; a source ctime older than `since` says the file
has not changed since. ctime is the one timestamp nothing can set back, so
an edit that restores the old mtime (`touch -r`, `rsync -t`, `cp -p`) still
moves it. Without this check such an edit, landing between the query and
the clone, is cloned with its new content and git reports it clean. The
comparison is in whole seconds because a filesystem with coarse timestamps
rounds a later change down, and it keeps a second's margin because the
kernel stamps ctime from a clock that runs up to a tick behind the one
`since` is read from: on Linux an edit 2 ms after `since` was measured with
a ctime in the second before it. Files changed in the last second or two
before `wtm new` are therefore left for git to hash, which costs nothing
worth counting. Requiring the clone's mtime to be older as well
keeps every filled entry out of git's racy window, since the index is
written after `since`, so no entry needs smudging.

A refused index costs speed, not correctness: `wtm` warns and runs the
refresh of 6.1. `WTM_NO_FAST_INDEX`, set to any value, skips the fill the
same way, as a way out should this code ever be suspected.

### 6.6 Submodules

`git worktree add` leaves submodule directories empty, and so does `wtm`:
after the walk, for every gitlink entry in the index, remove whatever the
clone put there and leave an empty directory. Cloning submodule contents
would carry `.git` files pointing at the source's gitdir and break every
git command in the new worktree. Initializing submodules is the init hook's
job (`git submodule update --init`).

### 6.7 Checkout fallback

`git checkout --detach <source HEAD>` with `checkout.workers` set to the
core count (section 3) and `checkout.thresholdForParallelism` 100. Then the same branch step and hook as the CoW path.

## 7. Init hook

Resolution: `--init`, else `WTM_INIT`, else project config, else global
config, else `wtm-init.sh`. A relative path typed this invocation, from the
flag or the variable, resolves against the caller's working directory, the
way any other path argument does. A relative path stored in a file, and the
`wtm-init.sh` default, resolve against the **source worktree's** root, so a
gitignored hook in the main checkout still runs and the new worktree needs no
copy of it. Absolute paths are used unchanged.

The resolved file must exist and be executable. Section 5 checks this as a
precondition rather than after the fact, so a path that could never have run
costs nothing. The `wtm-init.sh` default is the exception. Its absence is the
normal case and wtm skips it without a word. Any other layer naming a file
that is missing, is not a file, or is not executable fails with exit 2,
naming the path, the reason and the layer that set it.

Execution: the file runs directly, respecting its shebang, not through `sh`.
Working directory: the new worktree. Its stdout and stderr both go to `wtm`'s
stderr, unprefixed and inherited rather than piped, so a hook keeps its
terminal and its output arrives in real time and in its real order.
`--quiet` discards both. Stdin is inherited only when `wtm`'s own stdin is a
terminal, and is otherwise `/dev/null`, so a hook may prompt a person but can
never block an agent or a CI job on a read nobody will answer. Environment:
the caller's, minus the five `GIT_*` variables every git subprocess drops
(section 4), plus

```
WTM_HOOK_ROOT      absolute path of the new worktree
WTM_HOOK_NAME      worktree name
WTM_HOOK_BRANCH    branch checked out
WTM_HOOK_BASE_REF  base ref as given
WTM_HOOK_BASE_SHA  resolved base commit
WTM_HOOK_MAIN      absolute path of the main worktree
WTM_HOOK_REPO_ID   repo id
WTM_HOOK_METHOD    "cow" or "checkout"
```

All eight are always set, so a hook may run under `set -u`. A variable that
does not apply is empty rather than absent, such as `WTM_HOOK_METHOD` for a
`wtm init` rerun, which cannot know how the tree was populated.

The `GIT_*` scrub matters because a hook almost certainly runs git. An
inherited `GIT_DIR` would point it at the caller's repository rather than at
the worktree it was handed.

The `WTM_HOOK_` prefix separates the two directions. `WTM_<KEY>` names are
**input** to `wtm`, read as configuration overrides (section 3); these are
**output**, describing the worktree that was just made. Under one namespace,
a hook that starts anything which later runs `wtm` would pass this worktree's
base off as a configuration override, and a long-lived process started by a
hook would carry it for its whole life.

`WTM_HOOK_SOURCE` returns with `--from` (section 14); while the source is
always the main worktree it would duplicate `WTM_HOOK_MAIN`.

Failure: a non-zero exit keeps the worktree and prints "init hook failed
(exit N); worktree kept at <path>; rerun with: wtm init <name>", and `wtm
new` exits 3 after printing the path. wtm reports the outcome here and
nowhere else, and never writes it down. A hook that needs no shell can still
be a small script; there is no inline-command form in v1.

`--no-init` skips the hook, resolution and all.

## 8. Removing a worktree

### 8.1 Steps

```
resolve worktree by name (must be under the repo's data dir)
refuse if cwd is inside it
refuse if dirty and not --force            (git status --porcelain non-empty)
if --wait: delete synchronously, then prune, done
if locked (only reachable with --force): git worktree unlock
trash = <root>/<repo-id>/.trash            (mkdir -p)
rename(<worktree>, <trash>/<name, slashes as dashes>-<pid>-<nanos>)
    EXDEV or EBUSY -> delete synchronously instead, with a note
git -C <main> worktree prune
delete the branch only with -d (git branch -d) or -D (git branch -D); default keeps it
spawn reaper (8.3)
```

`rename(2)` is called directly, never `mv`, so a cross-filesystem move
fails instead of silently copying. `git worktree prune` is cheap: it only
checks whether each registered path exists, and it takes git's own
per-worktree metadata directory with it.

Prune ignores a locked worktree however long its directory has been gone,
which is why the unlock happens before the rename. Skipping it would leave
git holding a record of a path that nothing can ever clear.

### 8.2 Synchronous delete

Used for `--wait` and for the rename fallbacks. On macOS, first
`chflags -R nouchg` equivalent (clear `UF_IMMUTABLE`) because cloned
locked files are locked too; on both platforms, `chmod u+rwx` on
directories that refuse. Remove with a recursive unlink that treats
`ENOENT` as success. Report paths it could not remove and exit 1 for those.

### 8.3 The reaper

`wtm` re-executes itself as `wtm gc --detach --trash <dir>` (hidden flags).
`--trash` names the one directory to sweep, which is also why a reaper needs
no repository and is answered before discovery:

- child is created with `setsid()` in the pre-exec hook so terminal close
  and shell exit cannot signal it, and the parent gives it null streams so no
  descriptor of the caller's is ever inherited, even briefly;
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
delete entry synchronously (8.2), the entry directory included, ENOENT is success
close(fd)
```

An entry that has vanished between the listing and the open belongs to
whoever unlinked it and is counted as theirs, not as a deletion of ours.

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
      local out rc
      out="$(command wtm "$@")"
      rc=$?
      if [ -d "$out" ]; then
        cd "$out" || return $?
      elif [ -n "$out" ]; then
        printf '%s\n' "$out"
      fi
      return $rc
      ;;
    *)
      command wtm "$@"
      ;;
  esac
}
```

The exit code is always the binary's. Exit code 3 from `new` (hook failed)
still changes directory, because the worktree exists; the path is printed
instead only when there is no directory to enter. Bash uses the same text;
fish uses its own syntax, and needs `string collect` so the multi-line
function survives its command substitution.

One fish difference is visible to users: stderr written inside a command
substitution ignores a redirection applied to the enclosing function call,
so `wtm new x 2>/dev/null` still shows progress under the fish wrapper
where zsh and bash suppress it. Stdout, which is the contract, is
unaffected, and `--quiet` silences progress at the source in every shell.
Working around it would mean juggling file descriptors inside the wrapper,
which is not worth the fragility.

Completion scripts are a later addition.

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
  name fails at the name-is-free check. `wtm` takes no locks of its
  own because it writes no shared files.
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
needed). Measured on macOS 26: `/private/tmp`, `$TMPDIR` and `/Users` are
all on the same data volume, so none of them is a way to provoke `EXDEV`.
The volume that is separate is the sealed system volume at `/`, which is
read-only and holds nothing a test could write.

Linux: `ioctl(dst_fd, FICLONE, src_fd)` from `<linux/fs.h>`. Same
filesystem and mount required. Data only; the caller copies mode and
mtime. Supported on btrfs, XFS (reflink=1), bcachefs, ZFS 2.2+ when block
cloning is enabled; not ext4 or tmpfs. `copy_file_range` is the fallback
for a single file that refuses to clone mid-walk, but a whole-tree
fallback is the checkout path, not a userspace copy.

Both: `rename(2)` is atomic within a filesystem and O(1) in tree size.
`flock(2)` works on directory descriptors on both.

## 13. Testing

The strategy and the acceptance tests are in `TESTING.md`.

## 14. Not in v1

Carrying uncommitted changes into the new worktree (`--dirty`), inline hook
commands in config, symlinked shared caches, `--from` and closest-worktree
source selection (both wait for the clone that gives them a purpose),
trust prompts for repo hooks, btrfs subvolume snapshots, Windows,
shell completions, `wtm agent install`, and anything about macOS Spotlight
indexing: a user who minds it can exclude the directory themselves, from
the init hook or from the Spotlight Privacy settings.

## 15. Crate layout

Modules, types and signatures are in `ARCHITECTURE.md`. Dependencies:
`clap`, `serde` and `toml`, `serde_json`, `libc` for `clonefile`, `FICLONE`,
`setsid`, `flock` and priorities, and `sha1` and `sha2` for the repo id and
the index checksum. No git library: every git operation is a subprocess
call whose behaviour must match the user's git exactly.
