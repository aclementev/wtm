# wtm architecture

Companion to `DESIGN.md` (behaviour) and `TESTING.md` (verification). This
document fixes the module boundaries, the key types and the signatures an
implementer should keep, so the code stays clean when written by several
hands. Names are binding unless a good reason is recorded in the code.

## 1. Principles

- **Git is a subprocess, never a library.** Every git operation goes
  through one wrapper (`git::Git`) that scrubs the environment. Behaviour
  must match the user's git exactly; no libgit2 or gitoxide.
- **Parse, don't validate.** Raw strings become typed values once, at the
  edge: `WorktreeName`, `RepoId`, `Config`. Everything downstream takes the
  typed value and cannot be handed a bad one.
- **Platform code behind one trait.** `Cloner` has an APFS and a Linux
  implementation plus a test double; nothing else contains `cfg(target_os)`.
- **Side effects are orchestrated in one place per command.** `create.rs`
  and `remove.rs` sequence the steps and own rollback; the modules they call
  are pure or single-purpose.
- **Ask, then decide.** A command gathers what git and the filesystem say in
  one step and applies its rules to that value in the next, so the rules are
  pure functions over plain data and testable without a repository on disk.
  Git re-enforces every rule when it runs; the checks exist for the message.
- **A struct needs an invariant.** Types exist to make a bad state
  unrepresentable (`WorktreeName`, `BranchState`) or to pair an operation
  with its inverse (`Workspace`). A bundle of values that travel together is
  a parameter list, not a type.
- **Errors carry the exit code.** One error enum, `Error::exit_code()`,
  and `main` is the only place that prints an error and exits.
- **No global state in the process.** No statics, no `lazy_static` config,
  and no ambient context object: `run` builds `Git`, `Ui`, `Workspace` and
  `Config` in that order and hands each function the ones it uses. A
  signature naming four of them is reporting that the command touches four
  things, which is worth seeing.

## 2. Module map

```
src/
  main.rs         parse, call `run`, print the error, map the exit code. Nothing else.
  lib.rs          `run`: dispatch, and the four lines that build git, repo, config and workspace
  cli.rs          clap structs: `Cli`, `Command`, per-command arg structs. No logic.
  config.rs       `Config`, `Setting<T>`, `Origin`, loading and precedence
  ui.rs           `Ui`: output policy. `emit` to stdout, progress/warn/relay to stderr, `--quiet`
  error.rs        `Error` enum (thiserror), `exit_code()`
  git.rs          `Git` subprocess wrapper; `Oid`, `GitWorktree`; typed helpers for the handful of commands wtm uses
  repo.rs         `Repo` discovery from cwd/--repo; `RepoId`; the derived `WorktreeView`
  name.rs         `WorktreeName` newtype and validation
  workspace.rs    `Workspace`: a repo plus the data root. `dir` and its inverse `name_of`
  exclude.rs      `ExcludeSet` computation via git; `PathTrie` and `Class`
  clone/
    mod.rs        `Cloner` trait, `Walker`, `Method`, `MethodDecision`, probe
    apfs.rs       clonefile(2) implementation
    reflink.rs    FICLONE per-file implementation
    fake.rs       test double (plain copy, records calls)
  index.rs        `HashAlgo`, `Entry`, the parser, and the in-place stat fill
  hook.rs         `Hook` (what resolution came to), the one stat, `HookEnv`, execution
  create.rs       `wtm new` orchestration; `Rollback` guard
  remove.rs       `wtm rm`; rename to trash; synchronous delete
  reaper.rs       detach (setsid, fds, priorities) and `sweep(roots)` with flock
  shell.rs        wrapper text per shell
  docs.rs         skill markdown generated from the clap command tree plus prose
  commands.rs     `ls`, `cd`, `doctor`, `config`: thin, building inputs and formatting output.
                  A command moves to its own file once it outgrows the shared one.
```

Dependency direction: `commands/*` and `create/remove` depend on
everything below them; `git`, `index`, `clone`, `exclude`,
`layout`, `name`, `repo` depend only on `error` and `std`/`libc`. `ui`
is passed in, never imported by leaf modules.

## 3. Key types

### 3.1 Identity and names

