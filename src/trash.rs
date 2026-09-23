//! Removal that returns at once. A removed worktree is renamed into its
//! repository's trash directory, which takes one `rename(2)` whatever its
//! size, and a detached reaper unlinks it afterwards. Nothing depends on the
//! reaper finishing: any later command, or `wtm gc`, starts another, and a
//! half-deleted entry is simply resumed.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::ui::Ui;

/// Set to any value to stop wtm spawning background reapers. Tests need it:
/// `wtm rm` spawns a reaper for the entry it just made, so "exactly one entry
/// is in the trash" is otherwise a race against that reaper.
const NO_REAPER: &str = "WTM_NO_REAPER";

/// Takes the tree at `path` away. Under `wait`, or when it cannot be renamed
/// into `trash`, it is deleted here and now. Otherwise it is renamed into
/// `trash` under a name built from `label`, and a detached reaper unlinks it.
///
/// The rename is `std::fs::rename`, which is `rename(2)` and nothing else,
/// never `mv` or a helper that would silently copy the tree to another
/// filesystem. A trash elsewhere fails with `EXDEV`, a mount point with
/// `EBUSY`, and both fall back to deleting in place.
pub fn discard(ui: &Ui, path: &Path, trash: &Path, label: &str, wait: bool) -> Result<()> {
    if !wait {
        match rename_into(path, trash, label) {
            Ok(()) => return spawn_reaper(trash),
            Err(error) => ui.warn(format!("{}; deleting it in place instead", why(&error))),
        }
    }
    report(ui, delete(path))
}

/// Empties the trash directories that have anything in them: a detached
/// reaper each, or a sweep in this process under `wait`, which reports what
/// it did.
pub fn collect(ui: &Ui, trashes: &[PathBuf], wait: bool) -> Result<()> {
    let full: Vec<PathBuf> = trashes
        .iter()
        .filter(|trash| has_entries(trash))
        .cloned()
        .collect();
    if !wait {
        return full.iter().try_for_each(|trash| spawn_reaper(trash));
    }
    let stats = sweep(ui, &full);
    ui.progress(format!(
        "swept {} entries, {} left to another sweep",
        stats.deleted, stats.skipped
    ));
    match stats.failed {
        0 => Ok(()),
        count => Err(Error::Undeleted(count)),
    }
}

/// The reaper itself: detach from the caller for good, then sweep `trash`.
/// `wtm gc --detach --trash <dir>` lands here, before any repository is
/// discovered, because a reaper needs one directory and nothing else.
pub fn reap(ui: &Ui, trash: &Path) -> Result<()> {
    detach_self()?;
    match sweep(ui, std::slice::from_ref(&trash.to_path_buf())).failed {
        0 => Ok(()),
        count => Err(Error::Undeleted(count)),
    }
}

fn has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

fn rename_into(path: &Path, trash: &Path, label: &str) -> io::Result<()> {
    std::fs::create_dir_all(trash)?;
    // Bounded rather than a retry until it works: every attempt reads a
    // clock, and a clock that never moves would otherwise hang `wtm rm`.
    let mut last = None;
    for _ in 0..16 {
        let entry = trash.join(entry_name(label));
        match std::fs::rename(path, &entry) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no trash name was free")))
}

/// Unique within one trash directory, which is all that is asked of it: the
/// rename itself refuses a collision, so the clock only has to advance.
/// Slashes become dashes so a nested name stays one directory.
fn entry_name(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!("{}-{}-{nanos}", label.replace('/', "-"), std::process::id())
}

/// The two failures the design expects, said in words. Anything else is
/// reported as the operating system worded it.
fn why(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EXDEV) => "the trash is on another filesystem".to_string(),
        Some(libc::EBUSY) => "the worktree is in use".to_string(),
        _ => error.to_string(),
    }
}

