use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::git::Git;
use crate::name::WorktreeName;
use crate::reaper;
use crate::repo;
use crate::ui::Ui;
use crate::workspace::Workspace;

pub struct Options {
    pub force: bool,
    pub wait: bool,
    pub delete_branch: bool,
    pub force_delete_branch: bool,
}

/// Removes a worktree by renaming it into the trash, so the command returns
/// as soon as the path is gone rather than after the last file is unlinked.
/// A detached reaper collects the bytes afterwards, and nothing waits on it.
pub fn remove(
    git: &Git,
    ui: &Ui,
    workspace: &Workspace,
    name: &WorktreeName,
    options: &Options,
) -> Result<i32> {
    let worktree = repo::find(git, ui, workspace, name)?;
    let path = worktree.path.clone();

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
    // `git worktree prune` leaves a locked worktree registered however long
    // its directory has been gone, so the lock has to go before the rename
    // does. Only --force reaches this, having already promised as much.
    if worktree.locked {
        ui.relay(&git.run(&workspace.repo.main, &["worktree", "unlock", &path_arg])?);
    }

    // Whether anything was left for a sweep, which is the only reason to
    // spend a process on one.
    let trashed = match options.wait {
        true => {
            report(ui, &path, reaper::delete_tree_sync(&path))?;
            false
        }
        false => match move_to_trash(&path, name, &workspace.trash()) {
            Ok(()) => true,
            Err(error) => {
                ui.warn(format!("{}; deleting it in place instead", why(&error)));
                report(ui, &path, reaper::delete_tree_sync(&path))?;
                false
            }
        },
    };

    ui.relay(&git.run(&workspace.repo.main, &["worktree", "prune"])?);
    workspace.prune_empty_parents(&path);

    if trashed {
        reaper::spawn_detached_reaper(&workspace.trash())?;
    }

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

/// Renames the worktree under the trash, where a sweep will unlink it.
///
/// Fails rather than falling back: a trash on another filesystem gives
/// `EXDEV` here, and the caller deletes in place instead. This is why the
/// rename is `std::fs::rename`, which is `rename(2)` and nothing else, and
/// never `mv` or a helper that would silently copy the tree across.
fn move_to_trash(path: &Path, name: &WorktreeName, trash: &Path) -> io::Result<()> {
    std::fs::create_dir_all(trash)?;
    // Bounded rather than a retry until it works: every attempt reads a
    // clock, and a clock that never moves would otherwise hang `wtm rm`.
    let mut last = None;
    for _ in 0..16 {
        let entry = trash.join(entry_name(name));
        match std::fs::rename(path, &entry) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no trash name was free")))
}

/// Unique within one trash directory, which is all that is asked of it: the
/// rename itself refuses a collision, so the clock only has to advance.
/// Slashes become dashes so a nested name stays one directory.
fn entry_name(name: &WorktreeName) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!(
        "{}-{}-{nanos}",
        name.as_str().replace('/', "-"),
        std::process::id()
    )
}

/// Paths a synchronous delete could not remove are the user's to deal with,
/// so they are named and the command exits 1.
fn report(ui: &Ui, root: &Path, failed: Vec<PathBuf>) -> Result<()> {
    if failed.is_empty() {
        return Ok(());
    }
    for path in &failed {
        ui.warn(format!("could not remove {}", path.display()));
    }
    Err(Error::Undeleted {
        root: root.to_path_buf(),
    })
}

/// The two failures the design expects, said in words. Anything else is
/// reported as the operating system worded it.
fn why(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EXDEV) => "the trash is on another filesystem".to_string(),
        Some(libc::EBUSY) => "the worktree is in use".to_string(),
        _ => error.to_string(),
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