```rust
/// Identifies a repository by its main worktree.
///
/// Format: `<basename>-<8 hex>` where the hex is the first 8 characters of
/// sha256 over the canonicalized (symlinks resolved) absolute path of the
/// main worktree, e.g. `monorepo-3f9a1c2e`. The basename keeps the data
/// directory readable; the hash separates two repos with the same name.
/// Moving or renaming a repo therefore yields a new id: its old worktrees
/// become orphans, which `wtm ls --all` reports and `wtm gc` can remove.
/// The mapping is one-way: recover the repo behind a `<repo-id>` directory
/// by reading the `.git` file of any worktree inside it, never by reversing
/// the hash.
pub struct RepoId(String);
impl RepoId {
    pub fn for_main_worktree(canonical: &Path) -> RepoId;
    pub fn as_str(&self) -> &str;
}

/// A validated worktree name: `^[A-Za-z0-9._][A-Za-z0-9._/-]*$`, no `..`
/// component, no empty component, at most 200 bytes. Doubles as the branch
/// name after the prefix. Invariant: `layout.worktree_dir(id, &name)` is
/// always strictly inside `layout.repo_dir(id)`.
pub struct WorktreeName(String);
impl FromStr for WorktreeName { type Err = Error; }
impl WorktreeName {
    pub fn as_path(&self) -> &Path;          // slashes become directories
    pub fn trash_stem(&self) -> String;      // slashes replaced by "--"
}
```

### 3.2 Repository and git

```rust
pub struct Repo {
    pub main: PathBuf,        // canonical main worktree; the repository itself when bare
    pub common_dir: PathBuf,  // .git of the main worktree
    pub id: RepoId,
    pub bare: bool,           // no working tree, so creation cannot clone
}
impl Repo {
    pub fn discover(git: &Git, from: &Path) -> Result<Repo>;   // works from inside any worktree
    pub fn worktrees(&self, git: &Git) -> Result<Vec<GitWorktree>>;   // `git worktree list --porcelain`
}

pub struct GitWorktree { pub path: PathBuf, pub head: Option<Oid>, pub branch: Option<String>,
                         pub detached: bool, pub bare: bool, pub locked: bool, pub prunable: bool }

/// Everything `wtm ls` shows, derived per DESIGN.md 2.2: no stored metadata.
/// `missing` and `locked` are not fields: git reports both in `GitWorktree`,
/// and its judgement is better than stat'ing the path ourselves.
pub struct WorktreeView { pub git: GitWorktree, pub name: WorktreeName, pub created: Option<SystemTime>, pub base: Option<Oid> }
pub fn view(git: &Git, workspace: &Workspace) -> Result<Vec<WorktreeView>>;
/// Creation time: birth time of the worktree directory (mtime fallback).
pub fn created_at(worktree: &Path) -> Option<SystemTime>;

pub struct Git { exe: PathBuf, version: GitVersion }
impl Git {
    pub fn new() -> Result<Git>;                     // finds git, checks >= 2.31
    /// Runs git with GIT_DIR, GIT_WORK_TREE, GIT_INDEX_FILE, GIT_COMMON_DIR,
    /// GIT_OBJECT_DIRECTORY removed from the environment.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Result<Output>;
    pub fn run_z(&self, cwd: &Path, args: &[&str]) -> Result<Vec<Vec<u8>>>;   // NUL-separated stdout
    pub fn rev_parse(&self, cwd: &Path, rev: &str) -> Result<Oid>;
    pub fn status_is_clean(&self, cwd: &Path) -> Result<bool>;
    pub fn in_progress_operation(gitdir: &Path) -> Option<&'static str>;  // "rebase", "merge", ...
    /// Git's metadata directory for a worktree. Git derives its name from the
    /// basename of the worktree path and appends a digit on collision, so it is
    /// NOT `<common_dir>/worktrees/<name>`. Always read back from git.
    pub fn gitdir_of(&self, worktree: &Path) -> Option<PathBuf>;
}
```

### 3.3 Configuration

```rust
pub enum Origin { Flag, Env(String), Project(PathBuf), Global(PathBuf), Default }
pub struct Setting<T> { pub value: T, pub origin: Origin }

/// Four settings, each scoped to whoever owns the decision (DESIGN.md 3).
/// Anything a caller decides per invocation is a flag, not a setting.
pub struct Config {
    pub dir: Setting<PathBuf>,            // flag, env, global
    pub base: Setting<String>,            // flag, env, project
    pub branch_prefix: Setting<String>,   //       env, global
    pub fetch: Setting<bool>,             // flag, env, global
}

/// Layers are merged key by key, each key seeing only the layers it accepts;
/// a project file that sets a personal key is rejected naming file and key.
pub fn load(flags: &FlagOverrides, env: &dyn Fn(&str) -> Option<String>,
            project_file: Option<&Path>, global_file: Option<&Path>) -> Result<Config>;
```