/// Paths a synchronous delete could not remove are the user's to deal with,
/// so they are named and the command exits 1.
fn report(ui: &Ui, failed: Vec<PathBuf>) -> Result<()> {
    for path in &failed {
        ui.warn(format!("could not remove {}", path.display()));
    }
    match failed.len() {
        0 => Ok(()),
        count => Err(Error::Undeleted(count)),
    }
}

/// What a sweep did. `failed` counts paths that survived, which the caller
/// turns into exit 1; each has already been named in a warning.
#[derive(Default)]
struct SweepStats {
    deleted: usize,
    skipped: usize,
    failed: usize,
}

/// Runs the sweep in a process that outlives this one. The child holds no
/// descriptor of ours, so `$(wtm rm x)` sees end of file immediately rather
/// than waiting for the unlink to finish.
fn spawn_reaper(trash: &Path) -> Result<()> {
    if std::env::var_os(NO_REAPER).is_some() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| Error::io("the wtm binary", e))?;

    let mut command = Command::new(exe);
    command
        .args(["gc", "--detach", "--trash"])
        .arg(trash)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // A new session before exec, so closing the terminal or exiting the shell
    // cannot signal the reaper. Doing it here rather than after exec leaves no
    // window in which the child still belongs to the caller's process group.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    command
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::io("spawning the reaper", e))
}

/// Puts this process in the background for good: own session, no inherited
/// streams, and low enough priority that sweeping never competes with the
/// work the user is actually doing.
fn detach_self() -> Result<()> {
    // EPERM here means we already lead a process group, which is just as good.
    unsafe { libc::setsid() };
    redirect_streams()?;
    lower_priority();
    Ok(())
}

/// Points all three standard streams at `/dev/null`, or at a log when
/// `WTM_DEBUG` is set. Holding the caller's stdout open is what would make
/// `$(wtm rm x)` block until the sweep finished.
fn redirect_streams() -> Result<()> {
    let sink = match std::env::var_os("WTM_DEBUG") {
        None => std::fs::File::options().write(true).open("/dev/null"),
        Some(_) => {
            let log = crate::config::reaper_log_file();
            if let Some(parent) = log.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
            std::fs::File::options()
                .create(true)
                .append(true)
                .open(&log)
        }
    }
    .map_err(|e| Error::io("the reaper's output", e))?;

    let fd = sink.as_raw_fd();
    for stream in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        unsafe { libc::dup2(fd, stream) };
    }
    Ok(())
}

fn lower_priority() {
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) };

    #[cfg(target_os = "macos")]
    unsafe {
        darwin::setiopolicy_np(
            darwin::IOPOL_TYPE_DISK,
            darwin::IOPOL_SCOPE_PROCESS,
            darwin::IOPOL_THROTTLE,
        );
    }

    // IOPRIO_WHO_PROCESS, and the idle class in the top three bits of the
    // value. There is no libc wrapper for this one.
    #[cfg(target_os = "linux")]
    unsafe {
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        libc::syscall(libc::SYS_ioprio_set, 1, 0, IOPRIO_CLASS_IDLE << 13);
    }
}

/// Deletes every entry of each trash directory that no other sweeper is
/// already working on.
///
/// The claim is a `flock` on the entry's own directory inode: no lock file to
/// leave behind, and the kernel drops it if we are killed, so a half-deleted
/// tree is simply resumed by whoever sweeps next.
fn sweep(ui: &Ui, trash_dirs: &[PathBuf]) -> SweepStats {
    let mut stats = SweepStats::default();
    for trash in trash_dirs {
        let Ok(entries) = std::fs::read_dir(trash) else {
            continue;
        };
        for entry in entries.flatten() {
            // Only directories are ever swept, so a stray file in the trash is
            // left for a person to look at rather than deleted.
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            match sweep_entry(&entry.path()) {
                Claim::NotOurs => stats.skipped += 1,
                Claim::Took(failed) if failed.is_empty() => stats.deleted += 1,
                Claim::Took(failed) => {
                    for path in &failed {
                        ui.warn(format!("could not remove {}", path.display()));
                    }
                    stats.failed += failed.len();
                }
            }
        }
    }
    stats
}

