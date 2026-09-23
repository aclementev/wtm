use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::git::{self, Oid};
use crate::name::WorktreeName;
use crate::ui::Ui;

const TRASH: &str = ".trash";

/// Identifies a repository by its main worktree.
///
/// Format: `<basename>-<8 hex>`, the hex being the first 8 characters of
/// sha256 over the canonicalized absolute path of the main worktree, for
/// example `monorepo-3f9a1c2e`. The basename keeps the data directory
/// readable; the hash separates two repositories with the same name.
///
/// The id only decides where new worktrees go. Moving or renaming a
/// repository yields a new one, and its existing worktrees stay where they
/// are: which worktrees are ours is decided by `Repo::name_of`, which
/// accepts any repository directory under the data root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoId(String);

impl RepoId {
    pub fn for_main_worktree(canonical: &Path) -> RepoId {
        let basename = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".to_string());
        let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
        RepoId(format!("{basename}-{:.8}", hex(&digest)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The main worktree of the repository containing `from`, canonicalized,
/// and whether the repository is bare. Works from the main worktree, from a
/// linked worktree, or from any subdirectory of either.
///
/// Separate from `Repo::new` because the project configuration, which says
/// where the data root is, lives in the main worktree.
pub fn main_worktree(from: &Path) -> Result<(PathBuf, bool)> {
    if !git::succeeds(from, &["rev-parse", "--git-dir"]) {
        return Err(Error::NotARepo(from.to_path_buf()));
    }
    // The first record of `worktree list` is always the main worktree,
    // which is not otherwise derivable, because a repository may keep its
    // git directory somewhere else entirely.
    let first = git::worktrees(from)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotARepo(from.to_path_buf()))?;
    let main = std::fs::canonicalize(&first.path).map_err(|e| Error::io(&first.path, e))?;
    Ok((main, first.bare))
}

/// A repository, and where its worktrees live under the data root.
///
/// Invariant: `name_of` is the inverse of `dir`. That pair is how `wtm`
/// names its worktrees without keeping a registry, so both live here rather
/// than as path joins anyone may reimplement.
pub struct Repo {
    /// The main worktree, canonicalized. Never modified by `wtm`.
    ///
    /// For a bare repository this is the repository directory itself. A
    /// bare clone with linked worktrees is a supported layout, and git
    /// reports it in the same position.
    pub main: PathBuf,
    pub id: RepoId,
    /// No working tree of its own, so there is nothing to clone from and
    /// creation must fall back to checkout.
    pub bare: bool,
    root: PathBuf,
}

/// A worktree `wtm` made, as git reports it, with the name it was given.
pub struct Worktree {
    pub name: WorktreeName,
    pub path: PathBuf,
    pub head: Option<Oid>,
    /// Without `refs/heads/`, and `None` when detached.
    pub branch: Option<String>,
    /// `git worktree lock`: someone asked that nothing remove this.
    pub locked: bool,
    /// Git's own judgement that the worktree is gone: its directory has
    /// vanished, or its gitdir pointer is broken.
    pub prunable: bool,
}

impl Repo {
    /// `root` is canonicalized here: git reports worktree paths with
    /// symlinks resolved, and `name_of` prefix-matches the root against them.
    pub fn new(main: PathBuf, bare: bool, root: &Path) -> Repo {
        Repo {
            id: RepoId::for_main_worktree(&main),
            main,
            bare,
            root: canonical_root(root),
        }
    }

    /// The data root, shared by every repository. Only `gc` and `doctor`
    /// look past this repository's own directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repo_dir(&self) -> PathBuf {
        self.root.join(self.id.as_str())
    }

    pub fn dir(&self, name: &WorktreeName) -> PathBuf {
        self.repo_dir().join(name.as_path())
    }

    pub fn trash(&self) -> PathBuf {
        self.repo_dir().join(TRASH)
    }

    /// Every repository's trash under the data root, this one's included.
    /// Nothing deeper is looked at, so a `.trash` anywhere else is never
    /// swept.
    pub fn all_trashes(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path().join(TRASH))
            .filter(|trash| trash.is_dir())
            .collect()
    }

    /// The name of a worktree of this repository, or `None` when wtm did not
    /// make it. Ours means under the data root, in any repository's
    /// directory, not only the one `repo_dir` names today.
    ///
    /// `path` comes from this repository's own `git worktree list`, so
    /// anything under the root is this repository's whatever directory it
    /// sits in. And the directory does change: the id is computed from where
    /// the repository is, so moving it gives a new one, and the worktrees
    /// made before the move stay under the old.
    pub fn name_of(&self, path: &Path) -> Option<WorktreeName> {
        let mut parts = path.strip_prefix(&self.root).ok()?.components();
        parts.next()?;
        let name = parts.as_path();
        if name.starts_with(TRASH) {
            return None;
        }
        WorktreeName::from_str(name.to_str()?).ok()
    }

    /// The worktrees of this repository that wtm made, sorted by name.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let mut ours: Vec<Worktree> = git::worktrees(&self.main)?
            .into_iter()
            .filter_map(|w| {
                Some(Worktree {
                    name: self.name_of(&w.path)?,
                    branch: w.branch_short().map(str::to_string),
                    path: w.path,
                    head: w.head,
                    locked: w.locked,
                    prunable: w.prunable,
                })
            })
            .collect();
        ours.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ours)
    }

    /// The worktree called `name`, with its link back to the repository
    /// repaired first.
    ///
    /// Moving a repository breaks that link: the worktree's `.git` file still
    /// names the old path, and every git command inside it fails until `git
    /// worktree repair` runs. It rewrites only what is broken, so running it
    /// every time costs one subprocess and heals a move without anyone asking.
    pub fn find(&self, ui: &Ui, name: &WorktreeName) -> Result<Worktree> {
        let worktree = self
            .worktrees()?
            .into_iter()
            .find(|w| w.name == *name)
            .ok_or_else(|| Error::usage(format!("no worktree named {name}")))?;
        let path = worktree.path.to_string_lossy();
        // A worktree whose directory is gone has nothing to repair, and git
        // says so with an error that `rm --force` must not trip over.
        if let Ok(output) = git::run(&self.main, &["worktree", "repair", &path]) {
            ui.relay(&output);
        }
        Ok(worktree)
    }

    /// The ref `base = "origin/HEAD"` means, and the target a worktree's base
    /// is measured against. Falls back to the main worktree's own branch so a
    /// repository with no remote still works.
    pub fn default_branch(&self) -> Option<String> {
        let symref = git::stdout(&self.main, &["rev-parse", "--abbrev-ref", "origin/HEAD"]);
        if let Ok(branch) = symref
            && !branch.is_empty()
        {
            return Some(branch);
        }
        for candidate in ["origin/main", "origin/master"] {
            if git::succeeds(&self.main, &["rev-parse", "--verify", "--quiet", candidate]) {
                return Some(candidate.to_string());
            }
        }
        git::stdout(&self.main, &["symbolic-ref", "--short", "HEAD"]).ok()
    }

    /// What `rev` branched from: its merge-base with the default branch.
    /// Derived rather than remembered, so it is still available to
    /// `wtm init` long after the base that was asked for.
    pub fn base_of(&self, rev: &str) -> Option<Oid> {
        git::merge_base(&self.main, rev, &self.default_branch()?)
    }

    /// Removes the directories between `path` and the data root that are
    /// left empty once the worktree at `path` is gone: `feat` for a name
    /// like `feat/login`, and the repository's own directory, which would
    /// otherwise sit there empty. `remove_dir` only succeeds on an empty
    /// directory, which is exactly the condition for removing one.
    pub fn prune_empty_parents(&self, path: &Path) {
        for dir in path.ancestors().skip(1) {
            if dir == self.root || std::fs::remove_dir(dir).is_err() {
                break;
            }
        }
    }
}

/// The root with symlinks resolved and `..` removed, as git reports worktree
/// paths. The part that does not exist yet is appended unchanged.
fn canonical_root(root: &Path) -> PathBuf {
    let mut suffix = PathBuf::new();
    let mut candidate = root;
    loop {
        if let Ok(real) = std::fs::canonicalize(candidate) {
            return if suffix.as_os_str().is_empty() {
                real
            } else {
                real.join(suffix)
            };
        }
        let (Some(parent), Some(name)) = (candidate.parent(), candidate.file_name()) else {
            return root.to_path_buf();
        };
        suffix = Path::new(name).join(suffix);
        candidate = parent;
    }
}
