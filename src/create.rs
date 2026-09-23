use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cli::CloneMode;
use crate::clone;
use crate::config::{Config, Origin};
use crate::error::{Error, Result};
use crate::git::{self, Oid};
use crate::hook::{self, HookEnv};
use crate::name::WorktreeName;
use crate::repo::Repo;
use crate::ui::Ui;

/// `wtm new`: check everything that can be checked cheaply, make the
/// worktree, print its path, run the hook.
///
/// The checks run in a fixed order, cheapest and least surprising first, so
/// a hook or branch name that could never work costs no clone. Git enforces
/// most of them again when it runs, and another process may change any of
/// them in between. They exist for a precise message before work starts.
pub fn run(
    ui: &Ui,
    repo: &Repo,
    config: &Config,
    name: WorktreeName,
    branch: Option<&str>,
    no_init: bool,
    mode: CloneMode,
) -> Result<()> {
    let main = &repo.main;
    let dest = repo.dir(&name);
    let branch = format!(
        "{}{}",
        config.branch_prefix.value,
        branch.unwrap_or(name.as_str())
    );

    let hook = match no_init {
        true => None,
        false => hook::resolve(&config.init)?,
    };
    if !git::succeeds(main, &["check-ref-format", &format!("refs/heads/{branch}")]) {
        return Err(Error::usage(format!(
            "{branch} is not a valid branch name; pass --branch to choose another"
        )));
    }

    // Before anything reads a ref, so the base resolves against fresh ones.
    if config.fetch.value {
        git::stream(main, &["fetch"], ui.quiet())?;
    }

    let source_head = git::rev_parse(main, "HEAD")
        .map_err(|_| Error::usage("the main worktree has no commit to branch from"))?;
    if let Some(operation) = git::gitdir_of(main).and_then(|dir| git::in_progress_operation(&dir)) {
        return Err(Error::InProgress(operation));
    }
    if let Some(path) = occupied(repo, &name, &dest)? {
        return Err(Error::usage(format!(
            "{} already exists; choose another name",
            path.display()
        )));
    }

    // `None` means the branch exists and is checked out as it is.
    let base = match branch_state(main, &branch)? {
        BranchState::CheckedOut(at) => return Err(Error::BranchCheckedOut { branch, at }),
        // A default base is only a default, but a `--base` someone typed is
        // a request this cannot honour, and a warning would scroll past.
        BranchState::Free if config.base.origin == Origin::Flag => {
            return Err(Error::usage(format!(
                "branch {branch} already exists; drop --base to check it out as it is, \
                 or choose another name"
            )));
        }
        BranchState::Free => None,
        BranchState::Absent => Some(resolve_base(repo, &config.base.value).ok_or_else(|| {
            Error::usage(format!(
                "cannot resolve {} as a commit; pass --base",
                config.base.value
            ))
        })?),
    };

    let started = Instant::now();
    let verb = match make(ui, repo, &dest, &branch, &source_head, base.as_ref(), mode) {
        Ok(verb) => verb,
        Err(error) => {
            undo(ui, repo, &dest);
            return Err(error);
        }
    };
    ui.progress(format!(
        "{verb} in {:.1} s",
        started.elapsed().as_secs_f64()
    ));
    ui.emit(dest.display().to_string());

    // After the path is printed and outside `undo`, because a hook that
    // fails still leaves a usable worktree behind.
    let Some(hook) = hook else {
        return Ok(());
    };
    let base_sha = base.or_else(|| repo.base_of(&branch));
    let env = HookEnv {
        root: dest,
        name: name.to_string(),
        branch,
        base_sha: base_sha.map(|oid| oid.to_string()).unwrap_or_default(),
        main: main.clone(),
    };
    hook::run(&hook, &env, ui)
}

/// The three states of the branch a new worktree asks for. An enum rather
/// than a flag beside an optional path, so that "absent but checked out
/// somewhere" cannot be expressed.
enum BranchState {
    Absent,
    Free,
    CheckedOut(PathBuf),
}

/// Checked out anywhere means in any worktree of the repository, the main
/// one and those wtm did not make included.
fn branch_state(main: &Path, branch: &str) -> Result<BranchState> {
    if !git::branch_exists(main, branch) {
        return Ok(BranchState::Absent);
    }
    let holder = git::worktrees(main)?
        .into_iter()
        .find(|w| w.branch_short() == Some(branch));
    Ok(match holder {
        Some(worktree) => BranchState::CheckedOut(worktree.path),
        None => BranchState::Free,
    })
}

/// A worktree of ours already by this name, or anything at all at the
/// destination. The two differ once the repository has moved: its id
/// changed with its path, so the worktrees made before the move sit under
/// the old id's directory and not at `dest`.
fn occupied(repo: &Repo, name: &WorktreeName, dest: &Path) -> Result<Option<PathBuf>> {
    let ours = repo.worktrees()?.into_iter().find(|w| w.name == *name);
    Ok(ours
        .map(|w| w.path)
        .or_else(|| dest.exists().then(|| dest.to_path_buf())))
}

/// `origin/HEAD` means "the remote default branch" and has to be resolved;
/// any other value is a ref passed to git unchanged.
fn resolve_base(repo: &Repo, spec: &str) -> Option<Oid> {
    let reference = match spec {
        "origin/HEAD" => repo.default_branch()?,
        other => other.to_string(),
    };
    git::rev_parse(&repo.main, &reference).ok()
}