enum Claim {
    /// Someone else's: another sweeper holds the lock, or has already
    /// finished the entry and unlinked it between our listing and our open.
    NotOurs,
    Took(Vec<PathBuf>),
}

fn sweep_entry(path: &Path) -> Claim {
    // Opening the directory read-only is enough to flock it; the descriptor
    // closes with `dir`, which is also how the lock is released.
    let Ok(dir) = std::fs::File::open(path) else {
        return Claim::NotOurs;
    };
    let locked = unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Claim::NotOurs;
    }
    Claim::Took(delete(path))
}

/// Deletes a tree, returning the paths it could not remove. A path that is
/// already gone is success: a resumed sweep sees plenty of those.
///
/// The fast path is `remove_dir_all`. Only when that fails does the slow walk
/// run, clearing the immutable flag and restoring permissions as it goes,
/// because a cloned file on macOS inherits the lock of the file it came from.
fn delete(path: &Path) -> Vec<PathBuf> {
    if path.symlink_metadata().is_err() {
        return Vec::new();
    }
    if std::fs::remove_dir_all(path).is_ok() {
        return Vec::new();
    }
    let mut failed = Vec::new();
    remove_stubborn(path, &mut failed);
    failed
}

fn remove_stubborn(path: &Path, failed: &mut Vec<PathBuf>) {
    let Ok(metadata) = path.symlink_metadata() else {
        return;
    };

    if !metadata.is_dir() {
        if let Err(error) = std::fs::remove_file(path) {
            clear_immutable(path);
            if let Err(error) = retry(std::fs::remove_file(path), error) {
                note(path, error, failed);
            }
        }
        return;
    }

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            remove_stubborn(&entry.path(), failed);
        }
    }

    if let Err(error) = std::fs::remove_dir(path) {
        // A directory needs write and execute on itself before anything inside
        // it can be unlinked, so the children above may have failed for this.
        clear_immutable(path);
        make_writable(path);
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                remove_stubborn(&entry.path(), failed);
            }
        }
        if let Err(error) = retry(std::fs::remove_dir(path), error) {
            note(path, error, failed);
        }
    }
}

fn retry(result: io::Result<()>, first: io::Error) -> io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(again) if again.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(first),
    }
}

fn note(path: &Path, error: io::Error, failed: &mut Vec<PathBuf>) {
    if error.kind() != io::ErrorKind::NotFound {
        failed.push(path.to_path_buf());
    }
}

fn make_writable(path: &Path) {
    if let Ok(metadata) = path.symlink_metadata() {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

/// Clears `UF_IMMUTABLE`, which a file cloned from a locked one carries. No
/// equivalent exists on Linux, where the flag would need `CAP_LINUX_IMMUTABLE`
/// to have been set in the first place.
#[cfg(target_os = "macos")]
fn clear_immutable(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // `lchflags`, not `chflags`: on a symlink the latter would follow it and
    // clear the flags of a file outside the tree we are deleting.
    unsafe { darwin::lchflags(c_path.as_ptr(), 0) };
}

/// Declared here because the libc crate exposes neither. Both are documented
/// macOS system calls; the signatures come from `unistd.h` and `sys/stat.h`.
#[cfg(target_os = "macos")]
mod darwin {
    use libc::{c_char, c_int, c_uint};

    pub const IOPOL_TYPE_DISK: c_int = 0;
    pub const IOPOL_SCOPE_PROCESS: c_int = 0;
    pub const IOPOL_THROTTLE: c_int = 3;

    unsafe extern "C" {
        pub fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
        pub fn lchflags(path: *const c_char, flags: c_uint) -> c_int;
    }
}

#[cfg(not(target_os = "macos"))]
fn clear_immutable(_path: &Path) {}
