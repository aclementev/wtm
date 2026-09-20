use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

use common::{RepoBuilder, count_entries};

fn entries_in(trash: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(trash) else {
        return Vec::new();
    };
    entries.flatten().map(|entry| entry.path()).collect()
}

/// The point of the whole feature: `rm` hands the tree to the trash and
/// returns, so what proves it is the files still sitting there afterwards.
/// A return to deleting synchronously leaves no trash entry to find.
#[test]
fn rm_returns_with_the_tree_still_whole_in_the_trash() {
    let repo = RepoBuilder::new("removal-immediate").files(2000).build();
    let id = repo.repo_id();
    repo.wtm().args(["new", "task"]).assert().success();
    let path = repo.worktree_path(&id, "task");
    let planted = count_entries(&path);

    let start = Instant::now();
    repo.wtm().args(["rm", "task"]).assert().success();
    let elapsed = start.elapsed();

    assert!(!path.exists(), "the worktree path is gone immediately");
    assert!(
        !repo.is_registered(&path),
        "git no longer lists the worktree"
    );

    let entries = entries_in(&repo.trash(&id));
    assert_eq!(entries.len(), 1, "exactly one entry in the trash");
    assert_eq!(
        count_entries(&entries[0]),
        planted,
        "the trash entry still holds every file, so nothing was deleted first"
    );

    // Loose on purpose: the assertions above are what catch a synchronous
    // removal. This only catches one so slow that no bound would forgive it.
    assert!(elapsed < Duration::from_secs(2), "rm took {elapsed:?}");
}

/// The inherited-descriptor regression. A reaper holding the caller's stdout
/// makes `$(wtm rm x)` block until the sweep finishes, so the test reads to
/// end of file and then looks at whether there was still sweeping to do.
/// With the descriptor leaked, end of file could only arrive after an empty
/// trash.
#[test]
fn command_substitution_around_rm_reaches_end_of_file_before_the_sweep_does() {
    let repo = RepoBuilder::new("removal-fd-leak").build();
    let id = repo.repo_id();
    repo.wtm().args(["new", "task"]).assert().success();
    // Enough work that a sweep cannot plausibly finish while the pipe is read.
    repo.plant_trash(&id, 4, 5000);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin("wtm"))
        .args(["rm", "task"])
        .current_dir(&repo.main)
        .env("HOME", &repo.root)
        .env("XDG_CONFIG_HOME", repo.root.join("config"))
        .env("XDG_DATA_HOME", repo.root.join("share"))
        .env("WTM_DIR", &repo.data)
        .env_remove("WTM_DEBUG")
        .env_remove("WTM_NO_REAPER")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wtm rm");

    let mut stdout = child.stdout.take().expect("a piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let result = stdout.read_to_string(&mut text);
        let _ = sender.send(result.map(|_| text));
    });

    let read = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("stdout reached end of file; a reaper is holding it open")
        .expect("read stdout");
    assert_eq!(read, "", "rm writes nothing to stdout");

    let left = entries_in(&repo.trash(&id)).len();
    assert!(
        left > 0,
        "end of file arrived only once the trash was empty, which is what a \
         leaked descriptor looks like"
    );
    child.wait().expect("reap wtm rm");
}

/// Two sweeps over one trash divide the entries between them rather than
/// both deleting everything: each entry is claimed once, so the deletions
/// the two of them report add up to exactly what was there. Without the
/// locks both sweepers claim every entry and the total comes to twice that.
///
/// The sum holds however the two are scheduled. A sweeper that arrives after
/// the other has finished reports nothing and still satisfies it.
#[test]
fn two_concurrent_sweeps_divide_the_trash_and_both_succeed() {
    const PLANTED: usize = 8;
    let repo = RepoBuilder::new("removal-concurrent").build();
    let id = repo.repo_id();
    repo.plant_trash(&id, PLANTED, 200);

    let spawn = || {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("wtm"));
        command
            .args(["gc", "--wait"])
            .current_dir(&repo.main)
            .env("HOME", &repo.root)
            .env("XDG_CONFIG_HOME", repo.root.join("config"))
            .env("XDG_DATA_HOME", repo.root.join("share"))
            .env("WTM_DIR", &repo.data)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command.spawn().expect("spawn wtm gc")
    };

    let (first, second) = (spawn(), spawn());
    let (first, second) = (
        first.wait_with_output().expect("reap the first sweep"),
        second.wait_with_output().expect("reap the second sweep"),
    );

    assert!(first.status.success(), "the first sweep exits 0");
    assert!(second.status.success(), "the second sweep exits 0");
    assert_eq!(
        swept(&first) + swept(&second),
        PLANTED,
        "each entry was deleted by exactly one of the two sweeps"
    );
    assert!(
        entries_in(&repo.trash(&id)).is_empty(),
        "the trash is empty"
    );
}

/// The count a sweep reports on stderr, from `swept <n> entries`.
fn swept(output: &std::process::Output) -> usize {
    let text = String::from_utf8_lossy(&output.stderr);
    let (_, rest) = text
        .split_once("swept ")
        .expect("a sweep reports its count");
    let (count, _) = rest.split_once(' ').expect("a count then a word");
    count.parse().expect("the count is a number")
}

