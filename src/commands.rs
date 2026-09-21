use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;

use serde_json::json;

use crate::cli::CloneMode;
use crate::clone;
use crate::config::Config;
use crate::exclude;
use crate::error::{Error, Result};
use crate::git::Git;
use crate::hook::{self, HookEnv};
use crate::name::WorktreeName;
use crate::reaper;
use crate::repo::{self, WorktreeView};
use crate::ui::Ui;
use crate::workspace::Workspace;

pub fn ls(git: &Git, ui: &Ui, workspace: &Workspace, json: bool) -> Result<i32> {
    let now = SystemTime::now();
    let views = repo::view(git, workspace)?;

    if json {
        let entries: Vec<_> = views
            .iter()
            .map(|v| {
                json!({
                    "name": v.name.as_str(),
                    "branch": v.git.branch_short(),
                    "base": v.base.as_ref().map(|b| b.as_str()),
                    "head": v.git.head.as_ref().map(|h| h.as_str()),
                    "created": v.created.and_then(unix_seconds),
                    "path": v.git.path,
                    "status": status_of(&v.git),
                })
            })
            .collect();
        ui.emit(serde_json::to_string_pretty(&entries).unwrap_or_default());
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = views.iter().map(|v| row_of(v, now)).collect();
    for line in align(&rows) {
        ui.emit(line);
    }
    Ok(0)
}

fn row_of(view: &WorktreeView, now: SystemTime) -> Vec<String> {
    vec![
        view.name.to_string(),
        view.git.branch_short().unwrap_or("(detached)").to_string(),
        view.base.as_ref().map_or("-".to_string(), |b| b.short().to_string()),
        age(view.created, now),
        status_of(&view.git).unwrap_or_default().to_string(),
        view.git.path.display().to_string(),
    ]
}

/// Anything about a worktree that is not the normal case. `None` for a
/// healthy one, which `align` then drops as an empty column.
fn status_of(worktree: &crate::git::GitWorktree) -> Option<&'static str> {
    match worktree {
        w if w.prunable => Some("missing"),
        w if w.locked => Some("locked"),
        _ => None,
    }
}

/// Columns padded to the width of the rows actually being printed, separated
/// by two spaces. No header, because the output is one line per worktree
/// and anything piping this into `awk` would have to strip it.
fn align(rows: &[Vec<String>]) -> Vec<String> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|i| rows.iter().filter_map(|r| r.get(i)).map(|c| c.chars().count()).max().unwrap_or(0))
        .collect();

    rows.iter()
        .map(|row| {
            let cells = row
                .iter()
                .enumerate()
                .filter(|(i, _)| widths[*i] > 0)
                .map(|(i, cell)| format!("{cell:<width$}", width = widths[i]))
                .collect::<Vec<_>>();
            cells.join("  ").trim_end().to_string()
        })
        .collect()
}

