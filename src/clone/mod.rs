use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::CloneMode;
use crate::error::{Error, Result};
use crate::exclude::{Class, ExcludeSet};
use crate::git::Git;
use crate::ui::Ui;

#[cfg(target_os = "macos")]
mod apfs;
pub mod fake;
#[cfg(target_os = "linux")]
mod reflink;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("wtm clones with clonefile(2) on macOS and FICLONE on Linux; no other platform is supported");

/// How a worktree's files get there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Cow,
    Checkout,
}

impl Method {
    /// The value of `WTM_HOOK_METHOD`.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Cow => "cow",
            Method::Checkout => "checkout",
        }
    }
}

/// The choice and the sentence explaining it.
pub struct MethodDecision {
    pub method: Method,
    pub reason: String,
    /// Which channel the reason belongs on. Falling back because the
    /// filesystem cannot clone is ordinary and goes to progress; a
    /// condition the caller could fix, like a sparse source, is a warning.
    pub surprising: bool,
}

pub trait Cloner {
    /// Copy-on-write clone of a whole tree. `dst` must not exist; its
    /// parent must.
    fn clone_tree(&self, src: &Path, dst: &Path) -> Result<()>;
    /// Clone one regular file. Used inside mixed directories and by the
    /// probe.
    fn clone_file(&self, src: &Path, dst: &Path) -> Result<()>;
    fn name(&self) -> &'static str;
}

pub fn platform_cloner() -> Box<dyn Cloner> {
    #[cfg(target_os = "macos")]
    return Box::new(apfs::Clonefile);
    #[cfg(target_os = "linux")]
    return Box::new(reflink::Reflink);
}

/// Clones a temporary file within `dir` and removes both. `Ok(())` means
/// this filesystem clones; the error says why it does not.
///
/// `decide` has already established that the source and `dir` share a
/// filesystem, so cloning inside `dir` answers the same question without
/// searching the source for a file to copy or touching anything the user
/// owns.
pub fn probe(cloner: &dyn Cloner, dir: &Path) -> Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let src = dir.join(format!(".wtm-probe-{}-{nanos}", std::process::id()));
    let dst = src.with_extension("clone");

    fs::write(&src, b"wtm").map_err(|e| Error::io(&src, e))?;
    let cloned = cloner.clone_file(&src, &dst);
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    cloned
}

/// Which way `wtm new` will populate the worktree, and why.
///
/// `source` is `None` for a bare repository: there are no files to clone,
/// which is a property of the repository rather than something to work
/// around. Fails instead of falling back under `--clone-mode cow`, and is
/// called before anything is created so that failure costs nothing.
pub fn decide(
    git: &Git,
    mode: CloneMode,
    source: Option<&Path>,
    dest_parent: &Path,
    cloner: &dyn Cloner,
) -> Result<MethodDecision> {
    if let CloneMode::Checkout = mode {
        return Ok(MethodDecision {
            method: Method::Checkout,
            reason: "checking out; --clone-mode checkout".to_string(),
            surprising: false,
        });
    }

    match unsupported(git, source, dest_parent, cloner) {
        None => Ok(MethodDecision {
            method: Method::Cow,
            reason: format!("cloning with {}", cloner.name()),
            surprising: false,
        }),
        Some(Unsupported { reason, surprising }) => match mode {
            CloneMode::Cow => Err(Error::CloneUnsupported { reason }),
            _ => Ok(MethodDecision {
                method: Method::Checkout,
                reason: format!("checking out; {reason}"),
                surprising,
            }),
        },
    }
}

struct Unsupported {
    reason: String,
    surprising: bool,
}

