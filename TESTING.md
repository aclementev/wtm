# wtm testing strategy

Goal: high confidence in the parts that can silently corrupt a checkout,
with a small suite. Three ideas do most of the work: **differential tests
against git** (git is the oracle for what an index or a tree should look
like), **property tests** for the pure logic, and **fault injection** for
rollback. Trivial tests (a getter, a clap flag, a serde round-trip of a
struct with no invariants) are not written.

## 1. Test infrastructure

- `tests/common/repo.rs`: a builder that creates a small git repo in a
  temporary directory: `RepoBuilder::new().files(n).ignored("node_modules", 50)
  .scattered_ignored("__pycache__", 20).symlink_to_dir().submodule().dirty_file()
  .include(".env.local").build()`. Deterministic content from a seed.
- Temporary directories live under `target/tmp/` (same volume as the
  checkout), never under `/tmp`: on macOS `/tmp` is a separate APFS volume
  and every clone would fail with `EXDEV`.
- A `TestGit` helper runs git with the same scrubbed environment as the
  binary, so oracle and subject see identical repos.
- The binary is exercised through `assert_cmd` for end-to-end tests and
  through the library crate for module tests. Keep `main.rs` thin so the
  library covers everything.
- CI matrix: macOS (APFS, CoW path), Linux with a btrfs loop mount (reflink
  path), Linux ext4 (must take the checkout path and still pass every
  behavioural test). The same behavioural suite runs on all three; only
  `WalkStats` expectations differ.

## 2. Differential tests against git (the core)

### 2.1 Index parser and writer

For each index version 2, 3 and 4, and for SHA-1 and SHA-256 repos:

1. Build a random repo (random paths including deep, unicode and
   shared-prefix names to exercise v4 compression; random modes 100644,
   100755, 120000; a gitlink; a merge conflict for stage entries; an
   intent-to-add entry; a skip-worktree entry), `git update-index
   --index-version N`.
2. Parse `.git/index`. Compare every entry with `git ls-files --debug -s
   -z` (paths, modes, oids, stages, all seven stat fields). This is the
   only source of truth for the parser.
3. Write the parsed index back, byte-compare with the original after
   removing the extensions the writer drops. Then run `git fsck` and
   `git status` on the rewritten index: both must succeed and status must
   be unchanged.

Property-test variant (proptest): generate `Index` values directly
(arbitrary entries with sorted unique paths, arbitrary extensions) and
assert `parse(write(x)) == x` for every version. This catches padding and
varint bugs that the small git-made fixtures miss.

### 2.2 Stat rewrite proves git trusts the index, without tautology

After `rewrite_stat` and `install`, asserting "status is clean" is weak:
git could be re-hashing everything and still say clean. Two stronger
checks:

- **The lie test.** Pick a tracked file in the new worktree, overwrite its
  content with different bytes of the same length, then restore its mtime
  with `touch -r` from a copy. `git status` must report it **clean**. That
  proves git accepted the stat data and did not read the content. (Then
  restore the file.)
- **The detection test.** Plant each of: append, same-size overwrite with
  a new mtime, delete, chmod +x (when `core.fileMode` is true), a file
  modified within the same second as the index write. `git status` must
  report every one.

Run both on every index version and on the checkout path (where they must
also hold, trivially).

### 2.3 The clone walk equals `git worktree add` plus includes

For a random repo with random ignored and untracked files, random
`.worktreeinclude` patterns (drawn from a small grammar: exact file,
directory with trailing slash, `*.ext`, negation), compare the sorted
`find`-style listing of the `wtm new` tree against the union of the
`git worktree add` tree and the files git reports for the include patterns.
Both sides come from git, so the test cannot agree with a bug in our
matching. Also assert the source's untracked and ignored files that are
not included are absent, and that a symlink to a directory at the top
level is still a symlink in the result (the `CLONE_NOFOLLOW` regression).

### 2.4 ExcludeSet equivalence

If the single `git status --porcelain=v2 --ignored=matching` query is
implemented, a property test feeds random repos to both it and the three
`ls-files` queries and asserts the resulting `ExcludeSet` classifications
are identical for every path in the tree.

