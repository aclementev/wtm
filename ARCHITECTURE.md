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
- **Errors carry the exit code.** One error enum, `Error::exit_code()`,
  and `main` is the only place that prints an error and exits.
- **No global state in the process.** No statics, no `lazy_static` config;
  a `Context` value is built in `main` and passed down.

## 2. Module map

```
src/
  main.rs         clap definitions, dispatch, exit-code mapping. Help and skill prose (docs.rs data) live next to the flags.
  cli.rs          clap structs: `Cli`, `Command`, per-command arg structs. No logic.
  context.rs      `Context { config, ui, git, layout, clock }` built once from cli + env + files
  config.rs       `Config`, `Setting<T>`, `Origin`, loading and precedence
  ui.rs           `Ui`: progress/warn/error to stderr, `emit` to stdout, `--quiet`, `--json`
  error.rs        `Error` enum (thiserror), `exit_code()`
  git.rs          `Git` subprocess wrapper; typed helpers for the handful of commands wtm uses
  repo.rs         `Repo` discovery from cwd/--repo; `RepoId`
  name.rs         `WorktreeName` newtype and validation
  layout.rs       `Layout`: paths under a data root; pure functions of (root, repo id, name)
  exclude.rs      `ExcludeSet` computation via git; `PathTrie` and `Class`
  clone/
    mod.rs        `Cloner` trait, `Walker`, `Method`, `MethodDecision`, probe
    apfs.rs       clonefile(2) implementation
    reflink.rs    FICLONE per-file implementation
    fake.rs       test double (plain copy, records calls)
  index/
    mod.rs        `Index`, `Entry`, `StatData`, `Extension`, `HashAlgo`
    parse.rs      bytes -> Index (v2, v3, v4)
    write.rs      Index -> bytes (same version as source, checksum)
    stat.rs       `rewrite_stat(&mut Index, root, now)`; racy smudge
  hook.rs         init hook resolution and execution; `HookEnv`
  create.rs       `wtm new` orchestration; `Rollback` guard
  remove.rs       `wtm rm`; rename to trash; synchronous delete
  reaper.rs       detach (setsid, fds, priorities) and `sweep(roots)` with flock
  shell.rs        wrapper text per shell
  docs.rs         skill markdown generated from the clap command tree plus prose
  commands/
    ls.rs  gc.rs  doctor.rs  config_cmd.rs  init.rs  cd.rs   thin: build inputs, call modules, format output
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
    pub main: PathBuf,        // canonical main worktree
    pub common_dir: PathBuf,  // .git of the main worktree
    pub id: RepoId,
}
impl Repo {
    pub fn discover(git: &Git, from: &Path) -> Result<Repo>;   // works from inside any worktree
    pub fn worktrees(&self, git: &Git) -> Result<Vec<GitWorktree>>;   // `git worktree list --porcelain`
    /// Git's metadata directory for a worktree. Git derives its name from the
    /// basename of the worktree path and appends a digit on collision, so it is
    /// NOT `<common_dir>/worktrees/<name>`. Always read back from git.
    pub fn gitdir_for(&self, git: &Git, path: &Path) -> Result<PathBuf>;
}

pub struct GitWorktree { pub path: PathBuf, pub head: Oid, pub branch: Option<String>, pub detached: bool, pub locked: bool, pub prunable: bool }

/// Everything `wtm ls` shows, derived per DESIGN.md 2.2: no stored metadata.
pub struct WorktreeView { pub git: GitWorktree, pub name: WorktreeName, pub created: Option<SystemTime>, pub base: Option<Oid>, pub missing: bool }
pub fn view(git: &Git, repo: &Repo, layout: &Layout) -> Result<Vec<WorktreeView>>;
/// Creation time: birth time of git's metadata dir for the worktree (mtime fallback).
pub fn created_at(gitdir: &Path) -> Option<SystemTime>;
/// Which repo a `<repo-id>` directory belongs to: read the `.git` file of any
/// worktree inside it. Returns None for an orphaned or empty directory.
pub fn repo_of_dir(git: &Git, repo_dir: &Path) -> Result<Option<PathBuf>>;

pub struct Git { exe: PathBuf, version: GitVersion }
impl Git {
    pub fn new() -> Result<Git>;                     // finds git, checks >= 2.31
    /// Runs git with GIT_DIR, GIT_WORK_TREE, GIT_INDEX_FILE, GIT_COMMON_DIR,
    /// GIT_OBJECT_DIRECTORY removed from the environment.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Result<Output>;
    pub fn run_z(&self, cwd: &Path, args: &[&str]) -> Result<Vec<Vec<u8>>>;   // NUL-separated stdout
    pub fn rev_parse(&self, cwd: &Path, rev: &str) -> Result<Oid>;
    pub fn status_is_clean(&self, cwd: &Path) -> Result<bool>;
    pub fn in_progress_operation(&self, gitdir: &Path) -> Option<&'static str>;  // "rebase", "merge", ...
}
```

