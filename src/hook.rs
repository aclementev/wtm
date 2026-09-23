use std::ffi::OsStr;
use std::io::IsTerminal;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{Origin, Setting};
use crate::error::{Error, Result};
use crate::git::SCRUBBED;
use crate::ui::Ui;

/// The init hook to run, or `None` when there is nothing to run.
///
/// An absent default `wtm-init.sh` is the normal case and says nothing. A
/// hook someone configured that could never run is refused naming the path,
/// the reason and the layer that set it, before anything is created.
pub fn resolve(init: &Setting<PathBuf>) -> Result<Option<PathBuf>> {
    let path = &init.value;
    let reason = match std::fs::metadata(path) {
        Err(_) if init.origin == Origin::Default => return Ok(None),
        Err(_) => "does not exist",
        Ok(meta) if !meta.is_file() => "is not a file",
        Ok(meta) if meta.permissions().mode() & 0o111 == 0 => "is not executable",
        Ok(_) => return Ok(Some(path.clone())),
    };
    Err(Error::usage(format!(
        "init hook {} {reason} ({})",
        path.display(),
        init.origin
    )))
}

/// The `WTM_HOOK_*` environment a hook is given.
///
/// The prefix keeps these apart from `WTM_<KEY>`, which wtm reads as
/// configuration. Under one namespace, a hook that starts a long-lived
/// process would hand this worktree's base to every later wtm run under it.
///
/// Every field is exported on every run, so a hook may use `set -u`.
/// `base_sha` is empty when no merge-base with the default branch exists.
pub struct HookEnv {
    pub root: PathBuf,
    pub name: String,
    pub branch: String,
    pub base_sha: String,
    pub main: PathBuf,
}

impl HookEnv {
    /// `OsStr` so that a path which is not UTF-8 reaches the hook as it is.
    fn vars(&self) -> [(&'static str, &OsStr); 5] {
        [
            ("WTM_HOOK_ROOT", self.root.as_os_str()),
            ("WTM_HOOK_NAME", self.name.as_ref()),
            ("WTM_HOOK_BRANCH", self.branch.as_ref()),
            ("WTM_HOOK_BASE_SHA", self.base_sha.as_ref()),
            ("WTM_HOOK_MAIN", self.main.as_os_str()),
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