## 3. Property tests for pure logic

- `WorktreeName`: for arbitrary strings, either parsing fails or
  `layout.worktree_dir(id, name)` canonicalizes to a path strictly inside
  `layout.repo_dir(id)`, has no `..` components, and `trash_stem` contains
  no `/`. Also: parse is idempotent and round-trips through `Display`.
- `ExcludeSet::from_lists`: the invariants in `ARCHITECTURE.md` 3.5, checked
  over random path sets: ancestors of `Included` or `Mixed` are `Mixed`;
  descendants of `Excluded` are never visited (`classify` of a descendant of
  an excluded path that is not itself included returns `Excluded`); an
  included path under an excluded ancestor makes that ancestor `Mixed`.
- `Walker` against the `fake` cloner on a random tree with a random
  `ExcludeSet`: the destination equals the source minus excluded paths plus
  included ones, and `tree_clones + dirs_recursed` never exceeds the number
  of `Mixed` nodes plus one. This runs on any filesystem and is the fast
  test for the walk logic; 2.3 is the slow one that also covers git.
- Derivations (`DESIGN.md` 2.2), one test each, since these replace stored
  state: the creation time read from the worktree directory's birth time is
  within a second of the clock at creation; the base derived by merge-base
  equals the commit `--base` named, including when the base is a tag or a
  raw sha; git's metadata directory name is read, not computed, proven by
  creating worktrees whose paths share a basename so git appends a digit,
  then asserting both resolve correctly. Recovering the repo behind a
  `<repo-id>` directory is tested with `wtm gc`, which is what needs it.
- `Config`: one table-driven test over every key and every subset of the
  layers that key accepts, asserting both the merged value and its `Origin`.
  A key is absent from a layer on purpose (`DESIGN.md` 3), so the table
  also pins which layers each key accepts: a project file setting a
  personal key is rejected, as are unknown keys, both naming file and key.
- `RepoId`: symlinked and non-symlinked paths to the same directory give
  the same id; different directories with the same basename give different
  ids; the id matches `^[^/]+-[0-9a-f]{8}$`.

- The preconditions of `DESIGN.md` 5 (`create::check`) are a pure function
  of a `Request` and an `Observed`, so each rule is a table row rather than
  a repository fixture: one case per refusal, one asserting the specified
  order when two rules both apply, and one per branch outcome. This set
  grows with every later feature, which is why it is kept pure.

## 4. Fault injection for rollback

An internal environment variable, `WTM_TEST_FAIL_AT=<step>` (compiled in
only with `cfg(test)` or a `test-hooks` feature), makes `create` return an
error at a named step: `after_worktree_add`, `during_walk`, `after_index`,
`after_checkout`. For each step, assert afterwards: the destination does
not exist, `git worktree list` does not mention it, `wtm`-created branches
are gone, the data root gained no directories, and a second `wtm new` with the same
name succeeds. This replaces mocking with real failures at
real boundaries.

Hook failure is a separate case: a hook that exits 7 must leave the
worktree, print the path on stdout, name the exit code on stderr and return
exit code 3; `wtm init` with a fixed hook then exits 0.

## 5. Removal and reaping

- `wtm rm` on a clean worktree returns with the path gone, `git worktree
  list` no longer showing it, and exactly one entry in `.trash` still
  holding every file. The entry is the assertion that matters: a removal
  that went back to deleting synchronously leaves none. A wall-clock bound
  rides along at two seconds, loose on purpose, because a tight one fails on
  a loaded machine for reasons that have nothing to do with the code.
- Tests set `WTM_NO_REAPER` unless they are testing reaping. `wtm rm` spawns
  a reaper for the entry it has just made, so what is in the trash cannot be
  asserted while one is racing to empty it.
- Dirty worktree: exit 4, nothing changed; `--force`: removed.
- `-d` on an unmerged branch: worktree removed, branch kept, message
  printed; `-D`: branch gone.
- A trash that cannot be renamed into: the synchronous delete runs and the
  command exits 0. Provoked without needing a second filesystem by leaving a
  file where the `.trash` directory has to be created, which is a real
  failure at the real boundary rather than an injected one.