### 3.3 Configuration

```rust
pub enum Origin { Flag, Env(String), Project(PathBuf), Global(PathBuf), Default }
pub struct Setting<T> { pub value: T, pub origin: Origin }

pub struct Config {
    pub dir: Setting<PathBuf>,
    pub init: Setting<PathBuf>,
    pub base: Setting<String>,
    pub branch_prefix: Setting<String>,
    pub fetch: Setting<bool>,
    pub include: Setting<PathBuf>,
    pub clone_mode: Setting<CloneMode>,
    pub clone_workers: Setting<usize>,
    pub rm_wait: Setting<bool>,
}
pub enum CloneMode { Auto, Cow, Checkout }

/// Layers are merged key by key; `flags` is whatever the command parsed.
pub fn load(flags: &FlagOverrides, env: &dyn Fn(&str) -> Option<String>,
            project_file: Option<&Path>, global_file: Option<&Path>) -> Result<Config>;
```

`RawConfig` (serde, all fields `Option`) is the file shape; unknown keys
are rejected with `deny_unknown_fields`.

### 3.4 Layout

```rust
pub struct Layout { root: PathBuf }
impl Layout {
    pub fn repo_dir(&self, id: &RepoId) -> PathBuf;
    pub fn worktree_dir(&self, id: &RepoId, name: &WorktreeName) -> PathBuf;
    pub fn trash_dir(&self, id: &RepoId) -> PathBuf;
}
```

`wtm` persists nothing else. There is no `state.rs`, no registry, no lock
file: see `DESIGN.md` 2.2 for the table of what each fact is derived from.
An implementer who feels the need for a state file should add a derivation
instead, or raise it as a design change.

### 3.5 Exclusion

```rust
pub enum Class { Clean, Excluded, Included, Mixed }

/// Result of the three `git ls-files -o` queries in the source.
pub struct ExcludeSet { trie: PathTrie }
impl ExcludeSet {
    pub fn compute(git: &Git, source: &Path, include_file: &Path) -> Result<ExcludeSet>;
    pub fn from_lists(excluded: Vec<PathBuf>, included: Vec<PathBuf>) -> ExcludeSet;   // pure; tested with proptest
    /// Clean: no excluded/mixed descendant, clone whole. Excluded: skip.
    /// Included: clone whole even though it is untracked. Mixed: recurse.
    pub fn classify(&self, rel: &Path) -> Class;
}
```

Invariants of `from_lists`: an included path wins over an excluded
ancestor (the ancestor becomes `Mixed`); every ancestor of a `Mixed` or
`Included` path is `Mixed`; a path with no relation to any listed path is
`Clean`; the root is `Mixed` if anything is excluded, `Clean` otherwise.

### 3.6 Cloning