/// The conditions of `DESIGN.md` 6.2, in order, cheapest question first.
fn unsupported(
    git: &Git,
    source: Option<&Path>,
    dest_parent: &Path,
    cloner: &dyn Cloner,
) -> Option<Unsupported> {
    let ordinary = |reason: String| {
        Some(Unsupported {
            reason,
            surprising: false,
        })
    };

    let Some(source) = source else {
        return ordinary("the repository is bare and has no files to clone".to_string());
    };
    if is_sparse(git, source) {
        return Some(Unsupported {
            reason: format!(
                "{} is a sparse checkout and a clone would inherit its patterns; \
                 run `git sparse-checkout disable` there to clone instead",
                source.display()
            ),
            surprising: true,
        });
    }
    match (device_of(source), device_of(dest_parent)) {
        (Some(a), Some(b)) if a != b => {
            return ordinary(format!(
                "{} and {} are on different filesystems; point --dir at the repository's volume to clone instead",
                source.display(),
                dest_parent.display()
            ));
        }
        (None, _) | (_, None) => return ordinary("could not stat the source or the data root".to_string()),
        _ => {}
    }
    if let Err(error) = probe(cloner, dest_parent) {
        return ordinary(format!("this filesystem does not support cloning ({error})"));
    }
    None
}

/// A clone would inherit the source's sparse patterns, so a sparse source
/// takes the checkout path.
pub fn is_sparse(git: &Git, source: &Path) -> bool {
    let enabled = git
        .stdout(source, &["config", "--get", "core.sparseCheckout"])
        .is_ok_and(|value| value == "true");
    enabled || git.gitdir_of(source).is_some_and(|dir| dir.join("info/sparse-checkout").exists())
}

fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|m| m.dev())
}

/// What the walk did. Its only reader is the test that fails when the walk
/// stops collapsing whole directories and quietly becomes a file-by-file
/// copy.
#[derive(Clone, Copy, Debug, Default)]
pub struct WalkStats {
    pub tree_clones: u64,
    pub dirs_recursed: u64,
}

pub struct Walker<'a> {
    cloner: &'a dyn Cloner,
    set: &'a ExcludeSet,
    ui: &'a Ui,
    stats: WalkStats,
}

impl<'a> Walker<'a> {
    pub fn new(cloner: &'a dyn Cloner, set: &'a ExcludeSet, ui: &'a Ui) -> Walker<'a> {
        Walker {
            cloner,
            set,
            ui,
            stats: WalkStats::default(),
        }
    }

    /// Clones `src` into `dst`, which exists and holds nothing but the
    /// `.git` file git wrote.
    pub fn run(&mut self, src: &Path, dst: &Path) -> Result<WalkStats> {
        self.walk(src, dst, Path::new(""))?;
        Ok(self.stats)
    }

    fn walk(&mut self, src_root: &Path, dst_root: &Path, rel: &Path) -> Result<()> {
        let src = src_root.join(rel);
        let dst = dst_root.join(rel);
        let at_root = rel.as_os_str().is_empty();

        // The root is recursed whatever its class: git has already made the
        // destination and put a `.git` file in it, so there is nothing for
        // `clone_tree` to create and one entry to skip.
        if !at_root {
            match self.set.classify(rel) {
                Class::Skip => return Ok(()),
                Class::CloneWhole => {
                    self.cloner.clone_tree(&src, &dst)?;
                    self.stats.tree_clones += 1;
                    return Ok(());
                }
                Class::Recurse => {}
            }
            let mode = fs::symlink_metadata(&src).map_err(|e| Error::io(&src, e))?;
            create_dir(&dst, &mode)?;
        }

        self.stats.dirs_recursed += 1;
        for entry in fs::read_dir(&src).map_err(|e| Error::io(&src, e))? {
            let entry = entry.map_err(|e| Error::io(&src, e))?;
            let name = entry.file_name();
            // The source's `.git` is a directory in the main worktree and a
            // file in a linked one. Either way the destination has its own.
            if at_root && name == ".git" {
                continue;
            }
            let kind = entry.file_type().map_err(|e| Error::io(entry.path(), e))?;
            if !(kind.is_dir() || kind.is_file() || kind.is_symlink()) {
                // Git cannot track a socket or a fifo, so the untracked
                // query has already excluded it and the walk should never
                // arrive here. The check is a guard against a confusing
                // failure inside the cloner.
                self.ui.warn(format!(
                    "skipping {}: not a file, a directory or a symlink",
                    entry.path().display()
                ));
                continue;
            }
            self.walk(src_root, dst_root, &rel.join(name))?;
        }
        Ok(())
    }
}

fn create_dir(path: &Path, source: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new()
        .mode(source.permissions().mode())
        .create(path)
        .map_err(|e| Error::io(path, e))
}
