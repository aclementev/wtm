use std::path::PathBuf;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::{Git, Oid};
use crate::name::WorktreeName;
use crate::ui::Ui;
use crate::workspace::Workspace;

/// `wtm new` in four steps: derive what was asked for, observe what git and
/// the filesystem say about it, check the preconditions, then act. Only the
/// second and fourth touch the outside world.
pub fn run(
    git: &Git,
    ui: &Ui,
    workspace: &Workspace,
    config: &Config,
    name: WorktreeName,
    branch: Option<&str>,
) -> Result<()> {
    let request = derive(workspace, config, name, branch)?;

    // Fetching before observing, so the base resolves against fresh refs.
    if request.fetch {
        ui.progress("fetching");
        ui.relay(&git.run(&workspace.repo.main, &["fetch"])?);
    }

    let observed = observe(git, workspace, &request)?;
    let plan = check(&request, &observed)?;
    create(git, ui, workspace, &request, &plan)?;

    ui.emit(request.dest.display().to_string());
    Ok(())
}

/// What the caller asked for, with configuration already folded in. Pure: no
/// part of this depends on the state of the repository.
pub struct Request {
    pub dest: PathBuf,
    pub branch: String,
    /// The base as written, still unresolved: `origin/HEAD` means "ask git".
    pub base_spec: String,
    pub workers: usize,
    pub fetch: bool,
}

/// What git and the filesystem say about a `Request`. Every question is asked
/// here and nowhere else.
pub struct Observed {
    pub source_head: Option<Oid>,
    pub base: Option<Oid>,
    pub branch: BranchState,
    pub in_progress: Option<&'static str>,
    pub dest_exists: bool,
}

/// The three cases of `DESIGN.md` 4.1. As an enum rather than a flag plus an
/// optional path, "absent but checked out somewhere" cannot be expressed.
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
}

pub enum BranchAction {
    Create { base: Oid },
    Reuse,
}

pub fn derive(
    workspace: &Workspace,
    config: &Config,
    name: WorktreeName,
    branch: Option<&str>,
) -> Result<Request> {
    Ok(Request {
        dest: workspace.dir(&name),
        branch: format!(
            "{}{}",
            config.branch_prefix.value,
            branch.unwrap_or(name.as_str())
        ),
        base_spec: config.base.value.clone(),
        // Git's own default is one worker; passing the core count is what
        // makes the fallback checkout parallel at all.
        workers: std::thread::available_parallelism().map_or(1, |n| n.get()),
        fetch: config.fetch.value,
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

/// The preconditions of `DESIGN.md` 5, in that order. Pure, so the rules can
/// be exercised without a repository on disk.
///
/// Git enforces all of this again when it runs, and between observing and
/// acting another process may change any of it. The purpose here is a precise
/// message before work starts, not safety.
pub fn check(request: &Request, observed: &Observed) -> Result<Plan> {
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
    })
}

fn create(git: &Git, ui: &Ui, ws: &Workspace, request: &Request, plan: &Plan) -> Result<()> {
    match act(git, ui, ws, request, plan) {
        Ok(()) => Ok(()),
        Err(error) => {
            undo(git, ui, ws, request, plan);
            Err(error)
        }
    }
}

fn act(git: &Git, ui: &Ui, ws: &Workspace, request: &Request, plan: &Plan) -> Result<()> {
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
                "branch {} already exists; the base is ignored",
                request.branch
            ));
            ui.relay(&git.run(&request.dest, &["checkout", "-q", &request.branch])?);
        }
    }

    Ok(())
}

/// Removes what a failed creation left behind, so an immediate retry works.
/// Failures here are reported but must not mask the original error.
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

    fn request() -> Request {
        Request {
            dest: PathBuf::from("/data/repo-1234abcd/task"),
            branch: "task".to_string(),
            base_spec: "origin/HEAD".to_string(),
            workers: 4,
            fetch: false,
        }
    }

    fn observed() -> Observed {
        Observed {
            source_head: Some(Oid::from_hex("a".repeat(40))),
            base: Some(Oid::from_hex("b".repeat(40))),
            branch: BranchState::Absent,
            in_progress: None,
            dest_exists: false,
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
    }

    /// `DESIGN.md` 5 fixes the order, so a repository that is both mid-rebase
    /// and has a stale destination reports the rebase.
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