```rust
pub enum Method { Cow, Checkout }
pub struct MethodDecision { pub method: Method, pub reason: String }   // reason is shown in warnings and doctor

pub trait Cloner {
    /// Copy-on-write clone of a whole tree. `dst` must not exist; its parent must.
    fn clone_tree(&self, src: &Path, dst: &Path) -> Result<()>;
    /// Clone one regular file. Used inside mixed directories and by the probe.
    fn clone_file(&self, src: &Path, dst: &Path) -> Result<()>;
    fn name(&self) -> &'static str;   // "clonefile", "reflink", "fake"
}
pub fn platform_cloner() -> Box<dyn Cloner>;
pub fn probe(cloner: &dyn Cloner, source: &Path, dest_parent: &Path) -> Result<ProbeResult>;
pub fn decide(mode: CloneMode, source: &Path, dest_parent: &Path, cloner: &dyn Cloner) -> Result<MethodDecision>;

pub struct Walker<'a> { cloner: &'a dyn Cloner, set: &'a ExcludeSet, stats: WalkStats }
impl Walker<'_> {
    /// Clones `src` into the existing, empty-but-for-.git `dst`.
    pub fn run(&mut self, src: &Path, dst: &Path) -> Result<WalkStats>;
}
pub struct WalkStats { pub tree_clones: u64, pub file_clones: u64, pub dirs_recursed: u64, pub skipped: u64 }
```

`apfs.rs` implements `clone_tree` as one `clonefile` with `CLONE_NOFOLLOW`
and `clone_file` the same way. `reflink.rs` implements `clone_tree` as a
recursive walk calling `clone_file` (open, `FICLONE`, `fchmod`,
`futimens`) and creating directories and symlinks. `fake.rs` copies with
`std::fs` and records every call, for tests on any filesystem.

### 3.7 Index

```rust
pub enum HashAlgo { Sha1, Sha256 }   // from extensions.objectFormat; sets oid and checksum length
pub struct StatData { pub ctime: (u32, u32), pub mtime: (u32, u32), pub dev: u32, pub ino: u32, pub uid: u32, pub gid: u32, pub size: u32 }
pub struct Entry { pub stat: StatData, pub mode: u32, pub oid: Vec<u8>, pub flags: u16, pub ext_flags: Option<u16>, pub path: Vec<u8> }
impl Entry { pub fn stage(&self) -> u8; pub fn skip_worktree(&self) -> bool; pub fn intent_to_add(&self) -> bool; pub fn is_gitlink(&self) -> bool; }
pub enum Extension { Tree(Vec<u8>), Reuc(Vec<u8>), Untr, Fsmn, Eoie, Ieot, Link(Vec<u8>), Sdir, Other([u8; 4], Vec<u8>) }
pub struct Index { pub version: u32, pub algo: HashAlgo, pub entries: Vec<Entry>, pub extensions: Vec<Extension> }

pub fn parse(bytes: &[u8], algo: HashAlgo) -> Result<Index>;          // v2, v3, v4; verifies checksum
pub fn write(index: &Index) -> Vec<u8>;                                // same version; drops Untr/Fsmn/Eoie/Ieot/Sdir; recomputes checksum
pub fn has_split_index(index: &Index) -> bool;
/// Rewrites stat data from lstat of each entry under `root`; smudges racy entries
/// (mtime seconds >= `now`); leaves gitlinks, skip-worktree and intent-to-add alone;
/// zeroes stat for missing files and returns their paths.
pub fn rewrite_stat(index: &mut Index, root: &Path, now: SystemTime) -> Result<Vec<PathBuf>>;
pub fn install(index: &Index, gitdir: &Path) -> Result<()>;            // temp file, fsync, rename
```

### 3.8 Creation, removal, reaping

