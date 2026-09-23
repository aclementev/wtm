use std::path::Path;

use crate::error::{Error, Result};
use crate::git;
use crate::name::WorktreeName;
use crate::repo::Repo;
use crate::trash;
use crate::ui::Ui;

pub struct Options {
    pub force: bool,
    pub wait: bool,
    pub delete_branch: bool,
    pub force_delete_branch: bool,
}

/// `wtm rm`: refuse what `--force` would override, take the tree away (see
/// `trash::discard`), let git forget it, and delete the branch if asked.
pub fn remove(ui: &Ui, repo: &Repo, name: &WorktreeName, options: &Options) -> Result<i32> {
    let worktree = repo.find(ui, name)?;
    let path = worktree.path.clone();

    refuse_if_inside(&path)?;
    if !options.force {
        if worktree.locked {
            return Err(Error::Locked(path));
        }
        if !git::status_is_clean(&path)? {
            return Err(Error::Dirty(path));
        }
    }

    let path_arg = path.to_string_lossy().into_owned();
    // `git worktree prune` leaves a locked worktree registered however long
    // its directory has been gone, so the lock has to go before the rename
    // does. Only --force reaches this, having already promised as much.
    if worktree.locked {
        ui.relay(&git::run(&repo.main, &["worktree", "unlock", &path_arg])?);
    }

    trash::discard(ui, &path, &repo.trash(), name.as_str(), options.wait)?;
    ui.relay(&git::run(&repo.main, &["worktree", "prune"])?);
    repo.prune_empty_parents(&path);

    let Some(flag) = branch_flag(options) else {
        return Ok(0);
    };
    let Some(branch) = worktree.branch.as_deref() else {
        ui.warn("the worktree had no branch to delete");
        return Ok(0);
    };
    match git::run(&repo.main, &["branch", flag, branch]) {
        Ok(output) => {
            ui.relay(&output);
            Ok(0)
        }
        // The worktree is gone either way; only the branch deletion failed, so
        // the message is git's own and the exit code says not everything ran.
        Err(error) => {
            ui.warn(error.to_string());
            Ok(1)
        }
    }
}

fn branch_flag(options: &Options) -> Option<&'static str> {
    match (options.force_delete_branch, options.delete_branch) {
        (true, _) => Some("-D"),
        (_, true) => Some("-d"),
        _ => None,
    }
}

/// Removing the directory the caller is standing in would leave their shell
/// in a path that no longer exists, so `rm` refuses instead.
fn refuse_if_inside(path: &Path) -> Result<()> {
    let Ok(cwd) = std::env::current_dir() else {
        return Ok(());
    };
    if cwd.starts_with(path) {
        return Err(Error::usage(format!(
            "{} is the current directory; cd out of it first",
            path.display()
        )));
    }
    Ok(())
}
