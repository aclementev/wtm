use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cli::CloneMode;
use crate::clone::{self, Cloner, Method, Walker};
use crate::config::{Config, Setting};
use crate::error::{Error, Result};
use crate::exclude::{self, ExcludeSet};
use crate::git::{Git, Oid};
use crate::hook::{self, Hook, HookEnv};
use crate::index::{self, HashAlgo};
use crate::name::WorktreeName;
use crate::repo;
use crate::ui::Ui;
use crate::workspace::Workspace;

/// Choices made once, for this invocation, which is why they are flags and
/// never reach `Config`.
pub struct Options {
    pub branch: Option<String>,
    pub no_init: bool,
    pub clone_mode: CloneMode,
    /// Off when `WTM_NO_FAST_INDEX` is set to any value. Git then verifies
    /// the cloned files by reading every one, which is slower but runs none
    /// of our index code. It is the way out if that code is ever suspected.
    pub fast_index: bool,
}

/// `wtm new` in four steps: derive what was asked for, observe what git and
/// the filesystem say about it, check the preconditions, then act. Only the
/// second and fourth touch the outside world.
pub fn run(
    git: &Git,
    ui: &Ui,
    workspace: &Workspace,
    config: &Config,
    name: WorktreeName,
    options: &Options,
) -> Result<()> {
    let request = derive(workspace, config, name, options)?;

    // Fetching before observing, so the base resolves against fresh refs.
    if request.fetch {
        ui.progress("fetching");
        ui.relay(&git.run(&workspace.repo.main, &["fetch"])?);
    }

    let observed = observe(git, workspace, &request)?;
    let plan = check(&request, &observed)?;
    let method = create(git, ui, workspace, &request, &plan)?;

    ui.emit(request.dest.display().to_string());

    // Outside the rollback guard and after the path is printed, because a
    // hook that fails still leaves a usable worktree behind.
    match &plan.hook {
        None => Ok(()),
        Some(path) => hook::run(path, &hook_env(git, workspace, &request, &plan, method), ui),
    }
}

/// Reusing an existing branch ignores the base and warns about it, so
/// handing that base to the hook would contradict the warning. We derive it
/// from the merge-base instead, which is also all `wtm init` ever has.
fn hook_env(git: &Git, ws: &Workspace, request: &Request, plan: &Plan, method: Method) -> HookEnv {
    let base_sha = match &plan.branch {
        BranchAction::Create { base } => Some(base.clone()),
        BranchAction::Reuse => repo::base_of(git, &ws.repo, &request.dest),
    };
    HookEnv {
        root: request.dest.clone(),
        name: request.name.to_string(),
        branch: request.branch.clone(),
        base_ref: request.base_spec.clone(),
        base_sha: base_sha.map(|oid| oid.to_string()).unwrap_or_default(),
        main: ws.repo.main.clone(),
        repo_id: ws.repo.id.to_string(),
        method: method.as_str().to_string(),
    }
}

/// What the caller asked for, with configuration already folded in. Pure.
/// No part of this depends on the state of the repository.
pub struct Request {
    pub name: WorktreeName,
    pub dest: PathBuf,
    pub branch: String,
    /// The base as written, still unresolved: `origin/HEAD` means "ask git".
    pub base_spec: String,
    pub workers: usize,
    pub fetch: bool,
    pub clone_mode: CloneMode,
    pub fast_index: bool,
    /// `None` for `--no-init`. The origin comes along because it decides
    /// whether an absent file is silent or an error.
    pub hook: Option<Setting<PathBuf>>,
}

/// What git and the filesystem say about a `Request`. Nowhere else asks
/// them.
pub struct Observed {
    pub source_head: Option<Oid>,
    pub base: Option<Oid>,
    pub branch: BranchState,
    pub in_progress: Option<&'static str>,
    pub dest_exists: bool,
    pub hook: Hook,
}

/// An enum rather than a flag beside an optional path, so that "absent but
/// checked out somewhere" cannot be expressed.
pub enum BranchState {
    Absent,
    Free,
    CheckedOut(PathBuf),
}

