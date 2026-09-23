use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::error::{Error, Result};

/// `worktree list --porcelain -z` arrived in git 2.36. Parallel checkout,
/// which the fallback creation path relies on, arrived in 2.31.
const MIN_VERSION: (u32, u32) = (2, 36);

/// Variables that would silently redirect a subprocess at the caller's
/// repository instead of the one we named with `-C`. Inheriting them has
/// produced false merge conflicts in comparable tools.
pub(crate) const SCRUBBED: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

/// A git object id in hex, 40 characters for SHA-1 and 64 for SHA-256.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(String);

impl Oid {
    /// Only for tests and parsers. The hex is not checked.
    pub fn from_hex(hex: impl Into<String>) -> Oid {
        Oid(hex.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// An abbreviation for display only. Fixed-width rather than git's
    /// shortest-unambiguous length, which would cost a subprocess per row.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(7)]
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One record of `git worktree list --porcelain`.
#[derive(Clone, Debug)]
pub struct GitWorktree {
    pub path: PathBuf,
    pub head: Option<Oid>,
    pub branch: Option<String>,
    /// The repository itself, when it has no working tree.
    pub bare: bool,
    /// `git worktree lock`: the user has asked that nothing remove this,
    /// typically because it lives on removable media.
    pub locked: bool,
    /// Git's own judgement that the worktree is gone: the directory has
    /// vanished, or its gitdir pointer is broken. Better than stat'ing the
    /// path ourselves, and already in this output.
    pub prunable: bool,
}

impl GitWorktree {
    /// The branch without its `refs/heads/` prefix.
    pub fn branch_short(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub raw: String,
}

/// Checks that git is new enough and returns its version, which `doctor`
/// reports. Every git invocation in `wtm` goes through this module; nothing
/// else spawns git.
pub fn check_version() -> Result<GitVersion> {
    let version = parse_version(&stdout(Path::new("."), &["--version"])?)?;
    if (version.major, version.minor) < MIN_VERSION {
        return Err(Error::GitVersion {
            found: version.raw,
            needed: format!("{}.{}", MIN_VERSION.0, MIN_VERSION.1),
        });
    }
    Ok(version)
}

/// Stdout is always captured, never inherited. It is the one stream
/// `wtm` keeps clear of anything but its own result.
fn command(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).args(args);
    cmd.stdin(Stdio::null());
    for key in SCRUBBED {
        cmd.env_remove(key);
    }
    cmd
}

/// Runs git, failing on a non-zero exit status. Stderr comes back in the
/// `Output` for the caller to relay; it is never written directly.
pub fn run(cwd: &Path, args: &[&str]) -> Result<Output> {
    let output = command(cwd, args)
        .output()
        .map_err(|e| Error::io("git", e))?;
    if !output.status.success() {
        return Err(Error::Git {
            args: args.iter().map(|a| a.to_string()).collect(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output)
}

/// Runs git with its stderr on ours, for the steps slow enough that a person
/// should see git's progress. Stdout still goes nowhere near ours. Under
/// `quiet` this is `run` with the output dropped, so a failure still carries
/// git's message.
pub fn stream(cwd: &Path, args: &[&str], quiet: bool) -> Result<()> {
    if quiet {
        return run(cwd, args).map(|_| ());
    }
    let status = command(cwd, args)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| Error::io("git", e))?;
    if !status.success() {
        return Err(Error::Git {
            args: args.iter().map(|a| a.to_string()).collect(),
            status: status.code().unwrap_or(-1),
            stderr: "see git's message above".to_string(),
        });
    }
    Ok(())
}

pub fn stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = run(cwd, args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// True when git exits zero. For questions where failure is an answer
/// rather than an error, such as whether a ref exists.
pub fn succeeds(cwd: &Path, args: &[&str]) -> bool {
    command(cwd, args)
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn rev_parse(cwd: &Path, rev: &str) -> Result<Oid> {
    let out = stdout(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
    )?;
    if out.is_empty() {
        return Err(Error::usage(format!("{rev} does not name a commit")));
    }
    Ok(Oid(out))
}

pub fn merge_base(cwd: &Path, a: &str, b: &str) -> Option<Oid> {
    let output = command(cwd, &["merge-base", a, b]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string();
    (!text.is_empty()).then_some(Oid(text))
}

pub fn status_is_clean(cwd: &Path) -> Result<bool> {
    Ok(stdout(cwd, &["status", "--porcelain"])?.is_empty())
}

pub fn branch_exists(cwd: &Path, branch: &str) -> bool {
    succeeds(
        cwd,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// The gitlink entries of the index, one per submodule. Mode 160000 is
/// what makes an entry a submodule rather than a file.
pub fn gitlinks(cwd: &Path) -> Result<Vec<PathBuf>> {
    let output = run(cwd, &["ls-files", "-z", "--stage"])?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            // "<mode> <oid> <stage>\t<path>"
            let rest = record.strip_prefix(b"160000 ")?;
            let tab = rest.iter().position(|byte| *byte == b'\t')?;
            Some(PathBuf::from(OsStr::from_bytes(&rest[tab + 1..])))
        })
        .collect())
}

pub fn worktrees(cwd: &Path) -> Result<Vec<GitWorktree>> {
    let output = run(cwd, &["worktree", "list", "--porcelain", "-z"])?;
    Ok(parse_worktree_list(&output.stdout))
}

/// The worktree root containing `cwd`: for a linked worktree that is
/// the worktree itself, not the main one.
pub fn toplevel(cwd: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(stdout(
        cwd,
        &["rev-parse", "--show-toplevel"],
    )?))
}

/// Git's metadata directory for a worktree. Git derives its name from the
/// basename of the worktree path and appends a digit on collision, so a
/// worktree named `feat` can live in `worktrees/feat1`. It must be read
/// back like this, never built by joining the worktree name.
pub fn gitdir_of(worktree: &Path) -> Option<PathBuf> {
    stdout(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )
    .ok()
    .map(PathBuf::from)
}

/// The operation blocking a worktree, if any, named as git names it. The
/// markers live in the worktree's own gitdir, not the common directory.
pub fn in_progress_operation(gitdir: &Path) -> Option<&'static str> {
    const MARKERS: [(&str, &str); 6] = [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ];
    MARKERS
        .iter()
        .find(|(marker, _)| gitdir.join(marker).exists())
        .map(|(_, name)| *name)
}

fn parse_version(text: &str) -> Result<GitVersion> {
    let raw = text.trim().to_string();
    let numbers = raw
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        .ok_or_else(|| Error::usage(format!("cannot read a version from {raw:?}")))?;
    let mut parts = numbers.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    Ok(GitVersion {
        major: parts.next().unwrap_or(0),
        minor: parts.next().unwrap_or(0),
        raw,
    })
}

/// Records are separated by an empty field, attributes within a record by NUL.
/// The `-z` form is used because a worktree path may contain a newline.
fn parse_worktree_list(bytes: &[u8]) -> Vec<GitWorktree> {
    let mut worktrees = Vec::new();
    let mut current: Option<GitWorktree> = None;

    for field in bytes.split(|b| *b == 0) {
        if field.is_empty() {
            worktrees.extend(current.take());
            continue;
        }
        let (key, value) = match field.iter().position(|b| *b == b' ') {
            Some(i) => (&field[..i], &field[i + 1..]),
            None => (field, &field[field.len()..]),
        };
        let text = || String::from_utf8_lossy(value).into_owned();
        match key {
            b"worktree" => {
                worktrees.extend(current.take());
                current = Some(GitWorktree {
                    path: PathBuf::from(OsStr::from_bytes(value)),
                    head: None,
                    branch: None,
                    bare: false,
                    locked: false,
                    prunable: false,
                });
            }
            _ => {
                let Some(worktree) = current.as_mut() else {
                    continue;
                };
                match key {
                    b"HEAD" => worktree.head = Some(Oid(text())),
                    b"branch" => worktree.branch = Some(text()),
                    b"bare" => worktree.bare = true,
                    b"locked" => worktree.locked = true,
                    b"prunable" => worktree.prunable = true,
                    _ => {}
                }
            }
        }
    }
    worktrees.extend(current);
    worktrees
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newline_in_a_path_does_not_split_the_record() {
        let bytes = b"worktree /a/we\nird\0HEAD 072b863\0detached\0\0";
        let list = parse_worktree_list(bytes);

        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, PathBuf::from("/a/we\nird"));
    }

    #[test]
    fn reads_apple_and_upstream_version_strings() {
        assert_eq!(parse_version("git version 2.51.0").unwrap().minor, 51);
        let apple = parse_version("git version 2.39.3 (Apple Git-146)").unwrap();
        assert_eq!((apple.major, apple.minor), (2, 39));
    }
}