```rust
pub struct CreateRequest { pub name: WorktreeName, pub branch: String, pub base: String, pub source: PathBuf, pub run_init: bool, pub fetch: bool }
pub struct Created { pub path: PathBuf, pub method: Method, pub init: InitStatus }
pub fn create(ctx: &Context, repo: &Repo, req: CreateRequest) -> Result<Created>;

/// Undo list for a failed creation. Each step that makes something pushes a
/// closure; `disarm()` on success. Drop runs the closures in reverse.
struct Rollback { steps: Vec<Box<dyn FnOnce()>>, armed: bool }

pub struct RemoveRequest { pub name: WorktreeName, pub force: bool, pub wait: bool, pub delete_branch: Option<BranchDelete> }
pub enum BranchDelete { IfMerged, Force }
pub fn remove(ctx: &Context, repo: &Repo, req: RemoveRequest) -> Result<()>;
pub fn delete_tree_sync(path: &Path) -> Result<Vec<PathBuf>>;   // returns paths it could not remove

pub fn spawn_detached_reaper(exe: &Path, trash: &Path) -> Result<()>;
pub fn detach_self() -> Result<()>;           // setsid, fd redirection, priorities; called by `gc --reap --detach`
pub fn sweep(trash_dirs: &[PathBuf], ui: &Ui) -> Result<SweepStats>;
```

### 3.9 Hook

```rust
pub struct HookEnv { pub root, name, branch, base, base_sha, source, main, repo_id, method }
/// Outcome of the hook. Returned to the caller, reported immediately by
/// `ui` and the exit code, and never written to disk (DESIGN.md 2.2).
pub enum InitStatus { Ok, Failed { exit_code: i32 }, Skipped }
pub enum HookResolution { File(PathBuf), DefaultMissing, ConfiguredMissing(PathBuf, Origin) }
pub fn resolve(config: &Config, source: &Path) -> HookResolution;
pub fn run(path: &Path, cwd: &Path, env: &HookEnv, ui: &Ui) -> Result<InitStatus>;
```

### 3.10 Errors

```rust
#[derive(thiserror::Error, Debug)]
pub enum Error {
    Usage(String),                          // 2
    Git { args: Vec<String>, status: i32, stderr: String },   // 1
    InProgress(&'static str),               // 1, names the operation
    Dirty(PathBuf),                         // 4
    HookFailed { path: PathBuf, code: i32 },// 3
    CloneUnsupported { reason: String },    // 1, only with mode = cow
    BranchCheckedOut { branch: String, at: PathBuf },   // 1
    Io { path: PathBuf, source: std::io::Error },       // 1
    Config { file: PathBuf, message: String },          // 1
    Index(String),                          // 1
    ...
}
impl Error { pub fn exit_code(&self) -> i32; }
```

## 4. Control flow of `wtm new` (for orientation)

```
main -> cli parse -> Context::build -> Repo::discover
  -> config::load -> WorktreeName::from_str -> resolve base and branch
  -> preflight (in-progress op, sparse, dest exists, branch rules)
  -> clone::decide -> git worktree add --no-checkout --detach
  -> Rollback armed
  -> if Cow: ExcludeSet::compute -> Walker::run -> index::{parse, rewrite_stat, install} -> empty submodules -> git reset --hard
     else:   git checkout --detach with workers
  -> git checkout -b
  -> hook::resolve -> hook::run
  -> Rollback::disarm -> ui.emit(path) -> exit code from InitStatus
```

## 5. Things an implementer must not do

- Add a second code path that creates worktrees (an MCP tool, a
  `--worktree` flag elsewhere). Everything routes through `create::create`.
- Match `.gitignore` patterns in Rust. Ask git.
- Use `std::fs::rename` fallbacks that copy. On `EXDEV` do the synchronous
  delete.
- Use `/tmp` in tests on macOS. It is a different volume from `$HOME`.
- Read config from git config. Three sources are enough.
- Introduce a state file, a registry or a cache. Everything is derived
  (`DESIGN.md` 2.2); a new fact needs a new derivation.
- Build git's metadata path by joining the worktree name (`ARCHITECTURE.md`
  3.2): git renames on collision.
- Hold `Ui` or `Config` in statics.
