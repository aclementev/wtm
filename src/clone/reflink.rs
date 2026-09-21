use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use super::Cloner;
use crate::error::{Error, Result};

/// `ioctl(dst, FICLONE, src)` from `<linux/fs.h>`. It copies data and
/// nothing else, so the caller copies mode and mtime. The mtime has to
/// match the source or the index we write will not validate.
const FICLONE: libc::c_ulong = 0x4004_9409;

/// Per-file reflinks. Unlike `clonefile(2)` there is no whole-tree call, so
/// `clone_tree` walks the subtree itself.
pub struct Reflink;

impl Cloner for Reflink {
    fn clone_tree(&self, src: &Path, dst: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(src).map_err(|e| Error::io(src, e))?;
        if meta.is_symlink() {
            let target = fs::read_link(src).map_err(|e| Error::io(src, e))?;
            return std::os::unix::fs::symlink(target, dst).map_err(|e| Error::io(dst, e));
        }
        if meta.is_file() {
            return self.clone_file(src, dst);
        }
        if !meta.is_dir() {
            // Git cannot track a socket or a fifo, so nothing the worktree
            // needs is lost by leaving it out.
            return Ok(());
        }
        fs::DirBuilder::new()
            .mode(meta.permissions().mode())
            .create(dst)
            .map_err(|e| Error::io(dst, e))?;
        for entry in fs::read_dir(src).map_err(|e| Error::io(src, e))? {
            let entry = entry.map_err(|e| Error::io(src, e))?;
            self.clone_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    }

    fn clone_file(&self, src: &Path, dst: &Path) -> Result<()> {
        let from = File::open(src).map_err(|e| Error::io(src, e))?;
        let meta = from.metadata().map_err(|e| Error::io(src, e))?;
        let to = File::create(dst).map_err(|e| Error::io(dst, e))?;

        // Safe: both descriptors are open for the duration of the call.
        let rc = unsafe { libc::ioctl(to.as_raw_fd(), FICLONE, from.as_raw_fd()) };
        if rc != 0 {
            return Err(Error::io(dst, std::io::Error::last_os_error()));
        }
        to.set_permissions(fs::Permissions::from_mode(meta.mode()))
            .map_err(|e| Error::io(dst, e))?;
        set_mtime(&to, dst, &meta)
    }

    fn name(&self) -> &'static str {
        "reflink"
    }
}

/// The index records each file's mtime and git re-reads any file whose
/// mtime moved, so a clone that stamps the current time would cost a hash
/// of the whole tree on the first status.
fn set_mtime(file: &File, path: &Path, source: &fs::Metadata) -> Result<()> {
    let times = [
        libc::timespec {
            tv_sec: source.atime(),
            tv_nsec: source.atime_nsec(),
        },
        libc::timespec {
            tv_sec: source.mtime(),
            tv_nsec: source.mtime_nsec(),
        },
    ];
    // Safe: the descriptor is open and `times` is a two-element array.
    let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
    if rc != 0 {
        return Err(Error::io(path, std::io::Error::last_os_error()));
    }
    Ok(())
}