/// A sweeper killed mid-delete holds no lock afterwards, because the lock
/// lives on the entry's inode and the kernel drops it with the descriptor.
/// The next sweep picks the half-deleted tree up where it was left.
#[test]
fn a_killed_sweeper_leaves_a_lock_the_next_sweep_can_take() {
    let repo = RepoBuilder::new("removal-resume").build();
    let id = repo.repo_id();
    repo.plant_trash(&id, 1, 15000);
    let trash = repo.trash(&id);
    let entry = entries_in(&trash).remove(0);
    let before = count_entries(&entry);

    let mut sweeper = Command::new(assert_cmd::cargo::cargo_bin("wtm"))
        .args(["gc", "--wait"])
        .current_dir(&repo.main)
        .env("HOME", &repo.root)
        .env("XDG_CONFIG_HOME", repo.root.join("config"))
        .env("XDG_DATA_HOME", repo.root.join("share"))
        .env("WTM_DIR", &repo.data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the sweeper");

    std::thread::sleep(Duration::from_millis(150));
    sweeper.kill().expect("kill the sweeper");
    sweeper.wait().expect("reap the sweeper");

    assert!(entry.is_dir(), "the kill landed mid-delete, not after it");
    assert!(
        count_entries(&entry) < before,
        "the killed sweeper had started deleting"
    );

    repo.wtm().args(["gc", "--wait"]).assert().success();
    assert!(
        entries_in(&trash).is_empty(),
        "a second sweep finished what the killed one started"
    );
}

/// When the tree cannot be renamed into the trash, removal deletes it where
/// it stands and still succeeds. The failure is a real one at the real
/// boundary: a file sitting where the trash directory has to be created.
#[test]
fn an_unusable_trash_falls_back_to_deleting_in_place() {
    let repo = RepoBuilder::new("removal-fallback").build();
    let id = repo.repo_id();
    repo.wtm().args(["new", "task"]).assert().success();
    let path = repo.worktree_path(&id, "task");

    let trash = repo.trash(&id);
    std::fs::create_dir_all(trash.parent().unwrap()).unwrap();
    std::fs::write(&trash, "not a directory").unwrap();

    repo.wtm().args(["rm", "task"]).assert().success();

    assert!(!path.exists(), "the worktree was deleted in place");
    assert!(
        !repo.is_registered(&path),
        "git no longer lists the worktree"
    );
    assert!(trash.is_file(), "the blocking file was left alone");
}

/// `--wait` does the unlinking before returning, so nothing is left for a
/// sweep. Compared against the default, which leaves exactly one entry.
#[test]
fn wait_removes_the_tree_instead_of_trashing_it() {
    let repo = RepoBuilder::new("removal-wait").build();
    let id = repo.repo_id();
    repo.wtm().args(["new", "patient"]).assert().success();
    repo.wtm().args(["new", "hasty"]).assert().success();

    repo.wtm()
        .args(["rm", "patient", "--wait"])
        .assert()
        .success();
    assert!(
        entries_in(&repo.trash(&id)).is_empty(),
        "--wait leaves nothing behind"
    );

    repo.wtm().args(["rm", "hasty"]).assert().success();
    assert_eq!(
        entries_in(&repo.trash(&id)).len(),
        1,
        "the same removal without --wait does leave an entry"
    );
}

/// Nobody has to run `wtm gc`. Any command finding a non-empty trash hands
/// it to a reaper, so the bytes come back whether or not the user asks.
/// `wtm ls` reads nothing and removes nothing, which makes it the plainest
/// demonstration that the sweep does not ride on removal.
#[test]
fn any_command_collects_a_trash_someone_left_behind() {
    let repo = RepoBuilder::new("removal-opportunistic").build();
    let id = repo.repo_id();
    repo.plant_trash(&id, 2, 50);
    let trash = repo.trash(&id);
    assert_eq!(entries_in(&trash).len(), 2, "the trash starts populated");

    repo.wtm_reaping().arg("ls").assert().success();

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !entries_in(&trash).is_empty() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        entries_in(&trash).is_empty(),
        "a reaper spawned by `ls` emptied the trash"
    );
}

/// The `EXDEV` case specifically: a trash that is genuinely on another
/// filesystem, which no amount of arranging can produce on a machine with
/// only one. `WTM_TEST_XDEV_DIR` names a directory on a second filesystem;
/// without it this reports that it did not run rather than passing quietly.
///
/// `an_unusable_trash_falls_back_to_deleting_in_place` covers the same
/// fallback everywhere. This one proves the boundary it was designed for.
#[test]
fn a_trash_on_another_filesystem_falls_back_to_deleting_in_place() {
    let Some(elsewhere) = std::env::var_os("WTM_TEST_XDEV_DIR").map(std::path::PathBuf::from)
    else {
        eprintln!("skipped: set WTM_TEST_XDEV_DIR to a directory on another filesystem");
        return;
    };

    let repo = RepoBuilder::new("removal-xdev").build();
    let data = elsewhere.join(format!("wtm-xdev-{}", std::process::id()));
    std::fs::create_dir_all(&data).expect("create the data root");

    let device = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map(|m| m.dev()).expect("stat")
    };
    assert_ne!(
        device(&repo.main),
        device(&data),
        "WTM_TEST_XDEV_DIR is on the same filesystem as the repository"
    );

    let wtm = |args: &[&str]| {
        let mut command = repo.wtm();
        command.env("WTM_DIR", &data).args(args);
        command
    };
    let path = String::from_utf8(wtm(&["new", "task"]).output().unwrap().stdout)
        .expect("a path")
        .trim_end()
        .to_string();

    wtm(&["rm", "task"]).assert().success();

    assert!(
        !Path::new(&path).exists(),
        "the worktree was deleted in place"
    );
    std::fs::remove_dir_all(&data).ok();
}