`RawConfig` (serde, all fields `Option`) is the file shape; unknown keys
are rejected with `deny_unknown_fields`.

`Context::build` replaces `config.dir` with its resolved form: git reports
worktree paths with symlinks resolved, and `Layout::name_of` decides whether
a worktree is ours by prefix-matching the data root against them.

### 3.4 Workspace

```rust
/// Where one repository's worktrees live. `name_of` is the inverse of `dir`,
/// and that pair is how "which worktrees are ours" is answered without a
/// registry, so the two are defined together.
pub struct Workspace { pub repo: Repo, root: PathBuf }
impl Workspace {
    pub fn new(repo: Repo, root: PathBuf) -> Workspace;
    pub fn root(&self) -> &Path;             // only `ls --all` and `gc` range over repos
    pub fn repo_dir(&self) -> PathBuf;
    pub fn dir(&self, name: &WorktreeName) -> PathBuf;
    pub fn trash(&self) -> PathBuf;
    pub fn name_of(&self, path: &Path) -> Option<WorktreeName>;
}

/// The root is canonicalized before use: git reports worktree paths with
/// symlinks resolved, and `name_of` prefix-matches against them.
pub fn canonical_root(root: &Path) -> PathBuf;
```

`wtm` persists nothing else. There is no `state.rs`, no registry, no lock
file: see `DESIGN.md` 2.2 for the table of what each fact is derived from.
An implementer who feels the need for a state file should add a derivation
instead, or raise it as a design change.

### 3.5 Exclusion

```rust
/// Named for what the walk does, not for what the path is: a path kept
/// because `.worktreeinclude` asked for it and one kept because nothing
/// objected are the same instruction, and the walk is the only caller.
pub enum Class { Skip, CloneWhole, Recurse }

/// Result of the four queries of DESIGN.md 6.3 in the source.
pub struct ExcludeSet { trie: PathTrie }
impl ExcludeSet {
    pub fn compute(git: &Git, source: &Path) -> Result<ExcludeSet>;
    pub fn from_lists(excluded: Vec<PathBuf>, included: Vec<PathBuf>) -> ExcludeSet;   // pure; tested with proptest
    pub fn classify(&self, rel: &Path) -> Class;
}
```

Invariants of `from_lists`: an included path is `CloneWhole` even under an
excluded ancestor, and every ancestor between them is `Recurse`; a path
whose nearest named ancestor is excluded, with no include below it, is
`Skip` although nothing named it; a path unrelated to every named path is
`CloneWhole`.

`Walker` ignores the root's class and always recurses there: git has
already made the destination directory and written a `.git` file into it,
so there is nothing for `clone_tree` to create and one entry to skip.

### 3.6 Cloning

```rust
pub enum Method { Cow, Checkout }
pub struct MethodDecision { pub method: Method, pub reason: String }   // reason is shown by `new` at progress level and by doctor

pub trait Cloner {
    /// Copy-on-write clone of a whole tree. `dst` must not exist; its parent must.
    fn clone_tree(&self, src: &Path, dst: &Path) -> Result<()>;
    /// Clone one regular file. Used inside mixed directories and by the probe.
    fn clone_file(&self, src: &Path, dst: &Path) -> Result<()>;
    fn name(&self) -> &'static str;   // "clonefile", "reflink", "fake"
}
pub fn platform_cloner() -> Box<dyn Cloner>;
/// `None` for a bare repository, which has no files to clone. Errors under
/// `CloneMode::Cow`; falls back under `Auto`. The probe it runs is private.
pub fn decide(git: &Git, mode: CloneMode, source: Option<&Path>, dest_parent: &Path,
              cloner: &dyn Cloner) -> Result<MethodDecision>;
pub fn is_sparse(git: &Git, source: &Path) -> bool;
pub fn nearest_existing(path: &Path) -> Option<&Path>;
/// Of the nearest existing ancestor, so a data root nobody has created yet
/// still reports the volume it will be made in.
pub fn device_of(path: &Path) -> Option<u64>;

/// Everything the recursion holds still, so the recursive step takes only
/// the path that changes. `dst_root` exists and holds nothing but `.git`.
pub struct Walker<'a> { cloner: &'a dyn Cloner, set: &'a ExcludeSet, ui: &'a Ui,
                        src_root: &'a Path, dst_root: &'a Path, stats: WalkStats }
impl Walker<'_> {
    pub fn run(&mut self) -> Result<WalkStats>;
}
pub struct WalkStats { pub tree_clones: u64, pub dirs_recursed: u64 }
```

