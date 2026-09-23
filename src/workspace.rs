use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::name::WorktreeName;
use crate::repo::Repo;

const TRASH: &str = ".trash";

/// Where one repository's worktrees live: the repository itself, and the data
/// root holding it.
///
/// Invariant: `name_of` is the inverse of `dir`. That pair is how `wtm`
/// answers "which worktrees are ours" without keeping a registry, so they
/// live together here rather than as path joins anyone may reimplement.
pub struct Workspace {
    pub repo: Repo,
    root: PathBuf,
}

impl Workspace {
    pub fn new(repo: Repo, root: PathBuf) -> Workspace {
        Workspace { repo, root }
    }

    /// The data root, shared by every repository. Only `ls --all` and `gc`,
    /// which range over repositories, have any use for it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repo_dir(&self) -> PathBuf {
        self.root.join(self.repo.id.as_str())
    }

    pub fn dir(&self, name: &WorktreeName) -> PathBuf {
        self.repo_dir().join(name.as_path())
    }

    pub fn trash(&self) -> PathBuf {
        self.repo_dir().join(TRASH)
    }

    /// Removes the directories between `path` and the data root that are
    /// left empty once the worktree at `path` is gone: `feat` for a name
    /// like `feat/login`, and the repository's own directory, which would
    /// otherwise read as orphaned. `remove_dir` only succeeds on an empty
    /// directory, which is exactly the condition for removing one.
    pub fn prune_empty_parents(&self, path: &Path) {
        for dir in path.ancestors().skip(1) {
            if dir == self.root || std::fs::remove_dir(dir).is_err() {
                break;
            }
        }
    }

    pub fn name_of(&self, path: &Path) -> Option<WorktreeName> {
        let rel = path.strip_prefix(self.repo_dir()).ok()?;
        if rel.starts_with(TRASH) {
            return None;
        }
        WorktreeName::from_str(rel.to_str()?).ok()
    }
}

/// Git reports worktree paths with symlinks resolved and `..` removed, and
/// `name_of` prefix-matches the root against them, so the root must be in the
/// same form. The part that does not exist yet is appended unchanged.
pub fn canonical_root(root: &Path) -> PathBuf {
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
