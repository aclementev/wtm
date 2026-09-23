mod common;

use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{RepoBuilder, count_entries, entries_in};

/// The point of the whole feature: `rm` hands the tree to the trash and
/// returns, so what proves it is the files still sitting there afterwards.
/// `--wait` is the comparison. It deletes before returning and leaves no
/// entry.
#[test]
fn rm_returns_with_the_tree_still_whole_in_the_trash() {
    let repo = RepoBuilder::new("removal-immediate").files(2000).build();
    let path = repo.new_worktree(&["task"]);
    let planted = count_entries(&path);
    let trash = repo.trash();

    let start = Instant::now();
    repo.wtm().args(["rm", "task"]).assert().success();
    let elapsed = start.elapsed();

    assert!(!path.exists());
    assert!(!repo.is_registered(&path));
    let entries = entries_in(&trash);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        count_entries(&entries[0]),
        planted,
        "the trash entry still holds every file, so nothing was deleted first"
    );
    // Loose on purpose: the entry above is what catches a synchronous
    // removal. This only catches one so slow that no bound would forgive it.
    assert!(elapsed < Duration::from_secs(2), "rm took {elapsed:?}");

    let waited = repo.new_worktree(&["waited"]);
    repo.wtm()
        .args(["rm", "waited", "--wait"])
        .assert()
        .success();
    assert!(!waited.exists());
    assert_eq!(entries_in(&trash).len(), 1, "--wait added a trash entry");
}

/// The inherited-descriptor regression. A reaper holding the caller's stdout
/// makes `$(wtm rm x)` block until the sweep finishes, so the test reads to
/// end of file and then looks at whether there was still sweeping to do.
/// With the descriptor leaked, end of file could only arrive after an empty
/// trash.
#[test]
fn command_substitution_around_rm_reaches_end_of_file_before_the_sweep_does() {
    let repo = RepoBuilder::new("removal-fd-leak").build();
    repo.new_worktree(&["task"]);
    // Enough work that a sweep cannot plausibly finish while the pipe is read.
    repo.plant_trash(4, 5000);

    let mut child = repo
        .process()
        .args(["rm", "task"])
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
    assert!(
        !entries_in(&repo.trash()).is_empty(),
        "end of file arrived only once the trash was empty, which is what a \
         leaked descriptor looks like"
    );
    child.wait().expect("reap wtm rm");
}

/// Several agents in one repository start several sweeps over the same
/// trash. They must all succeed and leave it empty, however they interleave.
#[test]
fn concurrent_sweeps_all_succeed_and_empty_the_trash() {
    let repo = RepoBuilder::new("removal-concurrent").build();
    repo.plant_trash(8, 200);

    let sweeps: Vec<_> = (0..3)
        .map(|_| {
            repo.process()
                .args(["gc", "--wait"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn wtm gc")
        })
        .collect();
    for sweep in sweeps {
        let output = sweep.wait_with_output().expect("reap a sweep");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(entries_in(&repo.trash()).is_empty());
}

/// A sweeper killed mid-delete holds no lock afterwards, because the lock
/// lives on the entry's inode and the kernel drops it with the descriptor.
/// The next sweep picks the half-deleted tree up where it was left.
#[test]
fn a_killed_sweeper_leaves_work_the_next_sweep_finishes() {
    let repo = RepoBuilder::new("removal-resume").build();
    repo.plant_trash(1, 15000);
    let trash = repo.trash();
    let entry = entries_in(&trash).remove(0);
    let before = count_entries(&entry);

    let mut sweeper = repo
        .process()
        .args(["gc", "--wait"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the sweeper");

    // A fixed delay is either too short to see the sweep start or long
    // enough for a fast filesystem to finish it. The entry holds twenty
    // subdirectories, so the first one gone means deleting is under way
    // with most of the tree still left.
    while std::fs::read_dir(&entry).map_or(0, |e| e.count()) == 20 {
        assert!(
            sweeper.try_wait().expect("poll the sweeper").is_none(),
            "the sweeper exited before anything was seen deleted"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    sweeper.kill().expect("kill the sweeper");
    sweeper.wait().expect("reap the sweeper");

    assert!(entry.is_dir(), "the kill landed mid-delete, not after it");
    assert!(count_entries(&entry) < before);

    repo.wtm().args(["gc", "--wait"]).assert().success();
    assert!(entries_in(&trash).is_empty());
}

/// When the tree cannot be renamed into the trash, removal deletes it where
/// it stands and still succeeds. The failure is a real one at the real
/// boundary: a file sitting where the trash directory has to be created.
#[test]
fn an_unusable_trash_falls_back_to_deleting_in_place() {
    let repo = RepoBuilder::new("removal-fallback").build();
    let path = repo.new_worktree(&["task"]);
    let trash = repo.trash();
    std::fs::write(&trash, "not a directory").unwrap();

    repo.wtm().args(["rm", "task"]).assert().success();

    assert!(!path.exists());
    assert!(!repo.is_registered(&path));
    assert!(trash.is_file(), "the blocking file was left alone");
}

/// Nobody has to run `wtm gc`. Any command finding a non-empty trash hands
/// it to a reaper. `wtm ls` removes nothing, which makes it the plainest
/// demonstration that the sweep does not ride on removal.
#[test]
fn any_command_collects_a_trash_someone_left_behind() {
    let repo = RepoBuilder::new("removal-opportunistic").build();
    repo.plant_trash(2, 50);
    let trash = repo.trash();

    repo.wtm_reaping().arg("ls").assert().success();

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !entries_in(&trash).is_empty() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(entries_in(&trash).is_empty());
}
