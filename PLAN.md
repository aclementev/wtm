# Implementation plan

Read `DESIGN.md` (behaviour), `ARCHITECTURE.md` (modules and types) and
`TESTING.md` (how to verify) before starting. This file says in what order
to build it.

## How to work through this

- **The tool works at the end of every step.** Step 1 produces a `wtm` that
  really creates, lists and removes worktrees, using plain git underneath.
  Every later step replaces an internal with a better one or adds a feature.
  No step leaves the binary broken or half-wired.
- **One step per branch, merged when its acceptance criteria pass.** The
  criteria are written as tests; if a criterion cannot be tested, say so in
  the pull request rather than skipping it.
- **Write the tests named in the step, not more.** `TESTING.md` section 8
  lists what not to test. A test that would pass with the function body
  replaced by the obvious wrong thing is not worth keeping.
- **Do not reorder the clone steps.** Steps 6 to 9 are split so that each
  one is independently correct: primitives, then the walk, then a correct
  but slow integration, then the speed. Skipping to the fast index without
  the slow-but-correct integration removes the oracle that proves it.
- **When the design is wrong, change the design document in the same pull
  request.** Do not let code and specification drift.

## Step 1. Walking skeleton

The whole command surface, real behaviour, plain git underneath.

Build: cargo project; `cli.rs` with every subcommand from `DESIGN.md`
section 4 declared; `error.rs` with the exit codes; `ui.rs` enforcing the
stdout/stderr split; `git.rs` with the scrubbed-environment subprocess
wrapper and `worktree list --porcelain` parsing; `name.rs`; `repo.rs` with
discovery and `RepoId`; `layout.rs`. Wire `new`, `ls`, `cd`, `rm` to
`git worktree add`, `list` and `remove`. Hard-code the data root from XDG;
no config files yet. Commands not yet implemented exit 2 saying so.

Also build the test harness from `TESTING.md` section 1: `RepoBuilder`,
temporary directories under `target/tmp`, `assert_cmd` wiring.

Done when: on a scratch repo, `wtm new x` prints a path and `git worktree
list` shows it; `wtm ls` lists it; `wtm rm x` removes it; `wtm new x`
writes nothing but the path to stdout; exit codes match section 4; the
`RepoId` property test passes.

## Step 2. Configuration and identity

Build: `config.rs` with `Setting<T>`, `Origin` and the five-layer
precedence; `--dir`/`WTM_DIR`; `wtm config`; name validation wired into
`new`; the preflight checks of `DESIGN.md` section 5 that do not involve
cloning (in-progress operation, destination exists, branch rules);
`wtm doctor` reporting git version, paths, volumes and whether the data
root and repo share a filesystem.

Done when: the config precedence table test passes for every key and every
subset of layers; unknown keys are rejected naming file and key; the
`WorktreeName` property test passes; `wtm new` refuses a repo mid-rebase.

## Step 3. The user-facing surface

Build: `wtm shell` for zsh, bash and fish; `wtm agent skill` generated from
the command definitions; `--json` for `ls`, `doctor` and `config`;
`--quiet`; help text for every command.

Done when: the snapshot tests for help, skill and shell wrappers exist; the
wrapper is executed in each real shell and changes directory; the stdout
purity test passes, including with the debug variable set.

## Step 4. Init hook

Build: `hook.rs`, resolution order from `DESIGN.md` section 7, the
environment variables, execution with the new worktree as cwd, exit code 3
on failure without removing the worktree, and `wtm init`.

Done when: a hook sees every documented variable and the right cwd; a
failing hook leaves the worktree, prints the path on stdout and returns 3;
a missing default hook is silent while a missing configured hook is an
error; `--no-init` skips it.

## Step 5. Instant removal

The first large user-visible win, and independent of cloning.

Build: `remove.rs` with the direct `rename` call and the synchronous
fallback on `EXDEV` and `EBUSY`; `delete_tree_sync` including the immutable
flag clearing; `reaper.rs` with `setsid`, the file-descriptor redirection,
the priority lowering and the `flock` sweep; `wtm gc`; the `-d` and `-D`
branch flags; the opportunistic sweep on other commands.

Done when: `wtm rm` returns in under 200 ms on the test repo and the path
is gone; command substitution around `wtm rm` does not hang; a dirty
worktree exits 4 and `--force` removes it; two concurrent sweeps both exit
0 and empty the trash; a sweeper killed mid-delete leaves a lock that the
next sweep acquires and finishes; a cross-filesystem trash falls back to a
synchronous delete.

## Step 6. Clone primitives

Build: the `Cloner` trait with the clonefile, reflink and fake
implementations; the probe; `decide`. Report the chosen method and the
reason in `wtm doctor`. Nothing is wired into creation yet.

Done when: a directory clone and a file clone work on the platform under
test; a top-level symlink to a directory survives as a symlink, which is
the `CLONE_NOFOLLOW` regression; the probe reports unsupported across
volumes, which on macOS can be provoked with a path under `/tmp`; `decide`
is table-tested for the three modes.

## Step 7. Exclusion

Build: `exclude.rs` with the git queries, the trie and the four-way
classification; `Walker` over the `Cloner` trait.

Done when: the invariants in `ARCHITECTURE.md` section 3.5 hold under
property testing; the walker against the fake cloner reproduces source
minus excluded plus included on random trees; the walk statistics stay
within the bound in `TESTING.md` section 3.

## Step 8. Copy-on-write creation, correct before fast

Wire the clone into `create.rs` behind an explicit `--clone-mode cow`,
using `git read-tree HEAD` for the index. The result is correct and the
first `git status` is slow. This is the oracle for step 9.

Build: `create.rs` orchestration for the clone path, submodule emptying,
the reset of cloned uncommitted changes, and the branch step.

Done when: the differential test against `git worktree add` plus the
include patterns passes; `git status` is clean; switching to another branch
works; ignored files from the source are absent unless included; submodule
directories are empty.

## Step 9. The fast index

Build: `index/` with the parser for versions 2, 3 and 4 and both hash
algorithms, the writer, the stat rewrite with the racy smudge, and
installation. Make `auto` the default clone mode. Add the fallback to
`core.checkStat=minimal` for split index and anything unparseable.

Done when: the parser matches `git ls-files --debug` on generated repos of
every version; the property round-trip passes; the lie test shows git
trusting our stat data; every planted mutation is still detected; the first
`git status` on the 100k-file repo is under a second.

## Step 10. Hardening

Build: the `Rollback` guard and the fault-injection hook; the remaining
preflight checks (sparse source falls back to checkout, dirty source
handling); the missing-file and gitlink edge cases in the stat rewrite;
warnings that name the override that would fix them.

Done when: failure injected at each of the four named steps leaves no
directory, no git metadata, no stray branch and allows an immediate retry;
the zero-state invariant test passes over a full lifecycle.

## Step 11. Ship

Build: the CI matrix of macOS on APFS, Linux on a btrfs loop mount and
Linux on ext4 where the checkout path must still pass everything; the
nightly performance guard; README with install instructions; version 0.1.

Done when: all three CI jobs are green and the README documents the
`.worktreeinclude` file, the hook contract and the configuration keys.

## Deliberately after 0.1

Carrying uncommitted changes into a new worktree, inline hook commands,
symlinked shared caches, choosing the clone source by commit distance,
trust prompts for repository hooks, btrfs subvolume snapshots, Windows,
shell completions, and `wtm agent install`.
