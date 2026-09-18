use std::path::Path;

use crate::error::{Error, Result};
use crate::git::Git;
use crate::name::WorktreeName;
use crate::ui::Ui;
use crate::workspace::Workspace;

pub struct Options {
    pub force: bool,
    pub delete_branch: bool,
    pub force_delete_branch: bool,
}

pub fn remove(
    git: &Git,
    ui: &Ui,
    workspace: &Workspace,
    name: &WorktreeName,
    options: &Options,
) -> Result<i32> {
    let path = workspace.dir(name);
    let worktree = workspace
        .repo
        .worktrees(git)?
        .into_iter()
        .find(|w| w.path == path)
        .ok_or_else(|| Error::usage(format!("no worktree named {name}")))?;

    refuse_if_inside(&path)?;
    if !options.force {
        if worktree.locked {
            return Err(Error::Locked(path));
        }
        if !git.status_is_clean(&path)? {
            return Err(Error::Dirty(path));
        }
    }

    let path_arg = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if options.force {
        args.push("--force");
        // Git wants the flag twice before it will remove a locked worktree,
        // and `--force` here has already promised that it will.
        if worktree.locked {
            args.push("--force");
        }
    }
    args.push(&path_arg);
    ui.relay(&git.run(&workspace.repo.main, &args)?);
    prune_empty_parents(&path, &workspace.repo_dir());

    let Some(flag) = branch_flag(options) else {
        return Ok(0);
    };
    let Some(branch) = worktree.branch_short() else {
        ui.warn("the worktree had no branch to delete");
        return Ok(0);
    };
    match git.run(&workspace.repo.main, &["branch", flag, branch]) {
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

/// A name like `feat/login` leaves an empty `feat` behind once the worktree is
/// gone. `remove_dir` only succeeds on an empty directory, which is exactly
/// the condition for pruning one.
fn prune_empty_parents(path: &Path, stop: &Path) {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == stop || std::fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

/// Removing the directory the caller is standing in leaves their shell in a
/// path that no longer exists, so it is refused rather than surprising them.
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
