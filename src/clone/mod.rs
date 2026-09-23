//! Filling a new worktree by copy-on-write cloning the main worktree.
//!
//! `unavailable` says whether cloning can work between two places.
//! `populate` fills a freshly registered worktree with a clone and an index
//! git trusts without reading the files. `copy_included` carries the
//! `.worktreeinclude` files when git checked the tree out instead. The rest
//! is public for `wtm doctor` and for the tests.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::git::{self, Oid};
use crate::ui::Ui;

pub mod exclude;
pub mod index;

#[cfg(target_os = "macos")]
mod apfs;
#[cfg(target_os = "macos")]
use apfs as platform;
#[cfg(target_os = "linux")]
mod reflink;
#[cfg(target_os = "linux")]
use reflink as platform;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!(
    "wtm clones with clonefile(2) on macOS and FICLONE on Linux; no other platform is supported"
);

use exclude::{Class, ExcludeSet};
use index::HashAlgo;

/// Why cloning cannot work here.
pub struct Unavailable {
    pub reason: String,
    /// A condition the caller could fix, such as a sparse source, which is
    /// worth a warning. A filesystem that cannot clone is ordinary and is
    /// only reported as progress.
    pub fixable: bool,
}

/// Whether cloning from `source` into `dest_parent` would fail, cheapest
/// question first. `source` is `None` for a bare repository, which has no
/// files to clone.
pub fn unavailable(source: Option<&Path>, dest_parent: &Path) -> Option<Unavailable> {
    let ordinary = |reason: String| {
        Some(Unavailable {
            reason,
            fixable: false,
        })
    };

    let Some(source) = source else {
        return ordinary("the repository is bare and has no files to clone".to_string());
    };
    if is_sparse(source) {
        return Some(Unavailable {
            reason: format!(
                "{} is a sparse checkout and a clone would inherit its patterns; \
                 run `git sparse-checkout disable` there to clone instead",
                source.display()
            ),
            fixable: true,
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
        (None, _) | (_, None) => {
            return ordinary("could not stat the source or the data root".to_string());
        }
        _ => {}
    }
    if let Err(error) = probe(dest_parent) {
        return ordinary(format!(
            "this filesystem does not support cloning ({error})"
        ));
    }
    None
}

/// Clones a temporary file within `dir` and removes both. The device check
/// before it has already established that the source and `dir` share a
/// filesystem, so cloning inside `dir` answers the same question without
/// touching anything the caller owns.
fn probe(dir: &Path) -> Result<()> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let src = dir.join(format!(".wtm-probe-{}-{nanos}", std::process::id()));
    let dst = src.with_extension("clone");

    fs::write(&src, b"wtm").map_err(|e| Error::io(&src, e))?;
    let cloned = platform::clone_file(&src, &dst);
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    cloned
}

/// A clone would inherit the source's sparse patterns, so a sparse source
/// takes the checkout path.
pub fn is_sparse(source: &Path) -> bool {
    let enabled = git::stdout(source, &["config", "--get", "core.sparseCheckout"])
        .is_ok_and(|value| value == "true");
    enabled || git::gitdir_of(source).is_some_and(|dir| dir.join("info/sparse-checkout").exists())
}

/// The deepest ancestor of `path` that exists, `path` itself included.
pub fn nearest_existing(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

/// The device `path` is on, or would land on. A data root nobody has
/// created yet still reports the volume it will be made in.
pub fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(nearest_existing(path)?).ok().map(|m| m.dev())
}

/// Fills `dest`, a worktree git has just registered at `source`'s HEAD with
/// nothing in it but its `.git` file, with a clone of `source`'s tracked
/// files and its `.worktreeinclude` files. Leaves an index git trusts
/// without reading the files, and a clean status.
///
/// `WTM_NO_FAST_INDEX`, set to any value, skips our index code and lets git
/// verify the clone by reading every file. It is slower and is the way out
/// should the index fill ever be suspected.
pub fn populate(ui: &Ui, source: &Path, dest: &Path) -> Result<()> {
    // Taken before the dirty query inside `compute`, so that a file changed
    // after git last looked at it is caught by the index fill below.
    let since = SystemTime::now();
    let set = ExcludeSet::compute(source)?;
    walk(ui, &set, source, dest, Path::new(""))?;

    git::run(dest, &["read-tree", "HEAD"])?;
    empty_submodules(dest)?;

    // `read-tree` leaves every entry's stat data zeroed, and checkout reads
    // cached stat rather than hashing, so a `reset --hard` on top of it
    // would rewrite every file from the object store and waste the clone.
    let filled = if std::env::var_os("WTM_NO_FAST_INDEX").is_some() {
        ui.progress("WTM_NO_FAST_INDEX is set, so git will read every file to verify the clone");
        false
    } else {
        fill_index(ui, source, dest, since)?
    };
    // Without our stat data, the refresh pays for one hash of the tree,
    // finds the content already correct and writes the true stat back. It
    // exits non-zero when a file needs updating, which is what we asked it
    // to find out. After a fill it would find nothing: every entry left
    // zeroed is one the reset should write from the object store anyway.
    if !filled {
        let _ = git::run(dest, &["update-index", "--refresh", "-q"]);
    }
    git::run(dest, &["reset", "-q", "--hard"])?;
    Ok(())
}

/// Clones `rel` from `src_root` to `dst_root`, whole where nothing below it
/// is excluded and one level at a time where something is.
///
/// The walk always recurses at the root, whatever its class. Git has
/// already made the destination and put a `.git` file in it, so there is
/// nothing for `clone_tree` to create and one entry to skip.
fn walk(ui: &Ui, set: &ExcludeSet, src_root: &Path, dst_root: &Path, rel: &Path) -> Result<()> {
    let src = src_root.join(rel);
    let dst = dst_root.join(rel);
    let at_root = rel.as_os_str().is_empty();

    if !at_root {
        match set.classify(rel) {
            Class::Skip => return Ok(()),
            Class::CloneWhole => return platform::clone_tree(&src, &dst),
            Class::Recurse => {}
        }
        let source = fs::symlink_metadata(&src).map_err(|e| Error::io(&src, e))?;
        fs::DirBuilder::new()
            .mode(source.permissions().mode())
            .create(&dst)
            .map_err(|e| Error::io(&dst, e))?;
    }

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
            // Git cannot track a socket or a fifo, so the untracked query
            // has already excluded it and the walk should never arrive
            // here. The check guards against a confusing failure inside the
            // clone call.
            ui.warn(format!(
                "skipping {}: not a file, a directory or a symlink",
                entry.path().display()
            ));
            continue;
        }
        walk(ui, set, src_root, dst_root, &rel.join(name))?;
    }
    Ok(())
}