/// What will be done, once the preconditions hold. Nothing here is optional,
/// so acting on it needs no unwrapping.
pub struct Plan {
    pub source_head: Oid,
    pub branch: BranchAction,
    pub hook: Option<PathBuf>,
}

pub enum BranchAction {
    Create { base: Oid },
    Reuse,
}

pub fn derive(
    workspace: &Workspace,
    config: &Config,
    name: WorktreeName,
    options: &Options,
) -> Result<Request> {
    Ok(Request {
        dest: workspace.dir(&name),
        branch: format!(
            "{}{}",
            config.branch_prefix.value,
            options.branch.as_deref().unwrap_or(name.as_str())
        ),
        base_spec: config.base.value.clone(),
        // Git's own default is one worker; passing the core count is what
        // makes the fallback checkout parallel at all.
        workers: std::thread::available_parallelism().map_or(1, |n| n.get()),
        fetch: config.fetch.value,
        clone_mode: options.clone_mode,
        fast_index: options.fast_index,
        hook: (!options.no_init).then(|| config.init.clone()),
        name,
    })
}

pub fn observe(git: &Git, workspace: &Workspace, request: &Request) -> Result<Observed> {
    let main = &workspace.repo.main;
    Ok(Observed {
        source_head: git.rev_parse(main, "HEAD").ok(),
        base: resolve_base(git, workspace, &request.base_spec),
        branch: branch_state(git, workspace, &request.branch)?,
        in_progress: git
            .gitdir_of(main)
            .and_then(|gitdir| Git::in_progress_operation(&gitdir)),
        dest_exists: request.dest.exists(),
        hook: match &request.hook {
            None => Hook::Skip,
            Some(init) => hook::inspect(init.value.clone(), init.origin.clone()),
        },
    })
}

fn branch_state(git: &Git, workspace: &Workspace, branch: &str) -> Result<BranchState> {
    if !git.branch_exists(&workspace.repo.main, branch) {
        return Ok(BranchState::Absent);
    }
    let holder = workspace
        .repo
        .worktrees(git)?
        .into_iter()
        .find(|w| w.branch_short() == Some(branch));
    Ok(match holder {
        Some(worktree) => BranchState::CheckedOut(worktree.path),
        None => BranchState::Free,
    })
}

/// `origin/HEAD` means "the remote default branch" and has to be resolved;
/// any other value is a ref passed to git unchanged.
fn resolve_base(git: &Git, workspace: &Workspace, spec: &str) -> Option<Oid> {
    let reference = match spec {
        "origin/HEAD" => workspace.repo.default_branch(git)?,
        other => other.to_string(),
    };
    git.rev_parse(&workspace.repo.main, &reference).ok()
}

/// The preconditions of `wtm new`, in a fixed order. Pure, so the rules can
/// be exercised without a repository on disk.
///
/// Git enforces all of this again when it runs, and between observing and
/// acting another process may change any of it. The purpose here is a precise
/// message before work starts, not safety.
pub fn check(request: &Request, observed: &Observed) -> Result<Plan> {
    // First because it is the only rule needing nothing from git, and a hook
    // that could never run should not cost a worktree.
    let hook = observed.hook.path()?.map(Path::to_path_buf);

    let Some(source_head) = observed.source_head.clone() else {
        return Err(Error::usage("the main worktree has no commit to branch from"));
    };
    if let Some(operation) = observed.in_progress {
        return Err(Error::InProgress(operation));
    }
    if observed.dest_exists {
        return Err(Error::usage(format!(
            "{} already exists",
            request.dest.display()
        )));
    }

    let branch = match &observed.branch {
        BranchState::CheckedOut(at) => {
            return Err(Error::BranchCheckedOut {
                branch: request.branch.clone(),
                at: at.clone(),
            });
        }
        BranchState::Free => BranchAction::Reuse,
        BranchState::Absent => match observed.base.clone() {
            Some(base) => BranchAction::Create { base },
            None => {
                return Err(Error::usage(format!(
                    "cannot resolve {} as a commit; pass --base",
                    request.base_spec
                )));
            }
        },
    };

    Ok(Plan {
        source_head,
        branch,
        hook,
    })
}

