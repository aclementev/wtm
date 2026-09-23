use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::error::{Error, Result};

/// With this flag `clonefile` copies a symlink argument as a symlink.
/// Without it, a top-level link to a directory becomes a copy of the whole
/// directory it points at.
const CLONE_NOFOLLOW: u32 = 0x0001;

/// `clonefile(2)`. One call copies a whole tree, sharing blocks with the
/// source until something writes. `dst` must not exist; its parent must.
pub fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    clonefile(src, dst)
}

pub fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    clonefile(src, dst)
}

fn clonefile(src: &Path, dst: &Path) -> Result<()> {
    let from = cstring(src)?;
    let to = cstring(dst)?;
    // Safe: both pointers are valid, NUL-terminated and outlive the call.
    let rc = unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), CLONE_NOFOLLOW) };
    if rc != 0 {
        return Err(Error::io(dst, std::io::Error::last_os_error()));
    }
    Ok(())
}

fn cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::usage(format!("{} contains a NUL byte", path.display())))
}
