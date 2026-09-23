use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::git::{Git, GitWorktree, Oid};
use crate::name::WorktreeName;
use crate::workspace::Workspace;

/// Identifies a repository by its main worktree.
///
/// Format: `<basename>-<8 hex>`, the hex being the first 8 characters of
/// sha256 over the canonicalized absolute path of the main worktree, for
/// example `monorepo-3f9a1c2e`. The basename keeps the data directory
/// readable; the hash separates two repositories with the same name.
///
/// The mapping is one-way. Recover the repository behind a `<repo-id>`
/// directory with `repo_of_dir`, which reads the `.git` file of a worktree
/// inside it, never by reversing the hash. Moving or renaming a repository
/// therefore yields a new id. Its old worktrees become orphans, which
/// `wtm ls --all` reports and `wtm gc` can remove.
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

pub struct Repo {
    /// The main worktree, canonicalized. Never modified by `wtm`.
    ///
    /// For a bare repository this is the repository directory itself. A
    /// bare clone with linked worktrees is a supported layout, and git
    /// reports it in the same position.
    pub main: PathBuf,
    /// The real `.git` directory, shared by every worktree of the repository.
    pub common_dir: PathBuf,
    pub id: RepoId,
    /// No working tree of its own, so there is nothing to clone from and
    /// creation must fall back to checkout.
    pub bare: bool,
}

impl Repo {
    /// Works from the main worktree, from a linked worktree, or from any
    /// subdirectory of either.
    pub fn discover(git: &Git, from: &Path) -> Result<Repo> {
        let common_dir = git
            .stdout(
                from,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .map_err(|_| Error::NotARepo(from.to_path_buf()))?;

        // The first record of `worktree list` is always the main worktree,
        // which is not otherwise derivable, because a repository may keep
        // its git directory somewhere else entirely.
        let first = git
            .worktrees(from)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotARepo(from.to_path_buf()))?;
        let main = std::fs::canonicalize(&first.path).map_err(|e| Error::io(&first.path, e))?;

        Ok(Repo {
            id: RepoId::for_main_worktree(&main),
            common_dir: PathBuf::from(common_dir),
            bare: first.bare,
            main,
        })
    }

    pub fn worktrees(&self, git: &Git) -> Result<Vec<GitWorktree>> {
        git.worktrees(&self.main)
    }

    /// The ref `base = "origin/HEAD"` means, and the target `ls` measures a
    /// worktree against. Falls back to the main worktree's own branch so a
    /// repository with no remote still works.
    pub fn default_branch(&self, git: &Git) -> Option<String> {
        let symref = git.stdout(&self.main, &["rev-parse", "--abbrev-ref", "origin/HEAD"]);
        if let Ok(branch) = symref
            && !branch.is_empty()
        {
            return Some(branch);
        }
        for candidate in ["origin/main", "origin/master"] {
            if git.succeeds(&self.main, &["rev-parse", "--verify", "--quiet", candidate]) {
                return Some(candidate.to_string());
            }
        }
        git.stdout(&self.main, &["symbolic-ref", "--short", "HEAD"])
            .ok()
    }
}

/// What a worktree branched from. Derived rather than remembered, so it is
/// still available to `wtm init` long after the base that was asked for.
pub fn base_of(git: &Git, repo: &Repo, worktree: &Path) -> Option<Oid> {
    let head = git.rev_parse(worktree, "HEAD").ok()?;
    let default_branch = repo.default_branch(git)?;
    git.merge_base(&repo.main, head.as_str(), &default_branch)
}

/// Everything `wtm ls` shows, derived at call time. No field of it is read
/// from stored state.
pub struct WorktreeView {
    pub git: GitWorktree,
    pub name: WorktreeName,
    pub created: Option<SystemTime>,
    pub base: Option<Oid>,
}

/// The worktrees of a workspace, with the derived fields added. Costs a
/// subprocess per worktree; `Repo::worktrees` is the cheap call for code that
/// only needs to find one by name.
pub fn view(git: &Git, workspace: &Workspace) -> Result<Vec<WorktreeView>> {
    let repo = &workspace.repo;
    let default_branch = repo.default_branch(git);
    let mut views = Vec::new();

    for worktree in repo.worktrees(git)? {
        let Some(name) = workspace.name_of(&worktree.path) else {
            continue;
        };
        let created = created_at(&worktree.path);
        let base = match (&worktree.head, &default_branch) {
            (Some(head), Some(branch)) => git.merge_base(&repo.main, head.as_str(), branch),
            _ => None,
        };
        views.push(WorktreeView {
            git: worktree,
            name,
            created,
            base,
        });
    }

    views.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(views)
}

/// When a worktree was created: the birth time of its directory, which git
/// makes at `worktree add`.
///
/// Where the filesystem has no birth time this falls back to the
/// modification time, which moves whenever anything is written at the top
/// level of the worktree, so the age is unreliable there. Every filesystem
/// wtm targets has birth times.
pub fn created_at(worktree: &Path) -> Option<SystemTime> {
    let metadata = std::fs::metadata(worktree).ok()?;
    metadata.created().or_else(|_| metadata.modified()).ok()
}