/// At most two units, the smaller one dropped when it is zero.
fn age(created: Option<SystemTime>, now: SystemTime) -> String {
    let Some(created) = created else {
        return "-".to_string();
    };
    let Ok(elapsed) = now.duration_since(created) else {
        return "<1m".to_string();
    };
    let minutes = elapsed.as_secs() / 60;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    let (days, hours) = (hours / 24, hours % 24);

    match (days, hours, minutes) {
        (0, 0, 0) => "<1m".to_string(),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

fn unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

pub fn cd(ui: &Ui, workspace: &Workspace, name: Option<&str>) -> Result<i32> {
    let path = match name {
        None => workspace.repo.main.clone(),
        Some(name) => {
            let path = workspace.dir(&WorktreeName::from_str(name)?);
            if !path.is_dir() {
                return Err(Error::usage(format!("no worktree named {name}")));
            }
            path
        }
    };
    ui.emit(path.display().to_string());
    Ok(0)
}

/// Reruns the init hook, every value derived from the worktree itself since
/// nothing about the original creation was recorded. `WTM_HOOK_METHOD` is the
/// one thing that cannot be recovered that way, so it goes out empty.
pub fn init(
    git: &Git,
    ui: &Ui,
    workspace: &Workspace,
    config: &Config,
    name: Option<&str>,
) -> Result<i32> {
    let name = match name {
        Some(name) => WorktreeName::from_str(name)?,
        None => current_worktree(git, workspace)?,
    };
    let root = workspace.dir(&name);
    if !root.is_dir() {
        return Err(Error::usage(format!("no worktree named {name}")));
    }

    let inspected = hook::inspect(config.init.value.clone(), config.init.origin.clone());
    let Some(path) = inspected.path()? else {
        ui.warn(format!(
            "no init hook at {}; nothing to run",
            config.init.value.display()
        ));
        return Ok(0);
    };

    let repo = &workspace.repo;
    let branch = repo
        .worktrees(git)?
        .into_iter()
        .find(|w| w.path == root)
        .and_then(|w| w.branch_short().map(str::to_string));
    let env = HookEnv {
        name: name.to_string(),
        branch: branch.unwrap_or_default(),
        base_ref: config.base.value.clone(),
        base_sha: repo::base_of(git, repo, &root)
            .map(|oid| oid.to_string())
            .unwrap_or_default(),
        main: repo.main.clone(),
        repo_id: repo.id.to_string(),
        method: String::new(),
        root,
    };
    hook::run(path, &env, ui)?;
    Ok(0)
}

/// The wtm worktree the caller is standing in. The main worktree does not
/// count. wtm did not create it, and a hook is written for one it did.
fn current_worktree(git: &Git, workspace: &Workspace) -> Result<WorktreeName> {
    let cwd = std::env::current_dir().map_err(|e| Error::io("current directory", e))?;
    let toplevel = git
        .toplevel(&cwd)
        .and_then(|path| std::fs::canonicalize(&path).map_err(|e| Error::io(path, e)))?;
    workspace.name_of(&toplevel).ok_or_else(|| {
        Error::usage("not inside a wtm worktree; name one with `wtm init <name>`")
    })
}

/// Empties every trash under the data root and tidies what removal left
/// behind. Without `--wait` the sweeping is handed to detached reapers, one
/// per trash, which partition the entries between them through the locks.
///
/// `git worktree prune` runs for the current repository only. Pruning every
/// repository under the root means resolving a `<repo-id>` directory back to
/// its repository, which arrives with `wtm ls --all`.
pub fn gc(git: &Git, ui: &Ui, workspace: &Workspace, wait: bool) -> Result<i32> {
    let trashes = trash_dirs(workspace.root());

    let failed = if wait {
        let stats = reaper::sweep(&trashes, ui);
        ui.progress(format!(
            "swept {} entries, {} left to another sweep",
            stats.deleted, stats.skipped
        ));
        stats.failed
    } else {
        for trash in &trashes {
            reaper::spawn_detached_reaper(trash)?;
        }
        Vec::new()
    };

    ui.relay(&git.run(&workspace.repo.main, &["worktree", "prune"])?);
    prune_empty_dirs(workspace.root());

    if failed.is_empty() {
        Ok(0)
    } else {
        Err(Error::Undeleted {
            root: workspace.root().to_path_buf(),
        })
    }
}

/// Every `<repo-id>/.trash` directly under the data root. Nothing deeper is
/// looked at, so a `.trash` anywhere else is never a sweep target.
fn trash_dirs(root: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path().join(".trash"))
        .filter(|trash| trash.is_dir())
        .collect()
}

/// `remove_dir` succeeds only on an empty directory, which is the whole test
/// for whether one of these is finished with. A repository directory holding
/// worktrees, or a trash still being swept, simply refuses.
fn prune_empty_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let _ = std::fs::remove_dir(entry.path().join(".trash"));
        let _ = std::fs::remove_dir(entry.path());
    }
}

/// The include file is read from the main worktree, so a copy in a linked
/// worktree has no effect. Naming the path in effect is how someone finds
/// that out.
fn describe_include(include: Option<(std::path::PathBuf, usize)>) -> String {
    match include {
        None => "none; the repository is bare".to_string(),
        Some((file, _)) if !file.exists() => format!("none; no {}", file.display()),
        Some((file, matches)) => format!("{} matches {matches} paths", file.display()),
    }
}

