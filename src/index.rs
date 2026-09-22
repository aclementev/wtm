use std::ffi::OsStr;
use std::fs::{self, Metadata, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::Digest;

use crate::error::{Error, Result};
use crate::git::Oid;

const EXTENDED: u16 = 0x4000;
const STAGE: u16 = 0x3000;
const NAME_LENGTH: u16 = 0x0fff;
const FILE_TYPE: u32 = 0o170000;

/// The size of each entry's stat data, which is the whole of what we write.
const STAT_LENGTH: usize = 40;

/// The object hash of a repository. It sets the length of every object id
/// in the index and the digest in its trailer.
#[derive(Clone, Copy, Debug)]
pub enum HashAlgo {
    Sha1,
    Sha256,
}

impl HashAlgo {
    /// Any object id of the repository says which hash it uses.
    pub fn of(oid: &Oid) -> Option<HashAlgo> {
        match oid.as_str().len() {
            40 => Some(HashAlgo::Sha1),
            64 => Some(HashAlgo::Sha256),
            _ => None,
        }
    }

    fn len(self) -> usize {
        match self {
            HashAlgo::Sha1 => 20,
            HashAlgo::Sha256 => 32,
        }
    }

    fn digest(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            HashAlgo::Sha1 => sha1::Sha1::digest(bytes).to_vec(),
            HashAlgo::Sha256 => sha2::Sha256::digest(bytes).to_vec(),
        }
    }
}

/// One entry of an index as git wrote it. `offset` is where its stat data
/// starts.
pub struct Entry {
    pub offset: usize,
    pub mode: u32,
    pub flags: u16,
    pub path: Vec<u8>,
}

impl Entry {
    pub fn stage(&self) -> u16 {
        (self.flags & STAGE) >> 12
    }
}

/// Fills in the stat data of an index that `git read-tree` has just written
/// for `dest`, whose files were cloned from `source`, so that git trusts
/// them without reading them. Returns how many entries were filled, or
/// `None` when the index is not one we understand completely, in which
/// case nothing is written and git has to verify the files itself.
///
/// `since` must come before git was asked which files are dirty in the
/// source. A file changed after that has content git never looked at, and
/// its entry stays zeroed for git to check.
pub fn fill_stat(
    index: &Path,
    dest: &Path,
    source: &Path,
    algo: HashAlgo,
    since: SystemTime,
) -> Result<Option<usize>> {
    let mut bytes = fs::read(index).map_err(|e| Error::io(index, e))?;
    let Some(entries) = entries(&bytes, algo) else {
        return Ok(None);
    };
    let since = since
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);

    let stats = trusted_stats(&entries, dest, source, since);
    let mut filled = 0;
    for (entry, clone) in entries.iter().zip(stats) {
        if let Some(clone) = clone {
            write_stat(&mut bytes[entry.offset..entry.offset + STAT_LENGTH], &clone);
            filled += 1;
        }
    }

    let body = bytes.len() - algo.len();
    let digest = algo.digest(&bytes[..body]);
    bytes[body..].copy_from_slice(&digest);
    install(index, &bytes)?;
    Ok(Some(filled))
}

/// For each entry, the clone's metadata when the entry may be filled from
/// it, in the order of `entries`.
///
/// The first `lstat` of a freshly cloned directory tree is where APFS
/// finishes the clone, and it is most of the time the fill takes. Spread
/// over threads it takes half as long, 0.63 s against 1.3 s for 100k files.
fn trusted_stats(
    entries: &[Entry],
    dest: &Path,
    source: &Path,
    since: i64,
) -> Vec<Option<Metadata>> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let chunk = entries.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = entries
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|entry| trusted_stat(entry, dest, source, since))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("an lstat thread panicked"))
            .collect()
    })
}

/// The clone's metadata, if it may stand for the content HEAD names.
///
/// The dirty query ran after `since` and found the source file unchanged,
/// so a source ctime older than `since` means it is still unchanged. We
/// check ctime because nothing can set it back, so an edit that restores
/// the old mtime still moves it. Whole seconds, because a filesystem with
/// coarse timestamps rounds a later change down to before `since`.
/// Requiring the clone's mtime to be older too keeps every filled entry
/// out of git's racy window, since the index is written after `since`.
///
/// A missing file is the ordinary case for one dirty in the source, which
/// the walk leaves out. Its entry stays zeroed and the reset writes it. A
/// gitlink fails the type check, because the clone has a directory there.
/// No other kind of entry needs a rule: git never trusts stat data for an
/// intent-to-add, skip-worktree or unmerged entry, and `read-tree HEAD`
/// writes none.
fn trusted_stat(entry: &Entry, dest: &Path, source: &Path, since: i64) -> Option<Metadata> {
    let path = Path::new(OsStr::from_bytes(&entry.path));
    let clone = fs::symlink_metadata(dest.join(path)).ok()?;
    let original = fs::symlink_metadata(source.join(path)).ok()?;
    let same_type = clone.mode() & FILE_TYPE == entry.mode & FILE_TYPE;
    (same_type && original.ctime() < since && clone.mtime() < since).then_some(clone)
}