/// Registers the worktree and fills it, by clone or by checkout. Returns
/// what happened, in words for the progress line. `base` is `None` when an
/// existing branch is checked out as it is.
fn make(
    ui: &Ui,
    repo: &Repo,
    dest: &Path,
    branch: &str,
    source_head: &Oid,
    base: Option<&Oid>,
    mode: CloneMode,
) -> Result<&'static str> {
    let main = &repo.main;
    let parent = repo.repo_dir();
    std::fs::create_dir_all(&parent).map_err(|e| Error::io(&parent, e))?;
    let cow = use_clone(ui, mode, (!repo.bare).then_some(main.as_path()), &parent)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    // Git refuses a non-empty destination, so the worktree is registered
    // before any file is written into it. A clone starts at the source's
    // commit, which is what its files are. A checkout starts where it will
    // end, so git writes every file once.
    let start = match (cow, base) {
        (true, _) => source_head.as_str(),
        (false, Some(base)) => base.as_str(),
        (false, None) => branch,
    };
    let dest_arg = dest.to_string_lossy();
    git::run(
        main,
        &[
            "worktree",
            "add",
            "-q",
            "--no-checkout",
            "--detach",
            &dest_arg,
            start,
        ],
    )?;
    unsparse(dest)?;

    if cow {
        clone::populate(ui, main, dest)?;
        // Git now rewrites only the files that differ between the source's
        // commit and the base.
        let target = match base {
            Some(base) => vec!["-b", branch, base.as_str()],
            None => vec![branch],
        };
        checkout(ui, dest, &target, false)?;
        return Ok("cloned");
    }

    // HEAD is already where the worktree ends, so the full checkout names no
    // branch, and creating one afterwards writes nothing. That keeps it the
    // last step that can fail, after the include copy, which can refuse.
    let target = match base {
        Some(_) => vec![],
        None => vec![branch],
    };
    checkout(ui, dest, &target, true)?;
    if !repo.bare {
        clone::copy_included(main, dest)?;
    }
    if base.is_some() {
        checkout(ui, dest, &["-b", branch], false)?;
    }
    Ok("checked out")
}

/// Whether to clone, reporting why not when the answer is no. A filesystem
/// that cannot clone is ordinary, so the reason is progress, not a warning.
fn use_clone(ui: &Ui, mode: CloneMode, source: Option<&Path>, parent: &Path) -> Result<bool> {
    if let CloneMode::Checkout = mode {
        return Ok(false);
    }
    let Some(reason) = clone::unavailable(source, parent) else {
        return Ok(true);
    };
    if let CloneMode::Cow = mode {
        return Err(Error::CloneUnsupported { reason });
    }
    ui.progress(format!("checking out: {reason}"));
    Ok(false)
}

/// Git copies the sparse-checkout patterns of the worktree it runs in into
/// every worktree it adds, and a new worktree has every tracked file.
/// Without its pattern file git treats a worktree as full whatever
/// `core.sparseCheckout` says, and the file is the new worktree's own, so
/// removing it touches nothing the main worktree reads.
fn unsparse(dest: &Path) -> Result<()> {
    let Some(gitdir) = git::gitdir_of(dest) else {
        return Ok(());
    };
    let patterns = gitdir.join("info/sparse-checkout");
    match std::fs::remove_file(&patterns) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Error::io(&patterns, e)),
        _ => Ok(()),
    }
}

/// Runs `git checkout` in the new worktree with `target` as its arguments.
/// `fill` is for a worktree that holds no files yet, where this is the
/// checkout that writes all of them.
fn checkout(ui: &Ui, dest: &Path, target: &[&str], fill: bool) -> Result<()> {
    // Git's own default is one worker; passing the core count is what makes
    // a full checkout parallel at all.
    let workers = format!(
        "checkout.workers={}",
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
    let mut args = vec![
        "-c",
        &workers,
        "-c",
        "checkout.thresholdForParallelism=100",
        "checkout",
        "-q",
    ];
    // Git shows progress on a terminal unless told `-q`, and `-q` also
    // silences the chatter, so progress is asked for explicitly and only
    // where a person is watching.
    if fill && std::io::stderr().is_terminal() {
        args.push("--progress");
    }
    if fill {
        args.push("-f");
    }
    args.extend(target);
    git::stream(dest, &args, ui.quiet())
}

/// Removes what a failed creation left behind, so an immediate retry works.
/// We report failures here, but they must not mask the original error.
///
/// There is no branch to delete. Creating it is the last step that can
/// fail, and a `checkout -b` that fails creates no branch.
fn undo(ui: &Ui, repo: &Repo, dest: &Path) {
    if dest.exists() {
        let arg = dest.to_string_lossy();
        let removed = git::run(&repo.main, &["worktree", "remove", "--force", &arg]).is_ok();
        if !removed && std::fs::remove_dir_all(dest).is_err() {
            ui.warn(format!("could not remove {arg}; remove it by hand"));
        }
    }
    let _ = git::run(&repo.main, &["worktree", "prune"]);
    repo.prune_empty_parents(dest);
}