/// Sequences the steps and owns the rollback. Returns how the files got
/// there, which the init hook is told.
fn create(git: &Git, ui: &Ui, ws: &Workspace, request: &Request, plan: &Plan) -> Result<Method> {
    let parent = ws.repo_dir();
    std::fs::create_dir_all(&parent).map_err(|e| Error::io(&parent, e))?;

    let cloner = clone::platform_cloner();
    // Before anything is made, so `--clone-mode cow` where cloning is
    // impossible costs nothing and leaves nothing to undo.
    let decision = clone::decide(
        git,
        request.clone_mode,
        (!ws.repo.bare).then_some(ws.repo.main.as_path()),
        &parent,
        cloner.as_ref(),
    )?;
    if decision.surprising {
        ui.warn(&decision.reason);
    } else {
        ui.progress(&decision.reason);
    }

    match act(git, ui, ws, request, plan, decision.method, cloner.as_ref()) {
        Ok(()) => Ok(decision.method),
        Err(error) => {
            undo(git, ui, ws, request, plan);
            Err(error)
        }
    }
}

fn act(
    git: &Git,
    ui: &Ui,
    ws: &Workspace,
    request: &Request,
    plan: &Plan,
    method: Method,
    cloner: &dyn Cloner,
) -> Result<()> {
    if let Some(parent) = request.dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    // Git refuses a non-empty destination, so the worktree must be registered
    // before any file is written into it.
    let dest = request.dest.to_string_lossy().into_owned();
    ui.relay(&git.run(
        &ws.repo.main,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            &dest,
            plan.source_head.as_str(),
        ],
    )?);

    match method {
        Method::Cow => populate_by_clone(
            git,
            ui,
            &ws.repo.main,
            &request.dest,
            &plan.source_head,
            request.fast_index,
            cloner,
        )?,
        Method::Checkout => {
            ui.progress("checking out");
            let workers = format!("checkout.workers={}", request.workers);
            ui.relay(&git.run(
                &request.dest,
                &[
                    "-c",
                    &workers,
                    "-c",
                    "checkout.thresholdForParallelism=100",
                    "checkout",
                    "-q",
                    "--detach",
                    plan.source_head.as_str(),
                ],
            )?);
            if !ws.repo.bare {
                copy_included(git, &ws.repo.main, &request.dest)?;
            }
        }
    }

    // Detaching at the source's HEAD first means git now rewrites only the
    // files that differ between source and base.
    match &plan.branch {
        BranchAction::Create { base } => {
            ui.relay(&git.run(
                &request.dest,
                &["checkout", "-q", "-b", &request.branch, base.as_str()],
            )?);
        }
        BranchAction::Reuse => {
            ui.warn(format!(
                "checking out the existing branch {}; --base is ignored",
                request.branch
            ));
            ui.relay(&git.run(&request.dest, &["checkout", "-q", &request.branch])?);
        }
    }

    Ok(())
}