`apfs.rs` implements `clone_tree` as one `clonefile` with `CLONE_NOFOLLOW`
and `clone_file` the same way. `reflink.rs` implements `clone_tree` as a
recursive walk calling `clone_file` (open, `FICLONE`, `fchmod`,
`futimens`) and creating directories and symlinks. `fake.rs` copies with
`std::fs`, so the walk can be tested on a filesystem that cannot clone.
`WalkStats` already counts what a test would want from a recorder.

### 3.7 Index

```rust
pub enum HashAlgo { Sha1, Sha256 }   // HashAlgo::of(&head_oid); sets oid and checksum length
pub struct Entry { pub offset: usize, pub mode: u32, pub flags: u16, pub path: Vec<u8> }

/// Pure. v2, v3, v4; None for anything not understood completely.
pub fn entries(bytes: &[u8], algo: HashAlgo) -> Option<Vec<Entry>>;
/// Fills the stat data `read-tree` left zeroed, from lstat of the clone, for
/// every entry whose source file has not changed since `since`, with the lstat
/// calls spread over threads; writes via index.lock and rename.
/// Ok(None): not understood, nothing written.
pub fn fill_stat(index: &Path, dest: &Path, source: &Path, algo: HashAlgo, since: SystemTime)
    -> Result<Option<usize>>;
```

### 3.8 Creation, removal, reaping

```rust
/// What was asked for, with configuration folded in. Nothing downstream of
/// `derive` reads a setting.
pub struct Request { pub dest: PathBuf, pub branch: String, pub base_spec: String,
                     pub workers: usize, pub fetch: bool, pub clone_mode: CloneMode }

/// What git and the filesystem say about a `Request`. Asked in one place.
pub struct Observed { pub source_head: Option<Oid>, pub base: Option<Oid>,
                      pub branch: BranchState, pub in_progress: Option<&'static str>,
                      pub dest_exists: bool }

/// The three cases of DESIGN.md 4.1. As an enum rather than a flag beside an
/// optional path, "absent but checked out somewhere" cannot be expressed.
pub enum BranchState { Absent, Free, CheckedOut(PathBuf) }

/// What will be done. Nothing optional, so acting needs no unwrapping.
pub struct Plan { pub source_head: Oid, pub branch: BranchAction }
pub enum BranchAction { Create { base: Oid }, Reuse }

/// Per-invocation decisions that are not configuration (DESIGN.md 3).
pub struct Options { pub branch: Option<String>, pub no_init: bool, pub clone_mode: CloneMode }

pub fn derive(ws: &Workspace, config: &Config, name: WorktreeName,
              options: &Options) -> Result<Request>;                          // pure
pub fn observe(git: &Git, ws: &Workspace, req: &Request) -> Result<Observed>;
pub fn check(req: &Request, obs: &Observed) -> Result<Plan>;                  // pure
pub fn run(git: &Git, ui: &Ui, ws: &Workspace, config: &Config, name: WorktreeName,
           options: &Options) -> Result<()>;

/// Undo list for a failed creation. Each step that makes something pushes a
/// closure; `disarm()` on success. Drop runs the closures in reverse.
struct Rollback { steps: Vec<Box<dyn FnOnce()>>, armed: bool }

pub struct Options { pub force: bool, pub wait: bool,
                     pub delete_branch: bool, pub force_delete_branch: bool }
pub fn remove(git: &Git, ui: &Ui, ws: &Workspace, name: &WorktreeName,
              options: &Options) -> Result<i32>;
```

Deletion lives in `reaper.rs`, with the sweep that is its heaviest user.
`remove` calls it for `--wait` and when the rename into the trash fails.

```rust
/// Paths it could not remove, empty on success. Not a `Result`: a tree that
/// half resists is neither a failure nor a success until someone counts.
/// A path that is already gone is success, which a resumed sweep relies on.
pub fn delete_tree_sync(path: &Path) -> Vec<PathBuf>;

/// What a sweep did. `skipped` counts entries that were another sweeper's,
/// whether it held the lock or had already unlinked them.
pub struct SweepStats { pub deleted: usize, pub skipped: usize, pub failed: Vec<PathBuf> }

pub fn spawn_detached_reaper(trash: &Path) -> Result<()>;   // no-op under WTM_NO_REAPER
pub fn detach_self() -> Result<()>;   // setsid, fd redirection, priorities; called by `gc --detach`
pub fn sweep(trash_dirs: &[PathBuf], ui: &Ui) -> SweepStats;
```

### 3.9 Hook

Resolution itself is not here. `config::load` joins each layer's relative
path to the base that layer implies, so `Config.init` arrives absolute and
the rule sits on the same lines as the layers it governs. What remains is
looking at the file and running it.

