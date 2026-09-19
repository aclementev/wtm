use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;

use serde_json::json;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::Git;
use crate::hook::{self, HookEnv};
use crate::name::WorktreeName;
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

pub fn doctor(git: &Git, ui: &Ui, workspace: &Workspace, json: bool) -> Result<i32> {
    let repo = &workspace.repo;
    let root = workspace.root();
    let repo_device = device_of(&repo.main);
    let root_device = device_of(root);
    let same_filesystem = match (repo_device, root_device) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };

    if json {
        ui.emit(
            serde_json::to_string_pretty(&json!({
                "git_version": git.version().raw,
                "repo": { "main": repo.main, "common_dir": repo.common_dir,
                          "id": repo.id.as_str(), "device": repo_device,
                          "bare": repo.bare },
                "data_root": { "path": root, "device": root_device },
                "same_filesystem": same_filesystem,
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

/// The device of the nearest existing ancestor, so a data root that has not
/// been created yet still reports the volume it will land on.
fn device_of(path: &Path) -> Option<u64> {
    let mut candidate: Option<&Path> = Some(path);
    while let Some(current) = candidate {
        if let Ok(metadata) = std::fs::metadata(current) {
            return Some(metadata.dev());
        }
        candidate = current.parent();
    }
    None
}
