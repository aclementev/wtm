use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::error::Result;
use crate::git::Git;

/// What the clone walk should do with one path. Named for the action
/// rather than the reason: a path kept because `.worktreeinclude` asked for
/// it and one kept because nothing objected are the same instruction, and
/// the walk is the only caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Untracked, ignored or dirty in the source, with nothing kept below
    /// it.
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

/// What a path was named as. A path can appear in only one list: the include
/// query runs over untracked files, and a file cannot be both dirty and
/// untracked.
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

pub const INCLUDE_FILE: &str = ".worktreeinclude";

/// Where the include patterns are read from. Always the source, which is
/// the main worktree: the patterns are matched by git in the source, so a
/// copy in a linked worktree would have nothing to match against.
pub fn include_file(source: &Path) -> PathBuf {
    source.join(INCLUDE_FILE)
}

/// The untracked paths `.worktreeinclude` asks to carry, empty when there
/// is no such file. It is not carried itself unless it names itself: an
/// untracked file nothing ignores would leave a `??` in the new worktree's
/// status, which `wtm rm` refuses to remove without `--force`.
pub fn included_paths(git: &Git, source: &Path) -> Result<Vec<PathBuf>> {
    let file = include_file(source);
    if !file.exists() {
        return Ok(Vec::new());
    }
    let arg = format!("--exclude-from={}", file.display());
    paths(git, source, &["ls-files", "-z", "-o", "-i", &arg])
}

impl ExcludeSet {
    /// Runs the four queries of `DESIGN.md` 6.3 in the source worktree. Git
    /// is the only authority on what is ignored: a directory holding one
    /// tracked file is never collapsed, and a matcher written here would
    /// eventually delete a tracked file.
    pub fn compute(git: &Git, source: &Path) -> Result<ExcludeSet> {
        let mut excluded = paths(git, source, &["ls-files", "-z", "-o", "-i", "--exclude-standard", "--directory"])?;
        excluded.extend(paths(git, source, &["ls-files", "-z", "-o", "--exclude-standard", "--directory"])?);
        // A tracked file modified in the source carries content its index
        // entry does not describe. Leaving it out means the destination has
        // no file there and git writes the committed version.
        excluded.extend(paths(git, source, &["diff-index", "-z", "--name-only", "HEAD"])?);

        Ok(ExcludeSet::from_lists(excluded, included_paths(git, source)?))
    }

    /// Pure, so the classification rules can be exercised without a
    /// repository. An included path wins over an excluded ancestor, which
    /// becomes `Mixed`.
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
        let mut excluded_ancestor = false;
        for component in components(rel) {
            excluded_ancestor |= node.mark == Some(Mark::Excluded);
            match node.children.get(component) {
                // Nothing at or below `rel` was named, so only an excluded
                // ancestor has anything to say about it.
                None => return if excluded_ancestor { Class::Skip } else { Class::CloneWhole },
                Some(child) => node = child,
            }
        }
        classify_node(node, excluded_ancestor)
    }
}

fn classify_node(node: &Node, excluded_ancestor: bool) -> Class {
    let excluded_here = match node.mark {
        Some(Mark::Included) => false,
        Some(Mark::Excluded) => true,
        None => excluded_ancestor,
    };
    // An include below outranks an exclusion at or above it: the walk must
    // descend to reach the include whatever this directory was called, and
    // getting this precedence backwards skips a whole ignored tree that the
    // user asked to keep one file out of. An exclusion below only matters
    // where the walk would otherwise clone the subtree whole.
    let must_descend = node.include_below || (node.exclude_below && !excluded_here);

    match (must_descend, excluded_here) {
        (true, _) => Class::Recurse,
        (false, true) => Class::Skip,
        (false, false) => Class::CloneWhole,
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

/// Git never emits `.` or `..` and `-z` never quotes, so anything else in a
/// path is a component. Dropped prefixes would silently widen a class.
fn components(path: &Path) -> impl Iterator<Item = &OsStr> {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name),
        _ => None,
    })
}

/// Splits a `-z` listing. Git writes paths relative to the directory it ran
/// in, which is always the source root here.
fn paths(git: &Git, source: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    let output = git.run(source, args)?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| PathBuf::from(OsStr::from_bytes(record)))
        .collect())
}