pub fn config(ui: &Ui, config: &Config, json: bool) -> Result<i32> {
    let entries = config.entries();
    if json {
        let object: serde_json::Map<_, _> = entries
            .into_iter()
            .map(|(key, value, origin)| (key.to_string(), json!({ "value": value, "origin": origin })))
            .collect();
        ui.emit(serde_json::to_string_pretty(&object).unwrap_or_default());
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = entries
        .into_iter()
        .map(|(key, value, origin)| vec![key.to_string(), value, format!("({origin})")])
        .collect();
    for line in align(&rows) {
        ui.emit(line);
    }
    Ok(0)
}

/// Reports what `wtm new` would do here and why. Every line either changes
/// the outcome or explains one that did. A machine that cannot clone is a
/// supported machine, so the exit code stays 0; only a question we could
/// not ask is a failure.
pub fn doctor(git: &Git, ui: &Ui, workspace: &Workspace, json: bool) -> Result<i32> {
    let repo = &workspace.repo;
    let root = workspace.root();
    let repo_device = clone::device_of(&repo.main);
    let root_device = clone::device_of(root);
    let same_filesystem = match (repo_device, root_device) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };

    let source = (!repo.bare).then_some(repo.main.as_path());
    // The repository's directory under the data root does not exist until
    // the first worktree is made, and probing must not create it, because
    // an empty one reads as orphaned. Its nearest existing ancestor is on
    // the same filesystem, which is all the probe needs.
    let repo_dir = workspace.repo_dir();
    let probe_dir = clone::nearest_existing(&repo_dir).unwrap_or(root);
    let cloner = clone::platform_cloner();
    let decision = clone::decide(git, CloneMode::Auto, source, &probe_dir, cloner.as_ref())?;
    let sparse = source.is_some_and(|source| clone::is_sparse(git, source));
    let submodules = source.map_or(0, |source| git.gitlinks(source).map_or(0, |list| list.len()));
    let include = source.map(|source| {
        let file = exclude::include_file(source);
        let matches = exclude::included_paths(git, source).map_or(0, |paths| paths.len());
        (file, matches)
    });

    if json {
        ui.emit(
            serde_json::to_string_pretty(&json!({
                "git_version": git.version().raw,
                "repo": { "main": repo.main, "common_dir": repo.common_dir,
                          "id": repo.id.as_str(), "device": repo_device,
                          "bare": repo.bare },
                "data_root": { "path": root, "device": root_device },
                "same_filesystem": same_filesystem,
                "method": { "method": decision.method.as_str(), "reason": decision.reason },
                "sparse": sparse,
                "submodules": submodules,
                "include": include.as_ref().map(|(file, matches)| json!({
                    "file": file, "exists": file.exists(), "matches": matches,
                })),
            }))
            .unwrap_or_default(),
        );
        return Ok(0);
    }

    let rows = vec![
        vec!["git".into(), git.version().raw.clone()],
        vec!["repo".into(), repo.main.display().to_string()],
        vec!["repo id".into(), repo.id.to_string()],
        vec!["git dir".into(), repo.common_dir.display().to_string()],
        vec!["data root".into(), root.display().to_string()],
        vec![
            "volumes".into(),
            describe_volumes(repo_device, root_device, same_filesystem),
        ],
        vec![
            "working tree".into(),
            if repo.bare {
                "none; the repository is bare, so creation always checks out".into()
            } else {
                repo.main.display().to_string()
            },
        ],
        vec!["sparse".into(), if sparse { "yes" } else { "no" }.into()],
        vec!["submodules".into(), submodules.to_string()],
        vec!["include".into(), describe_include(include)],
        vec!["method".into(), decision.reason],
    ];
    for line in align(&rows) {
        ui.emit(line);
    }
    Ok(0)
}

fn describe_volumes(repo: Option<u64>, root: Option<u64>, same: Option<bool>) -> String {
    match (repo, root, same) {
        (Some(a), Some(_), Some(true)) => format!("repo and data root share device {a}"),
        (Some(a), Some(b), Some(false)) => {
            format!("repo is on device {a}, data root on {b}; cloning between them cannot work")
        }
        _ => "unknown; the data root does not exist yet".to_string(),
    }
}