- The `EXDEV` case itself needs a second filesystem, which not every machine
  has. `WTM_TEST_XDEV_DIR` names a directory on one; the test checks
  `st_dev` really differs and reports that it did not run when the variable
  is unset, rather than passing quietly. CI sets it.
- Zero-state invariant: after a full lifecycle (`new`, `init`, `ls`, `rm`,
  `gc`) on a scratch HOME, the only paths `wtm` created outside the data
  root and the repo are none. Assert by snapshotting the filesystem under
  `$XDG_CONFIG_HOME`, `$XDG_STATE_HOME`, `$XDG_CACHE_HOME` and `$HOME`
  before and after, allowing only the reaper log when `WTM_DEBUG` is set.
  This is the test that keeps the design honest as features are added.
- Sweep concurrency: populate a trash, run two `wtm gc --wait` at once,
  assert both exit 0 and that the deletions they report add up to exactly
  what was planted. The sum is the part with teeth: emptying the trash
  happens with or without the locks, but only the locks stop both sweepers
  claiming every entry. The sum holds however the two are scheduled. Sweep resumption: start a sweep as a subprocess on a large entry,
  `SIGKILL` it mid-way, assert the lock is free (a new sweeper acquires it)
  and a second sweep empties the trash.
- Reaper detachment: run `wtm rm` with stdout on a pipe over a trash big
  enough that a sweep cannot finish quickly, read to end of file, then
  assert the trash is still populated. A reaper holding the caller's stdout
  could only reach end of file after an empty trash, so this needs no
  timing threshold. This is the regression that would hang `$(wtm rm x)`.
- The sweep is nobody's chore: plant a trash, run `wtm ls`, and it empties
  on its own.

## 6. Output contracts and docs

- Snapshot tests (`insta`) for `--help` of every command, `wtm agent
  skill`, and `wtm shell zsh|bash|fish`. Any wording change is a reviewed
  snapshot update, and the skill cannot drift from the flags because both
  come from the same definitions.

  The `.snap` files are committed, so an accepted change arrives as a diff
  in the pull request. Working on them needs `cargo install cargo-insta`:

  | command | what it does |
  |---|---|
  | `cargo test` | fails on any mismatch, writes the new output beside the old as `.snap.new` |
  | `cargo insta review` | shows each pending change and asks to accept or reject it |
  | `cargo insta accept` | accepts every pending change without looking, for when the diff has already been read |
  | `cargo insta test --check` | fails on a mismatch and writes nothing; this is the CI form |

  Do not set `INSTA_FORCE_UPDATE`. It writes straight over the `.snap`,
  skipping the pending step that makes a change reviewable, and it leaves
  an `assertion_line` field in the file that `accept` would have stripped,
  so every snapshot churns when a line moves in the test file.
- The shell wrapper is executed for real: a test spawns `zsh -c 'eval
  "$(wtm shell zsh)"; wtm new t >/dev/null; pwd'` and asserts the printed
  directory. Same for bash and fish. A second test asserts the wrapper
  returns the binary's exit code, which a snapshot cannot show and which a
  careless wrapper loses by returning the status of its last `cd` or
  `printf`. A shell that is not installed fails the test rather than
  skipping: a wrapper nobody runs is a wrapper nobody has checked.
- stdout purity: for `wtm new`, stdout is exactly one line, the path, even
  when the hook writes to stdout, even on exit code 3. The case is also run
  with `WTM_DEBUG` set; until the reaper exists that variable produces no
  output, so it is a guard against future diagnostics reaching the wrong
  stream rather than coverage of anything today.

## 7. Performance guard

One `#[ignore]` benchmark test builds a 100k-file repo and asserts `wtm new`
is not more than 30% slower than `git worktree add -c checkout.workers=8`
and that the first `git status` takes under one second. Run in CI nightly,
not per commit. The research scripts under `research/git/` remain the
reference for anything slower than expected.

## 8. What not to test

Clap parsing of individual flags (the snapshot covers the surface), serde
round-trips of plain structs, `Layout` path joins, and anything that only
restates the implementation. If a test would pass with the function body
replaced by the obvious wrong thing, it is not worth keeping.
