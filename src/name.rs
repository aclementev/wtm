use std::fmt;
use std::path::Path;
use std::str::FromStr;

use crate::error::{Error, Result};

const MAX_BYTES: usize = 200;

/// A validated worktree name: `^[A-Za-z0-9._][A-Za-z0-9._/-]*$`, no empty,
/// `.` or `..` component, at most 200 bytes. A name may contain `/`, which
/// becomes a directory separator on disk.
///
/// Invariant: `Repo::dir(name)` is always strictly inside
/// `Repo::repo_dir()`. The validation exists to guarantee that, since the
/// name arrives from the command line and is joined onto a path we delete from.
///
/// The name doubles as the branch name once `branch_prefix` is applied. Git
/// imposes further rules on branch names (no component starting with `.`, no
/// `.lock` suffix). Those are not repeated here: `wtm new` asks git with
/// `check-ref-format` before it creates anything.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorktreeName(String);

impl WorktreeName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl FromStr for WorktreeName {
    type Err = Error;

    fn from_str(s: &str) -> Result<WorktreeName> {
        if s.is_empty() {
            return Err(Error::usage("worktree name is empty"));
        }
        if s.len() > MAX_BYTES {
            return Err(Error::usage(format!(
                "worktree name is {} bytes, the limit is {MAX_BYTES}",
                s.len()
            )));
        }

        let first = s.as_bytes()[0];
        if !(first.is_ascii_alphanumeric() || first == b'.' || first == b'_') {
            return Err(Error::usage(format!(
                "worktree name {s:?} must start with a letter, digit, '.' or '_'"
            )));
        }
        if let Some(bad) = s
            .bytes()
            .find(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-')))
        {
            return Err(Error::usage(format!(
                "worktree name {s:?} contains {:?}; use letters, digits, \
                 '.', '_', '-' and '/'",
                bad as char
            )));
        }
        for component in s.split('/') {
            if component.is_empty() {
                return Err(Error::usage(format!(
                    "worktree name {s:?} has an empty path component"
                )));
            }
            if component == "." || component == ".." {
                return Err(Error::usage(format!(
                    "worktree name {s:?} has a {component:?} component"
                )));
            }
        }

        Ok(WorktreeName(s.to_string()))
    }
}

impl fmt::Display for WorktreeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