```rust
/// What resolving the init hook came to. `--no-init` and an absent default
/// both give `Skip`. The `Origin` is what separates an absent default from
/// an absent file someone configured.
pub enum Hook { Skip, Run(PathBuf), Unusable { path: PathBuf, origin: Origin, reason: &'static str } }
impl Hook {
    /// The hook to run, or `None`. The refusal lives here so `wtm new` and
    /// `wtm init` cannot word it differently.

    pub fn path(&self) -> Result<Option<&Path>>;
}
pub fn inspect(path: PathBuf, origin: Origin) -> Hook;   // the one look at the filesystem

/// Exported as `WTM_HOOK_*`. The prefix keeps these apart from `WTM_<KEY>`,
/// which wtm reads as configuration. All eight are always set; one that does
/// not apply is empty.
pub struct HookEnv { pub root, name, branch, base_ref, base_sha, main, repo_id, method }
pub fn run(path: &Path, env: &HookEnv, ui: &Ui) -> Result<()>;
```

There is no `InitStatus`. Resolution is a precondition, so by the time
anything calls `run` the only outcomes left are success and
`Error::HookFailed`. The skipped case never reaches it.

### 3.10 Errors

```rust
#[derive(thiserror::Error, Debug)]
pub enum Error {
    Usage(String),                          // 2
    Git { args: Vec<String>, status: i32, stderr: String },   // 1
    InProgress(&'static str),               // 1, names the operation
    Dirty(PathBuf),                         // 4
    HookFailed { code: i32, worktree: PathBuf, name: String },  // 3
    CloneUnsupported { reason: String },    // 1, only with mode = cow
    BranchCheckedOut { branch: String, at: PathBuf },   // 1
    Io { path: PathBuf, source: std::io::Error },       // 1
    Config { file: PathBuf, message: String },          // 1
    ...
}
impl Error { pub fn exit_code(&self) -> i32; }
```

## 4. Control flow of `wtm new` (for orientation)

Four steps, of which only the second and fourth touch the outside world.

```
run -> Git::new -> Repo::discover -> config::load -> Workspace::new
  -> create::run

     derive   (pure)  args + config      -> Request { dest, branch, base_spec, workers, fetch, fast_index }
     fetch    (io)    only when asked, before observing, so the base is fresh
     observe  (io)    every git and filesystem question, asked once
                      -> Observed { source_head, base, branch: BranchState, in_progress, dest_exists }
     check    (pure)  the preconditions of DESIGN.md 5
                      -> Plan { source_head, branch: BranchAction::{Create{base}, Reuse} }
     act      (io)    git worktree add --no-checkout --detach
                      -> clone::decide -> if Cow: ExcludeSet::compute -> Walker::run
                                          -> git read-tree HEAD
                                          -> empty submodules
                                          -> index::fill_stat, unless WTM_NO_FAST_INDEX
                                          -> git update-index --refresh, only if not filled
                                          -> git reset --hard
                         else:            git checkout --detach with workers
                      -> git checkout -b
                      -> hook::resolve -> hook::run
                      -> on error: undo
  -> ui.emit(path) -> exit code from InitStatus
```

`check` is pure so that the rule set, which grows in every later feature,
stays a table test. It is advisory: git enforces the same rules when it runs,
and between `observe` and `act` another process may invalidate any of them.

## 5. Things an implementer must not do

- Add a second code path that creates worktrees (an MCP tool, a
  `--worktree` flag elsewhere). Everything routes through `create::create`.
- Match `.gitignore` patterns in Rust. Ask git.
- Use `std::fs::rename` fallbacks that copy. On `EXDEV` do the synchronous
  delete.
- Write test fixtures outside `target/tmp`. It is not about volumes on
  macOS, where `/private/tmp` clones from `$HOME` fine; it is that
  `cargo clean` should take the fixtures with it.
- Read config from git config, or add a setting for something a caller
  decides per invocation. A setting also needs a scope: if a repository
  should not be able to impose it, it does not belong in the project layer.
- Introduce a state file, a registry or a cache. Everything is derived
  (`DESIGN.md` 2.2); a new fact needs a new derivation.
- Build git's metadata path by joining the worktree name (`ARCHITECTURE.md`
  3.2): git renames on collision.
- Hold `Ui` or `Config` in statics, or reintroduce a context object that
  every function takes and each function uses a third of.
- Let `Config` reach past `derive` into the acting code. Configuration is
  resolved once, into `Request`; nothing downstream reads a setting.