/// Writes the cloned files' stat data into the index so git trusts them
/// without reading them. False when the index was left alone, and git has
/// to verify the clone itself.
fn fill_index(ui: &Ui, source: &Path, dest: &Path, since: SystemTime) -> Result<bool> {
    let index = PathBuf::from(git::stdout(
        dest,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    let head: Oid = git::rev_parse(dest, "HEAD")?;
    let filled = match HashAlgo::of(&head) {
        Some(algo) => index::fill_stat(&index, dest, source, algo, since)?,
        None => None,
    };
    if filled.is_none() {
        ui.warn(format!(
            "wtm cannot read the index format git wrote at {}, so git will read every file \
             to verify the clone. The worktree is still correct, and a newer wtm may \
             restore the fast path",
            index.display()
        ));
    }
    Ok(filled.is_some())
}

/// The checkout path's share of `.worktreeinclude`. Git wrote only tracked
/// files, so the untracked ones the include file matches are copied in from
/// the source.
///
/// A file already there is one the new branch tracks. Copying over it would
/// leave the worktree dirty from the start, and on the clone path git
/// refuses the same conflict, so this refuses too.
pub fn copy_included(source: &Path, dest: &Path) -> Result<()> {
    for rel in exclude::included_paths(source)? {
        let (from, to) = (source.join(&rel), dest.join(&rel));
        if to.symlink_metadata().is_ok() {
            let why = std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "the new branch tracks this file, and .worktreeinclude asks for the \
                 untracked copy in the main worktree; remove one of the two",
            );
            return Err(Error::io(&to, why));
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        // Git lists an untracked symlink as a file. Copying it would copy
        // what it points at.
        let meta = fs::symlink_metadata(&from).map_err(|e| Error::io(&from, e))?;
        if meta.is_symlink() {
            let target = fs::read_link(&from).map_err(|e| Error::io(&from, e))?;
            std::os::unix::fs::symlink(target, &to).map_err(|e| Error::io(&to, e))?;
        } else {
            fs::copy(&from, &to).map_err(|e| Error::io(&to, e))?;
        }
    }
    Ok(())
}

/// `git worktree add` leaves submodule directories empty and so does
/// `wtm`. Cloning their contents would carry `.git` files pointing at the
/// source's gitdir and break every git command inside them. Filling them
/// is the init hook's job.
fn empty_submodules(dest: &Path) -> Result<()> {
    for submodule in git::gitlinks(dest)? {
        let path = dest.join(submodule);
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|e| Error::io(&path, e))?;
        }
        fs::create_dir_all(&path).map_err(|e| Error::io(&path, e))?;
    }
    Ok(())
}