/// Git stores each field as the low 32 bits of what `lstat` returned and
/// compares it that way, so ours are truncated the same way.
fn write_stat(stat: &mut [u8], meta: &Metadata) {
    let fields = [
        (0, meta.ctime() as u32),
        (4, meta.ctime_nsec() as u32),
        (8, meta.mtime() as u32),
        (12, meta.mtime_nsec() as u32),
        (16, meta.dev() as u32),
        (20, meta.ino() as u32),
        // The mode at 24 is kept from HEAD. With `core.fileMode` false the
        // filesystem's executable bit is not the one git records.
        (28, meta.uid()),
        (32, meta.gid()),
        (36, meta.size() as u32),
    ];
    for (at, value) in fields {
        stat[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }
}

/// Written beside the index and renamed over it, so a reader sees the old
/// file or the new one and never part of either. The temporary name is
/// git's lock file, so a git command that tries to write the index
/// meanwhile fails instead of being silently overwritten.
fn install(index: &Path, bytes: &[u8]) -> Result<()> {
    let lock = index.with_extension("lock");
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&lock, index));
    written.map_err(|e| {
        let _ = fs::remove_file(&lock);
        Error::io(index, e)
    })
}

/// Every entry of an index, or `None` unless the whole file is understood.
///
/// We never change an entry's length, so an entry only needs measuring to
/// find where the next one starts. A mistake in that arithmetic anywhere
/// leaves the extensions and the trailer somewhere other than where the
/// file ends, and the whole index is refused. That check is what lets the
/// parser handle three versions without trusting itself.
pub fn entries(bytes: &[u8], algo: HashAlgo) -> Option<Vec<Entry>> {
    let hash = algo.len();
    let body = bytes.len().checked_sub(hash)?;
    let (bytes, trailer) = bytes.split_at(body);
    // We recompute the trailer after writing, which would pass off a
    // damaged index as sound. An all-zero trailer is `index.skipHash`.
    if trailer.iter().any(|&b| b != 0) && algo.digest(bytes) != trailer {
        return None;
    }
    if bytes.get(..4)? != b"DIRC" {
        return None;
    }
    let version = u32_at(bytes, 4)?;
    if !(2..=4).contains(&version) {
        return None;
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut at = 12;
    for _ in 0..u32_at(bytes, 8)? {
        let offset = at;
        let mode = u32_at(bytes, offset + 24)?;
        let flags = u16_at(bytes, offset + STAT_LENGTH + hash)?;
        at = offset + STAT_LENGTH + hash + 2;
        // Extended flags, v3 and up, add two bytes we have no use for.
        if flags & EXTENDED != 0 {
            if version < 3 {
                return None;
            }
            at += 2;
        }
        let path = if version == 4 {
            // The path is the previous one with some bytes stripped from
            // its end, followed by a NUL-terminated suffix. No padding.
            let previous = entries.last().map_or(&[][..], |e| e.path.as_slice());
            let (strip, length) = varint(bytes.get(at..)?)?;
            let kept = previous.len().checked_sub(strip)?;
            at += length;
            let end = nul_from(bytes, at)?;
            let path = [&previous[..kept], &bytes[at..end]].concat();
            at = end + 1;
            path
        } else {
            let end = nul_from(bytes, at)?;
            let path = bytes[at..end].to_vec();
            // One to eight NULs pad the entry to a multiple of eight bytes.
            at = offset + (end - offset + 8) / 8 * 8;
            path
        };
        // The flags repeat the path's length, capped at 12 bits: a free
        // second opinion on where this entry's path ended.
        if usize::from(flags & NAME_LENGTH) != path.len().min(usize::from(NAME_LENGTH)) {
            return None;
        }
        entries.push(Entry {
            offset,
            mode,
            flags,
            path,
        });
    }

    // Git's own rule: an extension whose signature starts with an
    // uppercase letter is optional and may be ignored. Any other, such as
    // a split index's `link`, changes what the entries mean.
    while at + 8 <= bytes.len() {
        if !bytes[at].is_ascii_uppercase() {
            return None;
        }
        at = at
            .checked_add(8)?
            .checked_add(u32_at(bytes, at + 4)? as usize)?;
    }
    (at == bytes.len()).then_some(entries)
}

/// Git's offset varint, which is not LEB128: every continuation adds one
/// before shifting, so each value has exactly one encoding. Returns the
/// value and how many bytes it used.
fn varint(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value = usize::from(*bytes.first()? & 0x7f);
    let mut used = 1;
    while bytes[used - 1] & 0x80 != 0 {
        let byte = *bytes.get(used)?;
        value = value
            .checked_add(1)?
            .checked_mul(128)?
            .checked_add(usize::from(byte & 0x7f))?;
        used += 1;
    }
    Some((value, used))
}

fn nul_from(bytes: &[u8], at: usize) -> Option<usize> {
    Some(at + bytes.get(at..)?.iter().position(|&b| b == 0)?)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
