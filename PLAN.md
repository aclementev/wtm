# Implementation plan

Read `DESIGN.md` (behaviour), `ARCHITECTURE.md` (modules and types) and
`TESTING.md` (how to verify) before starting. This file says in what order
to build it: six features, each one something a user would notice.

## How to work through this

- **The tool works at the end of every feature.** The first one produces a
  `wtm` that really creates, lists and removes worktrees, using plain git
  underneath. Every later one replaces an internal with a better one or
  adds behaviour. No stage leaves the binary broken or half-wired.
- **One feature per branch, merged when its criteria pass.** The criteria
  are written as tests; if one cannot be tested, say so in the pull request
  rather than skipping it.
- **Write the tests named here, not more.** `TESTING.md` section 7 lists
  what is not worth testing.
- **When the design is wrong, change the design document in the same
  commit.** Do not let code and specification drift.

## 1. The CLI exists and manages worktrees

The whole command surface from `DESIGN.md` section 4, with real behaviour,
backed by plain `git worktree add`, `list` and `remove`. Repository
discovery and identity, the layout under the data root, name validation,
the five-layer configuration, the stdout and stderr split, shell
integration, the agent skill, `doctor`, JSON output, and the test harness
of `TESTING.md` section 1. `init` and `gc` exit 2 saying they are not
implemented yet.

Done when: a worktree can be created, listed, entered and removed on a
scratch repository; `wtm new` writes nothing to stdout but the path, even
with debugging enabled; exit codes match section 4; the identity, name,
configuration-precedence and derivation tests pass; help, skill and the
three shell wrappers have snapshots and each wrapper is executed in its
real shell.

## 2. Worktrees initialize themselves

The per-project hook: resolution through flag, environment, project
configuration, global configuration and the default file; execution in the
new worktree with the documented variables; the rule that a failed hook
keeps the worktree, still prints the path and exits 3; `wtm init` to rerun
it; `--no-init` to skip it.

Done when: a hook sees every variable and the right working directory; a
hook exiting non-zero leaves a usable worktree and returns 3; a missing
default hook is silent while a missing configured one is an error; nothing
about the outcome is written to disk.

Independent of feature 3; either order.

## 3. Removal returns immediately

The rename into the trash, the synchronous fallback when the rename cannot
happen, the detached reaper with its file-descriptor and priority handling,
the lock-based sweep, `wtm gc`, and the branch-deletion flags.

Done when: removal returns in under 200 ms on the test repository and the
path is gone; command substitution around it does not hang, which is the
inherited-descriptor regression; a dirty worktree exits 4 and `--force`
removes it; two concurrent sweeps both exit 0 and empty the trash; a
sweeper killed mid-delete leaves a lock the next sweep can take; a trash
that cannot be renamed into falls back to a synchronous delete.

## 4. Creation uses copy-on-write

The clone primitives for both platforms with a test double, the probe and
the method decision, the exclusion set and `.worktreeinclude`, the walk,
and the wiring into creation. Correct but not yet fast: the index comes
from `read-tree` with no stat data, so git hashes every file once before
the tree is clean, and that second or more is spent inside `wtm new`.

Done when: the resulting tree equals what `git worktree add` produces plus
exactly the files the include patterns match; ignored files are absent
unless included; a top-level symlink to a directory survives as a symlink;
submodule directories exist and are empty; status is clean and switching
branches works; the exclusion invariants hold under property testing; the
on a filesystem without cloning the checkout path still passes every
behavioural test.

## 5. Creation is fast

The index parser and the in-place stat fill of the index `read-tree`
wrote, guarded by the source's ctime; the refresh as the fallback for
anything not understood; `WTM_NO_FAST_INDEX` to skip the fill.

Done when: the parser matches `git ls-files -s` on generated repositories
of every index version and both hashes; `git diff-files` after a fill
lists only the entries left zeroed on purpose; a file changed after the
dirty query is left for git; every planted mutation is still detected;
creation on a hundred-thousand-file repository no longer hashes the tree,
and the first status is under a second.

This is the riskiest code in the project. Feature 4 stays in place as its
oracle: the two must produce identical trees, one of them without any index
cleverness. Do not merge the two features.

## 6. Hardening and release

Rollback after a failed creation, the remaining preconditions and edge
cases, warnings that name the override which would change the outcome, the
CI matrix of macOS on APFS and Linux on both a reflink filesystem and ext4
(with rustfmt and clippy), the README, and version 0.1.

Done when: a creation that fails with the tree fully populated leaves no
directory, no git metadata and no stray branch, and an immediate retry
works; the zero-state invariant test passes over a full lifecycle; all
three CI jobs are green.

## Deliberately after 0.1

Carrying uncommitted changes into a new worktree, inline hook commands in
configuration, symlinked shared caches, choosing the clone source by commit
distance, trust prompts for repository hooks, btrfs subvolume snapshots,
Windows, shell completions, `wtm agent install`, and anything about macOS
Spotlight indexing.