/// Fills a worktree that holds nothing but the `.git` file git wrote.
fn populate_by_clone(
    git: &Git,
    ui: &Ui,
    source: &Path,
    dest: &Path,
    source_head: &Oid,
    fast_index: bool,
    cloner: &dyn Cloner,
) -> Result<()> {
    ui.progress("cloning");
    // Taken before the dirty query inside `compute`, so that a file changed
    // after git last looked at it is caught by the index fill below.
    let since = SystemTime::now();
    let set = ExcludeSet::compute(git, source)?;
    let stats = Walker::new(cloner, &set, ui, source, dest).run()?;
    ui.progress(format!(
        "cloned {} subtrees, recursed into {} directories",
        stats.tree_clones, stats.dirs_recursed
    ));

    ui.relay(&git.run(dest, &["read-tree", "HEAD"])?);
    empty_submodules(git, dest)?;

    // `read-tree` leaves every entry's stat data zeroed, and checkout reads
    // cached stat rather than hashing, so a `reset --hard` on top of it
    // would rewrite every file from the object store and waste the clone.
    let filled = if fast_index {
        fill_index(git, ui, source, dest, source_head, since)?
    } else {
        ui.progress("WTM_NO_FAST_INDEX is set, so git will read every file to verify the clone");
        false
    };
    // Without our stat data, the refresh pays for one hash of the tree,
    // finds the content already correct and writes the true stat back. It
    // exits non-zero when a file needs updating, which is what we asked it
    // to find out. After a fill it would find nothing: every entry left
    // zeroed is one the reset should write from the object store anyway.
    if !filled {
        let _ = git.run(dest, &["update-index", "--refresh", "-q"]);
    }
    ui.relay(&git.run(dest, &["reset", "-q", "--hard"])?);
    Ok(())
}

