# wtm testing strategy

Goal: high confidence in the parts that can silently corrupt a checkout,
with a small suite. Three ideas do most of the work: **differential tests
against git** (git is the oracle for what an index or a tree should look
like), **property tests** for the pure logic, and **fault injection** for
rollback. Trivial tests (a getter, a clap flag, a serde round-trip of a
struct with no invariants) are not written.

## 1. Test infrastructure

- `tests/common/repo.rs`: a builder that creates a small git repo in a
  temporary directory: `RepoBuilder::new(label).files(n).ignored("node_modules", 50)
  .symlink_to_dir().submodule().dirty_file().build()`, with fixed content.
- Temporary directories live under `target/tmp/`, which `cargo clean`
  removes and no other user shares. On macOS this is not about volumes:
  `/private/tmp` and `$TMPDIR` are on the same data volume as `$HOME` and
  clone from it perfectly well.
- `TestRepo::git` runs git with the same scrubbed environment as the
  binary, so oracle and subject see identical repos.
- The binary is exercised through `assert_cmd` for end-to-end tests and
  through the library crate for module tests. Keep `main.rs` thin so the
  library covers everything.
- CI matrix: macOS (APFS, CoW path), Linux with an XFS loop mount (reflink
  path), Linux ext4 (must take the checkout path and still pass every
  behavioural test). The same suite runs on all three; the tests that only
  mean something on the CoW path say so when they do not run.

## 2. Differential tests against git (the core)

### 2.1 Index parser

For index versions 2, 3 and 4, in SHA-1 and SHA-256 repositories: build a
repository with deep, unicode and prefix-sharing paths (for v4
compression), a symlink, an executable, a gitlink, and for v3 and v4 an
intent-to-add entry to set the extended flags. Set the version with `git
update-index --index-version N`, then compare the mode, stage and path of
every parsed entry with `git ls-files -s -z`. Every truncation of a real
index, and a flipped byte, must be refused.

There is no writer, so there is no round-trip property. The fill changes
40-byte prefixes and the checksum, and nothing else.

### 2.2 The fill proves git trusts the index, without tautology

"Status is clean" is weak: git re-hashing every file also ends in a clean
status. The oracle is `git diff-files --name-only`, which lists every entry
whose stat data does not match the file on disk and decides that from stat
alone. After a fill it must list exactly the entries left zeroed on purpose.
If the fill wrote nothing it lists everything; if one field is wrong on one
entry, it lists that entry.

Fixture files carry a 2020 mtime, set before `git add`. A file written in
the same second as the index is racy, git checks its content whatever its
stat data says, and `diff-files` would pass with wrong stat data.

- **Git trusts the fill and still sees changes.** On v2 and v4: zero the
  index with `read-tree HEAD`, delete one tracked file, fill. `diff-files`
  lists only the deleted file. Then plant an append, a same-size
  overwrite, a chmod +x and a symlink retarget; `git status` reports each.
- **A change after `since` is left for git.** Overwrite a file with
  same-size content and restore its old mtime, after `since`. The fill
  must leave it zeroed. This is the edit that is otherwise cloned with new
  content and reported clean.
- **The mode comes from HEAD.** With `core.fileMode` false and an
  executable bit on disk that HEAD does not have, `git diff --cached` is
  empty after the fill.
- **Both creation paths keep the clone** (macOS). `wtm new` with and
  without `WTM_NO_FAST_INDEX`: clean status, HEAD's content in a file
  dirty in the source, and an untouched file still carrying the source's
  mtime, which a rewrite by git would replace with the current time.

An earlier draft had a "lie test": swap a file's content for different
bytes of the same length, restore its mtime, and expect a clean status. It
cannot pass. The overwrite moves the ctime, which nothing can restore, and
git compares ctime.

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

Listings alone cannot tell a clone from a checkout that arrived at the
same contents, and the checkout is what a mistake in the creation sequence
silently falls back to. On the CoW path, assert that an untouched tracked
file still carries the source's mtime: a checkout stamps the current
time, a clone does not.

## 3. Property tests for pure logic

- `WorktreeName`: for arbitrary strings, either parsing fails or
  `Workspace::dir` gives a path strictly inside `Workspace::repo_dir` with
  no `..` components. Also: parsing is idempotent, and `name_of` recovers
  the name from the path `dir` gives.
- `ExcludeSet::from_lists`: the invariants in `ARCHITECTURE.md` 3.5, checked
  over random path sets: every ancestor of an included path is `Recurse`,
  so the walk can reach it; a descendant of an excluded path that is not
  itself included is `Skip`, so the walk never visits it; an included path
  is `CloneWhole` however deep inside an excluded tree it sits.
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

## 4. Rollback

The cleanup after a failed creation does the same thing wherever the
failure happened, so one real failure at the last step that can fail, with
the tree fully populated, covers it: an untracked file carried by
`.worktreeinclude` that the base tracks makes git refuse the branch
checkout. Assert afterwards that the data root is empty, git has no
record or metadata of the worktree, no branch was left, and a retry with
the file gone succeeds. No fault-injection switch is compiled in.

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

## 7. What not to test

Clap parsing of individual flags (the snapshot covers the surface), serde
round-trips of plain structs, `Workspace` path joins, and anything that only
restates the implementation. If a test would pass with the function body
replaced by the obvious wrong thing, it is not worth keeping.
