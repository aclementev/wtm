mod common;

use common::RepoBuilder;
use std::path::Path;

#[test]
fn new_prints_one_absolute_path_and_git_knows_the_worktree() {
    let repo = RepoBuilder::new("lifecycle-new").build();

    let output = repo.wtm().args(["new", "feat/login"]).output().unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "stdout was {stdout:?}");
    let path = Path::new(stdout.trim_end());
    assert!(path.is_absolute());
    assert!(path.is_dir());

    let listed = repo.git(&["worktree", "list", "--porcelain"]);
    assert!(
        listed.contains(&path.display().to_string()),
        "git does not know {path:?}:\n{listed}"
    );
    assert_eq!(repo.git_in(path, &["rev-parse", "--abbrev-ref", "HEAD"]), "feat/login");
}

#[test]
fn stdout_carries_only_the_path_even_with_debugging_on() {
    let repo = RepoBuilder::new("lifecycle-stdout").build();

    let output = repo
        .wtm()
        .env("WTM_DEBUG", "1")
        .args(["new", "noisy"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "stdout was {stdout:?}");
    assert!(Path::new(stdout.trim_end()).is_dir());
    assert!(
        !String::from_utf8_lossy(&output.stderr).is_empty(),
        "progress should still have been reported on stderr"
    );
}

#[test]
fn ls_reports_the_worktree_with_branch_base_and_age() {
    let repo = RepoBuilder::new("lifecycle-ls").build();
    repo.wtm().args(["new", "feat/login"]).assert().success();

    let head = repo.git(&["rev-parse", "HEAD"]);
    let output = repo.wtm().arg("ls").output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let line = stdout.lines().next().expect("one worktree listed");

    assert!(line.starts_with("feat/login"), "{line}");
    assert!(line.contains(&head[..7]), "base commit missing from {line}");
    assert!(line.contains("<1m"), "age missing from {line}");
    assert!(line.ends_with(&repo.worktree_path(&repo.repo_id(), "feat/login").display().to_string()));
}

#[test]
fn cd_prints_the_worktree_path_and_bare_cd_prints_the_main_worktree() {
    let repo = RepoBuilder::new("lifecycle-cd").build();
    repo.wtm().args(["new", "task"]).assert().success();

    let expected = repo.worktree_path(&repo.repo_id(), "task");
    repo.wtm()
        .args(["cd", "task"])
        .assert()
        .success()
        .stdout(format!("{}\n", expected.display()));

    repo.wtm()
        .arg("cd")
        .assert()
        .success()
        .stdout(format!("{}\n", repo.main.display()));
}

#[test]
fn rm_refuses_a_dirty_worktree_with_exit_four_and_force_removes_it() {
    let repo = RepoBuilder::new("lifecycle-rm-dirty").build();
    repo.wtm().args(["new", "task"]).assert().success();
    let path = repo.worktree_path(&repo.repo_id(), "task");
    std::fs::write(path.join("file0.txt"), "uncommitted\n").unwrap();

    repo.wtm().args(["rm", "task"]).assert().code(4);
    assert!(path.is_dir(), "a refused removal must leave the worktree");

    repo.wtm().args(["rm", "task", "--force"]).assert().success();
    assert!(!path.exists());
}

#[test]
fn rm_removes_a_clean_worktree_and_keeps_its_branch_unless_asked() {
    let repo = RepoBuilder::new("lifecycle-rm").build();
    repo.wtm().args(["new", "keep"]).assert().success();
    repo.wtm().args(["new", "drop"]).assert().success();

    repo.wtm().args(["rm", "keep"]).assert().success();
    assert!(repo.git(&["branch", "--list", "keep"]).contains("keep"));

    repo.wtm().args(["rm", "drop", "-d"]).assert().success();
    assert!(repo.git(&["branch", "--list", "drop"]).is_empty());
}

#[test]
fn rm_prunes_the_empty_directory_a_slashed_name_leaves_behind() {
    let repo = RepoBuilder::new("lifecycle-rm-nested").build();
    repo.wtm().args(["new", "feat/login"]).assert().success();

    repo.wtm().args(["rm", "feat/login"]).assert().success();
    assert!(!repo.worktree_path(&repo.repo_id(), "feat").exists());
}

#[test]
fn new_refuses_a_repository_that_is_mid_rebase_and_names_the_operation() {
    let repo = RepoBuilder::new("lifecycle-rebase").build();
    repo.write("file0.txt", "on main\n");
    repo.git(&["commit", "-qam", "on main"]);
    repo.git(&["checkout", "-q", "-b", "side", "HEAD~1"]);
    repo.write("file0.txt", "on side\n");
    repo.git(&["commit", "-qam", "on side"]);

    // A conflicting rebase stops and leaves the in-progress markers behind.
    let mut rebase = std::process::Command::new("git");
    rebase.arg("-C").arg(&repo.main).args(["rebase", "main"]);
    assert!(!rebase.output().unwrap().status.success());

    repo.wtm()
        .args(["new", "task"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("rebase is in progress"));
}

#[test]
fn new_refuses_a_branch_that_is_checked_out_elsewhere() {
    let repo = RepoBuilder::new("lifecycle-branch").build();
    repo.wtm().args(["new", "taken"]).assert().success();

    repo.wtm()
        .args(["new", "another", "--branch", "taken"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already checked out"));
}

#[test]
fn a_usage_error_exits_two_and_an_unimplemented_command_says_so() {
    let repo = RepoBuilder::new("lifecycle-exits").build();

    repo.wtm().args(["new", "bad name"]).assert().code(2);
    repo.wtm().args(["cd", "missing"]).assert().code(2);
    repo.wtm()
        .arg("gc")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("not implemented"));
}

#[test]
fn a_failed_creation_leaves_nothing_behind_and_allows_a_retry() {
    let repo = RepoBuilder::new("lifecycle-rollback").build();

    // No such base, so creation fails after the worktree has been registered.
    repo.wtm()
        .args(["new", "task", "--base", "refs/heads/does-not-exist"])
        .assert()
        .failure();

    let path = repo.worktree_path(&repo.repo_id(), "task");
    assert!(!path.exists(), "the destination survived a failed creation");
    assert!(repo.git(&["branch", "--list", "task"]).is_empty());

    repo.wtm().args(["new", "task"]).assert().success();
}

/// A bare clone with linked worktrees is the layout people reach for when
/// they live in many worktrees at once, which is exactly wtm's audience.
#[test]
fn a_bare_repository_supports_the_whole_lifecycle() {
    let seed = RepoBuilder::new("lifecycle-bare").build();
    let bare = seed.root.join("bare.git");
    seed.git_in(
        &seed.root,
        &["clone", "-q", "--bare", &seed.main.display().to_string(), &bare.display().to_string()],
    );

    let wtm = |args: &[&str]| {
        let mut command = seed.wtm();
        command.current_dir(&bare).args(args);
        command
    };

    let output = wtm(&["new", "task"]).output().unwrap();
    assert!(output.status.success());
    let path = String::from_utf8(output.stdout).unwrap().trim_end().to_string();
    assert!(Path::new(&path).is_dir());

    wtm(&["ls"])
        .assert()
        .success()
        .stdout(predicates::str::contains("task"));
    wtm(&["doctor"])
        .assert()
        .success()
        .stdout(predicates::str::contains("the repository is bare"));
    wtm(&["rm", "task"]).assert().success();
    assert!(!Path::new(&path).exists());
}

#[test]
fn rm_refuses_a_locked_worktree_and_force_removes_it() {
    let repo = RepoBuilder::new("lifecycle-locked").build();
    repo.wtm().args(["new", "pinned"]).assert().success();
    let path = repo.worktree_path(&repo.repo_id(), "pinned");
    repo.git(&["worktree", "lock", &path.display().to_string()]);

    repo.wtm()
        .args(["rm", "pinned"])
        .assert()
        .code(4)
        .stderr(predicates::str::contains("is locked"));
    assert!(path.is_dir());

    repo.wtm().args(["rm", "pinned", "--force"]).assert().success();
    assert!(!path.exists());
}

/// Git knows a worktree is gone for reasons a `stat` of the path would miss,
/// such as a broken gitdir pointer, and reports it in the same listing.
#[test]
fn ls_reports_a_worktree_whose_directory_disappeared_as_missing() {
    let repo = RepoBuilder::new("lifecycle-missing").build();
    repo.wtm().args(["new", "vanishing"]).assert().success();
    std::fs::remove_dir_all(repo.worktree_path(&repo.repo_id(), "vanishing")).unwrap();

    repo.wtm()
        .arg("ls")
        .assert()
        .success()
        .stdout(predicates::str::contains("missing"));
}