/// Writes the cloned files' stat data into the index so git trusts them
/// without reading them. False when the index was left alone, and git has
/// to verify the clone itself.
fn fill_index(
    git: &Git,
    ui: &Ui,
    source: &Path,
    dest: &Path,
    source_head: &Oid,
    since: SystemTime,
) -> Result<bool> {
    let index = PathBuf::from(git.stdout(
        dest,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    let filled = match HashAlgo::of(source_head) {
        Some(algo) => index::fill_stat(&index, dest, source, algo, since)?,
        None => None,
    };
    match filled {
        Some(count) => {
            ui.progress(format!("filled stat data for {count} files"));
            Ok(true)
        }
        None => {
            ui.warn(format!(
                "wtm cannot read the index format git wrote at {}, so git will read every file \
                 to verify the clone. The worktree is still correct, and a newer wtm may \
                 restore the fast path",
                index.display()
            ));
            Ok(false)
        }
    }
}

/// The checkout path's share of `.worktreeinclude`. Git wrote only tracked
/// files, so the untracked ones the include file matches are copied in from
/// the source, before the branch step as the walk would have carried them.
fn copy_included(git: &Git, source: &Path, dest: &Path) -> Result<()> {
    for rel in exclude::included_paths(git, source)? {
        let (from, to) = (source.join(&rel), dest.join(&rel));
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        // Git lists an untracked symlink as a file. Copying it would copy
        // what it points at.
        let meta = std::fs::symlink_metadata(&from).map_err(|e| Error::io(&from, e))?;
        if meta.is_symlink() {
            let target = std::fs::read_link(&from).map_err(|e| Error::io(&from, e))?;
            std::os::unix::fs::symlink(target, &to).map_err(|e| Error::io(&to, e))?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| Error::io(&to, e))?;
        }
    }
    Ok(())
}

/// `git worktree add` leaves submodule directories empty and so does
/// `wtm`. Cloning their contents would carry `.git` files pointing at the
/// source's gitdir and break every git command inside them. Filling them
/// is the init hook's job.
fn empty_submodules(git: &Git, dest: &Path) -> Result<()> {
    for submodule in git.gitlinks(dest)? {
        let path = dest.join(submodule);
        if path.exists() {
            std::fs::remove_dir_all(&path).map_err(|e| Error::io(&path, e))?;
        }
        std::fs::create_dir_all(&path).map_err(|e| Error::io(&path, e))?;
    }
    Ok(())
}

/// Removes what a failed creation left behind, so an immediate retry works.
/// We report failures here, but they must not mask the original error.
fn undo(git: &Git, ui: &Ui, ws: &Workspace, request: &Request, plan: &Plan) {
    let dest = request.dest.to_string_lossy().into_owned();
    if request.dest.exists() {
        let removed = git
            .run(&ws.repo.main, &["worktree", "remove", "--force", &dest])
            .is_ok();
        if !removed && std::fs::remove_dir_all(&request.dest).is_err() {
            ui.warn(format!("could not remove {dest}; remove it by hand"));
        }
    }
    let _ = git.run(&ws.repo.main, &["worktree", "prune"]);
    if matches!(plan.branch, BranchAction::Create { .. }) {
        let _ = git.run(&ws.repo.main, &["branch", "-D", &request.branch]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::Origin;
    use std::str::FromStr;

    fn request() -> Request {
        Request {
            name: WorktreeName::from_str("task").expect("a valid name"),
            hook: None,
            dest: PathBuf::from("/data/repo-1234abcd/task"),
            branch: "task".to_string(),
            base_spec: "origin/HEAD".to_string(),
            workers: 4,
            fetch: false,
            clone_mode: CloneMode::Auto,
            fast_index: true,
        }
    }

    fn observed() -> Observed {
        Observed {
            source_head: Some(Oid::from_hex("a".repeat(40))),
            base: Some(Oid::from_hex("b".repeat(40))),
            branch: BranchState::Absent,
            in_progress: None,
            dest_exists: false,
            hook: Hook::Skip,
        }
    }

    fn unusable_hook(reason: &'static str) -> Hook {
        Hook::Unusable {
            path: PathBuf::from("/repo/setup.sh"),
            origin: Origin::Flag,
            reason,
        }
    }

    fn message(observed: Observed) -> String {
        check(&request(), &observed)
            .err()
            .expect("the preconditions should refuse this")
            .to_string()
    }

    #[test]
    fn a_clean_request_for_a_new_branch_creates_it_at_the_base() {
        let plan = check(&request(), &observed()).expect("nothing should refuse this");

        assert_eq!(plan.source_head.as_str(), "a".repeat(40));
        match plan.branch {
            BranchAction::Create { base } => assert_eq!(base.as_str(), "b".repeat(40)),
            BranchAction::Reuse => panic!("an absent branch must be created"),
        }
    }

    #[test]
    fn an_existing_free_branch_is_reused_even_when_no_base_resolves() {
        let plan = check(
            &request(),
            &Observed {
                branch: BranchState::Free,
                base: None,
                ..observed()
            },
        )
        .expect("a free branch needs no base");

        assert!(matches!(plan.branch, BranchAction::Reuse));
    }

    #[test]
    fn each_precondition_refuses_with_its_own_message() {
        assert!(message(Observed { source_head: None, ..observed() }).contains("no commit"));
        assert!(message(Observed { in_progress: Some("rebase"), ..observed() }).contains("rebase"));
        assert!(message(Observed { dest_exists: true, ..observed() }).contains("already exists"));
        assert!(message(Observed { base: None, ..observed() }).contains("origin/HEAD"));
        assert!(
            message(Observed {
                branch: BranchState::CheckedOut(PathBuf::from("/elsewhere/task")),
                ..observed()
            })
            .contains("/elsewhere/task")
        );

        let hook = message(Observed { hook: unusable_hook("does not exist"), ..observed() });
        assert!(hook.contains("/repo/setup.sh"), "{hook}");
        assert!(hook.contains("does not exist"), "{hook}");
        assert!(hook.contains("flag"), "the layer that set it is named: {hook}");
    }

    #[test]
    fn an_unusable_hook_is_refused_before_the_repository_is_consulted() {
        let both = Observed {
            hook: unusable_hook("is not executable"),
            in_progress: Some("rebase"),
            source_head: None,
            ..observed()
        };
        assert!(message(both).contains("is not executable"));
    }

    #[test]
    fn nothing_to_run_leaves_the_plan_without_a_hook() {
        let plan = check(&request(), &observed()).expect("a skipped hook refuses nothing");
        assert!(plan.hook.is_none());
    }

    /// The order is fixed, so a repository that is both mid-rebase and has a
    /// stale destination reports the rebase.
    #[test]
    fn the_preconditions_are_reported_in_the_specified_order() {
        let both = Observed {
            in_progress: Some("rebase"),
            dest_exists: true,
            ..observed()
        };
        assert!(message(both).contains("rebase"));
    }

}
