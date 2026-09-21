use std::fs;
use std::path::Path;

use super::Cloner;
use crate::error::{Error, Result};

/// Copies with `std::fs` instead of cloning, so the walk can run on a
/// filesystem that has no copy-on-write. Nothing reaches it from the
/// command line, because `platform_cloner` never returns it.
pub struct Copier;

impl Cloner for Copier {
    fn clone_tree(&self, src: &Path, dst: &Path) -> Result<()> {
        let kind = fs::symlink_metadata(src).map_err(|e| Error::io(src, e))?;
        if kind.is_symlink() {
            let target = fs::read_link(src).map_err(|e| Error::io(src, e))?;
            return std::os::unix::fs::symlink(target, dst).map_err(|e| Error::io(dst, e));
        }
        if !kind.is_dir() {
            return self.clone_file(src, dst);
        }
        fs::create_dir(dst).map_err(|e| Error::io(dst, e))?;
        for entry in fs::read_dir(src).map_err(|e| Error::io(src, e))? {
            let entry = entry.map_err(|e| Error::io(src, e))?;
            self.clone_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    }

    fn clone_file(&self, src: &Path, dst: &Path) -> Result<()> {
        fs::copy(src, dst).map_err(|e| Error::io(dst, e))?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "fake"
    }
}
