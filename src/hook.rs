use std::ffi::OsStr;
use std::io::IsTerminal;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::Origin;
use crate::error::{Error, Result};
use crate::git::SCRUBBED;
use crate::ui::Ui;

/// What resolving the init hook came to. Any path here is absolute.
///
/// `--no-init` and an absent default `wtm-init.sh` both give `Skip`. Neither
/// is worth reporting, and nothing downstream needs to tell them apart. An
/// absent hook that someone did configure is `Unusable` instead.
pub enum Hook {
    Skip,
    Run(PathBuf),
    Unusable {
        path: PathBuf,
        origin: Origin,
        reason: &'static str,
    },
}

impl Hook {
    /// The refusal lives here so that `new`, which asks among its
    /// preconditions, and `init`, which asks before anything else, cannot
    /// word it differently.
    pub fn path(&self) -> Result<Option<&Path>> {
        match self {
            Hook::Skip => Ok(None),
            Hook::Run(path) => Ok(Some(path)),
            Hook::Unusable {
                path,
                origin,
                reason,
            } => Err(Error::usage(format!(
                "init hook {} {reason} ({origin})",
                path.display()
            ))),
        }
    }
}

/// Looks the hook up on disk, apart from running it, so that `new` can refuse
/// one it could never run before it creates anything.
pub fn inspect(path: PathBuf, origin: Origin) -> Hook {
    let unusable = |reason| Hook::Unusable {
        path: path.clone(),
        origin: origin.clone(),
        reason,
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return match origin {
            Origin::Default => Hook::Skip,
            _ => unusable("does not exist"),
        };
    };
    if !metadata.is_file() {
        return unusable("is not a file");
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return unusable("is not executable");
    }
    Hook::Run(path)
}

/// The `WTM_HOOK_*` environment a hook is given.
///
/// The prefix keeps these apart from `WTM_<KEY>`, which wtm reads as
/// configuration. Under one namespace, a hook that starts a long-lived
/// process would hand this worktree's base to every later wtm run under it.
///
/// Every field is exported on every run, so a hook may use `set -u`. A field
/// that does not apply is empty rather than missing: `method` after a rerun,
/// `base_sha` for a branch that already existed.
pub struct HookEnv {
    pub root: PathBuf,
    pub name: String,
    pub branch: String,
    pub base_ref: String,
    pub base_sha: String,
    pub main: PathBuf,
    pub repo_id: String,
    pub method: String,
}

impl HookEnv {
    /// `OsStr` so that a path which is not UTF-8 reaches the hook as it is.
    fn vars(&self) -> [(&'static str, &OsStr); 8] {
        [
            ("WTM_HOOK_ROOT", self.root.as_os_str()),
            ("WTM_HOOK_NAME", self.name.as_ref()),
            ("WTM_HOOK_BRANCH", self.branch.as_ref()),
            ("WTM_HOOK_BASE_REF", self.base_ref.as_ref()),
            ("WTM_HOOK_BASE_SHA", self.base_sha.as_ref()),
            ("WTM_HOOK_MAIN", self.main.as_os_str()),
            ("WTM_HOOK_REPO_ID", self.repo_id.as_ref()),
            ("WTM_HOOK_METHOD", self.method.as_ref()),
        ]
    }
}

/// Runs the hook in the worktree `env` describes. Nothing it writes reaches
/// stdout, which carries the worktree path alone.
pub fn run(path: &Path, env: &HookEnv, ui: &Ui) -> Result<()> {
    let mut command = Command::new(path);
    command.current_dir(&env.root);

    // A hook almost certainly runs git, and an inherited GIT_DIR would aim it
    // at the caller's repository instead of the worktree it was handed.
    for key in SCRUBBED {
        command.env_remove(key);
    }
    for (key, value) in env.vars() {
        command.env(key, value);
    }

    // A hook may prompt a person, but must never block an agent or a CI job
    // on a read that nobody will answer.
    command.stdin(if std::io::stdin().is_terminal() {
        Stdio::inherit()
    } else {
        Stdio::null()
    });

    if ui.quiet() {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        // Inheriting rather than piping keeps the hook on a terminal whenever
        // wtm is, so its colours and the order of its two streams survive.
        let stderr = std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| Error::io(path, e))?;
        command.stdout(Stdio::from(stderr)).stderr(Stdio::inherit());
    }

    let status = command.status().map_err(|e| Error::io(path, e))?;
    if status.success() {
        return Ok(());
    }
    Err(Error::HookFailed {
        code: status.code().unwrap_or(-1),
        worktree: env.root.clone(),
        name: env.name.clone(),
    })
}
