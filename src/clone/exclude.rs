use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::error::Result;
use crate::git;

/// What the clone walk does with one path. The names say the action rather
/// than the reason. A path kept because `.worktreeinclude` asked for it and
/// one kept because nothing objected are the same instruction, and the walk
/// is the only caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Untracked, ignored or dirty in the source, with nothing kept below it.
    Skip,
    /// Clone the whole subtree in one call.
    CloneWhole,
    /// Holds both kept and skipped paths below it.
    Recurse,
}

/// The source's untracked, ignored, dirty and included paths, arranged so
/// the walk can ask about one path at a time.
pub struct ExcludeSet {
    root: Node,
}

/// What the queries said about a path. Both lists name the same path often:
/// the include query runs over untracked files, so everything it returns is
/// also in the untracked list. `from_lists` inserts the includes second so
/// they win.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Excluded,
    Included,
}

#[derive(Default)]
struct Node {
    children: BTreeMap<OsString, Node>,
    /// Set when a query named this exact path.
    mark: Option<Mark>,
    /// Set on every ancestor of an included path.
    include_below: bool,
    /// Set on every ancestor of an excluded path.
    exclude_below: bool,
}

const INCLUDE_FILE: &str = ".worktreeinclude";

/// Where the include patterns come from. Always the source, which is the
/// main worktree. Git matches the patterns in the source, so a copy sitting
/// in a linked worktree has nothing to match against and does nothing.
pub fn include_file(source: &Path) -> PathBuf {
    source.join(INCLUDE_FILE)
}

/// The untracked paths `.worktreeinclude` asks to carry, empty when there is
/// no such file.
///
/// The file carries itself only if it names itself. Carrying an untracked
/// file that nothing ignores would leave a `??` in the new worktree's
/// status, and `wtm rm` refuses a worktree whose status is not empty.
pub fn included_paths(source: &Path) -> Result<Vec<PathBuf>> {
    let file = include_file(source);
    if !file.exists() {
        return Ok(Vec::new());
    }
    let arg = format!("--exclude-from={}", file.display());
    paths(source, &["ls-files", "-z", "-o", "-i", &arg])
}

impl ExcludeSet {
    /// Asks git four questions in the source worktree: what is ignored,
    /// what is untracked, what the include file matches, and which tracked
    /// files are dirty.
    ///
    /// Git is the only authority on what is ignored. It never collapses a
    /// directory holding even one tracked file, and a matcher written here
    /// would eventually delete a tracked file.
    pub fn compute(source: &Path) -> Result<ExcludeSet> {
        let ignored = &[
            "ls-files",
            "-z",
            "-o",
            "-i",
            "--exclude-standard",
            "--directory",
        ];
        let untracked = &["ls-files", "-z", "-o", "--exclude-standard", "--directory"];
        // A tracked file modified in the source holds content its index
        // entry does not describe. Leaving it out of the walk means the
        // destination has no file there and git writes the committed
        // version.
        let dirty = &["diff-index", "-z", "--name-only", "HEAD"];

        let mut excluded = paths(source, ignored)?;
        excluded.extend(paths(source, untracked)?);
        excluded.extend(paths(source, dirty)?);

        Ok(ExcludeSet::from_lists(excluded, included_paths(source)?))
    }

    /// Pure, so the rules run without a repository on disk.
    pub fn from_lists(excluded: Vec<PathBuf>, included: Vec<PathBuf>) -> ExcludeSet {
        let mut root = Node::default();
        for path in &excluded {
            root.insert(path, Mark::Excluded);
        }
        // Second, so an include overwrites the mark of a path both lists
        // name and the walk keeps the file.
        for path in &included {
            root.insert(path, Mark::Included);
        }
        ExcludeSet { root }
    }

    pub fn classify(&self, rel: &Path) -> Class {
        let mut node = &self.root;
        let mut under_exclusion = false;
        for component in components(rel) {
            under_exclusion |= node.mark == Some(Mark::Excluded);
            match node.children.get(component) {
                Some(child) => node = child,
                // No query named anything at or below `rel`, so only an
                // excluded ancestor has a say.
                None => {
                    return if under_exclusion {
                        Class::Skip
                    } else {
                        Class::CloneWhole
                    };
                }
            }
        }

        let excluded = match node.mark {
            Some(Mark::Included) => false,
            Some(Mark::Excluded) => true,
            None => under_exclusion,
        };
        // An include below outranks an exclusion at or above it. The walk
        // has to descend to reach the include whatever this directory was
        // called, and reversing this precedence skips a whole ignored tree
        // that someone asked to keep one file out of. An exclusion below
        // only matters where the walk would otherwise clone the subtree
        // whole.
        let descend = node.include_below || (node.exclude_below && !excluded);

        match (descend, excluded) {
            (true, _) => Class::Recurse,
            (false, true) => Class::Skip,
            (false, false) => Class::CloneWhole,
        }
    }
}

impl Node {
    fn insert(&mut self, path: &Path, mark: Mark) {
        let mut node = self;
        for component in components(path) {
            match mark {
                Mark::Excluded => node.exclude_below = true,
                Mark::Included => node.include_below = true,
            }
            node = node.children.entry(component.to_os_string()).or_default();
        }
        node.mark = Some(mark);
    }
}

/// Git never emits `.` or `..`, and `-z` never quotes, so every part of a
/// path git gave us is a normal component.
fn components(path: &Path) -> impl Iterator<Item = &OsStr> {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name),
        _ => None,
    })
}

/// Splits a `-z` listing. Git writes the paths relative to the directory it
/// ran in, which is always the source root here.
fn paths(source: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    let output = git::run(source, args)?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| PathBuf::from(OsStr::from_bytes(record)))
        .collect())
}
