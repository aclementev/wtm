mod common;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use common::{RepoBuilder, TestRepo, listing};
use predicates::str::contains;

fn commit_in(repo: &TestRepo, worktree: &Path, file: &str) {
    std::fs::write(worktree.join(file), "work\n").unwrap();
    repo.git_in(worktree, &["add", file]);
    repo.git_in(worktree, &["commit", "-q", "-m", file]);
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Stdout is the contract `cd "$(wtm new x)"` relies on, so it is checked
/// with debugging on, the setting most likely to leak something onto it.
#[test]
fn new_prints_only_the_path_of_a_worktree_git_knows() {
    let repo = RepoBuilder::new("lifecycle-new").build();

    let output = repo
        .wtm()
        .env("WTM_DEBUG", "1")
        .args(["new", "feat/login"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "stdout was {stdout:?}");
    let path = Path::new(stdout.trim_end());
    assert!(path.is_absolute() && path.is_dir());
    assert!(repo.is_registered(path));
    assert_eq!(
        repo.git_in(path, &["branch", "--show-current"]),
        "feat/login"
    );
}

/// Nothing `ls` shows is stored. Each field is derived from git or the
/// filesystem, and git is the oracle for each.
#[test]
fn ls_derives_branch_base_age_and_status() {
    let repo = RepoBuilder::new("lifecycle-ls").build();
    let first = repo.git(&["rev-parse", "HEAD"]);
    repo.git(&["tag", "v1"]);
    repo.write("file0.txt", "second commit\n");
    repo.git(&["commit", "-qam", "second"]);
    let second = repo.git(&["rev-parse", "HEAD"]);

    let before = now();
    let made = [
        ("feat/login", repo.new_worktree(&["feat/login"]), &second),
        (
            "from-tag",
            repo.new_worktree(&["from-tag", "--base", "v1"]),
            &first,
        ),
        (
            "from-sha",
            repo.new_worktree(&["from-sha", "--base", &first]),
            &first,
        ),
    ];
    let after = now();
    // The base is where the branch left the default branch, not its head.
    commit_in(&repo, &made[1].1, "ahead.txt");

    let listed = listing(&repo);
    assert_eq!(listed.len(), made.len());
    for (name, path, base) in &made {
        let entry = listed
            .iter()
            .find(|e| e["name"] == *name)
            .unwrap_or_else(|| panic!("{name} is not listed: {listed:?}"));
        assert_eq!(entry["branch"], *name);
        assert_eq!(entry["path"], path.display().to_string());
        assert_eq!(entry["base"], **base, "{name}");
        let created = entry["created"].as_u64().expect("a creation time");
        assert!(before <= created + 1 && created <= after + 1, "{name}");
        assert!(entry["status"].is_null(), "{name}");
    }

    repo.git(&["worktree", "lock", &made[0].1.display().to_string()]);
    std::fs::remove_dir_all(&made[2].1).unwrap();
    let listed = listing(&repo);
    let status = |name: &str| listed.iter().find(|e| e["name"] == name).unwrap()["status"].clone();
    assert_eq!(status("feat/login"), "locked");
    assert_eq!(status("from-sha"), "missing");

    let text = repo.wtm().arg("ls").output().unwrap();
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        made.iter().all(|(name, _, _)| text.contains(name)),
        "{text}"
    );
}

/// The branch starts at the remote's default branch, so commits that exist
/// only in the main checkout stay out unless asked for, and `--fetch` sees
/// what was pushed since the last fetch.
#[test]
fn new_starts_from_the_remote_default_branch() {
    let origin = RepoBuilder::new("lifecycle-origin").build();
    let clone = TestRepo::clone_of(&origin, "lifecycle-clone");
    clone.write("unpushed.txt", "local only\n");
    clone.git(&["add", "unpushed.txt"]);
    clone.git(&["commit", "-q", "-m", "unpushed"]);
    origin.write("pushed.txt", "pushed by someone else\n");
    origin.git(&["add", "pushed.txt"]);
    origin.git(&["commit", "-q", "-m", "pushed"]);

    let remote = clone.new_worktree(&["remote"]);
    assert!(!remote.join("unpushed.txt").exists());

    let fetched = clone.new_worktree(&["fetched", "--fetch"]);
    assert!(fetched.join("pushed.txt").is_file());
    assert!(!fetched.join("unpushed.txt").exists());

    let local = clone.new_worktree(&["local", "--base", "HEAD"]);
    assert!(local.join("unpushed.txt").is_file());
}

#[test]
fn cd_prints_a_worktree_path_or_the_main_worktree() {
    let repo = RepoBuilder::new("lifecycle-cd").build();
    let path = repo.new_worktree(&["task"]);

    repo.wtm()
        .args(["cd", "task"])
        .assert()
        .success()
        .stdout(format!("{}\n", path.display()));
    repo.wtm()
        .arg("cd")
        .assert()
        .success()
        .stdout(format!("{}\n", repo.main.display()));
}

#[test]
fn rm_refuses_uncommitted_locked_and_current_worktrees() {
    let repo = RepoBuilder::new("lifecycle-rm-refuse").build();
    let dirty = repo.new_worktree(&["dirty"]);
    std::fs::write(dirty.join("file0.txt"), "uncommitted\n").unwrap();
    let locked = repo.new_worktree(&["locked"]);
    repo.git(&["worktree", "lock", &locked.display().to_string()]);
    let here = repo.new_worktree(&["here"]);

    repo.wtm().args(["rm", "dirty"]).assert().code(4);
    repo.wtm().args(["rm", "locked"]).assert().code(4);
    repo.wtm()
        .current_dir(&here)
        .args(["rm", "here", "--force"])
        .assert()
        .code(2);
    assert!(dirty.is_dir() && locked.is_dir() && here.is_dir());

    for name in ["dirty", "locked"] {
        repo.wtm().args(["rm", name, "--force"]).assert().success();
    }
    assert!(!dirty.exists() && !locked.exists());
    // `git worktree prune` keeps a locked worktree registered however long
    // its directory has been gone, so forcing one out has to unlock it.
    assert!(!repo.is_registered(&locked));
}

/// The worktree is removed whatever happens to the branch, and an unmerged
/// branch survives `-d`, which then exits 1 because it did not do all it
/// was asked.
#[test]
fn rm_keeps_the_branch_unless_asked_and_d_keeps_unmerged_work() {
    let repo = RepoBuilder::new("lifecycle-rm-branch").build();
    let paths: Vec<PathBuf> = ["kept", "merged", "unmerged", "forced"]
        .iter()
        .map(|name| repo.new_worktree(&[name]))
        .collect();
    commit_in(&repo, &paths[2], "work.txt");
    commit_in(&repo, &paths[3], "work.txt");

    repo.wtm().args(["rm", "kept"]).assert().success();
    repo.wtm().args(["rm", "merged", "-d"]).assert().success();
    repo.wtm().args(["rm", "unmerged", "-d"]).assert().code(1);
    repo.wtm().args(["rm", "forced", "-D"]).assert().success();

    assert!(paths.iter().all(|path| !path.exists()));
    assert!(repo.branch_exists("kept"));
    assert!(!repo.branch_exists("merged"));
    assert!(repo.branch_exists("unmerged"));
    assert!(!repo.branch_exists("forced"));
}

#[test]
fn new_refuses_a_repository_mid_rebase_and_names_the_operation() {
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
        .code(1)
        .stderr(contains("rebase"));
}

#[test]
fn new_refuses_a_branch_checked_out_elsewhere_and_says_where() {
    let repo = RepoBuilder::new("lifecycle-branch").build();
    let taken = repo.new_worktree(&["taken"]);

    repo.wtm()
        .args(["new", "another", "--branch", "taken"])
        .assert()
        .code(1)
        .stderr(contains(taken.display().to_string()));
}

/// `wtm rm` keeps the branch, so `wtm new` with the same name picks the work
/// up where it was left. A `--base` typed with it is a request the existing
/// branch cannot honour, so that is refused before anything is made.
#[test]
fn new_reuses_an_existing_branch_and_refuses_an_explicit_base() {
    let repo = RepoBuilder::new("lifecycle-reuse").build();
    let path = repo.new_worktree(&["resume"]);
    commit_in(&repo, &path, "progress.txt");
    let tip = repo.git_in(&path, &["rev-parse", "HEAD"]);
    repo.wtm().args(["rm", "resume"]).assert().success();

    repo.wtm()
        .args(["new", "resume", "--base", "main"])
        .assert()
        .code(2);
    assert!(!path.exists());

    let path = repo.new_worktree(&["resume"]);
    assert_eq!(repo.git_in(&path, &["rev-parse", "HEAD"]), tip);
    assert_eq!(repo.git_in(&path, &["branch", "--show-current"]), "resume");
}

#[test]
fn usage_errors_exit_two() {
    let repo = RepoBuilder::new("lifecycle-exits").build();

    for args in [
        &["new", "bad name"][..],
        &["new", "../escape"],
        &["new", "x.lock"],
        &["cd", "missing"],
        &["rm", "never-existed"],
        &["init", "missing"],
    ] {
        repo.wtm().args(args).assert().code(2);
    }
}

/// A real failure at the last step that can fail, when the tree is fully
/// populated: an untracked file carried by `.worktreeinclude` is one the
/// base tracks. On the clone path git refuses to switch over it, and on the
/// checkout path the include copy refuses to overwrite it. Whatever failed
/// earlier left a subset of this to clean up.
#[test]
fn a_failed_creation_leaves_nothing_behind_and_allows_a_retry() {
    for mode in ["auto", "checkout"] {
        let repo = RepoBuilder::new(&format!("lifecycle-rollback-{mode}")).build();
        repo.git(&["checkout", "-q", "-b", "base"]);
        repo.write("conf.local", "tracked on base\n");
        repo.git(&["add", "conf.local"]);
        repo.git(&["commit", "-q", "-m", "track conf.local"]);
        repo.git(&["checkout", "-q", "main"]);
        repo.write(".worktreeinclude", "conf.local\n");
        repo.git(&["add", ".worktreeinclude"]);
        repo.git(&["commit", "-q", "-m", "carry conf.local"]);
        repo.write("conf.local", "untracked on main\n");
        let new = ["new", "feat/task", "--base", "base", "--clone-mode", mode];

        repo.wtm()
            .args(new)
            .assert()
            .code(1)
            .stderr(contains("conf.local"));

        assert_eq!(
            std::fs::read_dir(&repo.data).unwrap().count(),
            0,
            "{mode}: the data root kept a directory"
        );
        assert!(!repo.git(&["worktree", "list"]).contains("feat/task"));
        let metadata = std::fs::read_dir(repo.main.join(".git/worktrees")).map_or(0, |d| d.count());
        assert_eq!(metadata, 0, "{mode}: git kept metadata for the worktree");
        assert!(
            !repo.branch_exists("feat/task"),
            "{mode}: branch left behind"
        );

        std::fs::remove_file(repo.main.join("conf.local")).unwrap();
        repo.wtm().args(new).assert().success();
    }
}

/// A bare clone with linked worktrees is the layout people reach for when
/// they live in many worktrees at once, which is wtm's audience.
#[test]
fn a_bare_repository_supports_the_whole_lifecycle() {
    let seed = RepoBuilder::new("lifecycle-bare").build();
    let bare = seed.root.join("bare.git");
    seed.git_in(
        &seed.root,
        &[
            "clone",
            "-q",
            "--bare",
            &seed.main.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    let wtm = |args: &[&str]| {
        let mut command = seed.wtm();
        command.current_dir(&bare).args(args);
        command
    };

    let output = wtm(&["new", "task"]).output().unwrap();
    assert!(output.status.success());
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end());
    assert!(path.join("file0.txt").is_file());

    wtm(&["ls"]).assert().success().stdout(contains("task"));
    wtm(&["doctor"]).assert().success();
    wtm(&["rm", "task"]).assert().success();
    assert!(!path.exists());
}

/// Moving a repository changes its id, and with it where new worktrees go.
/// The ones made before the move stay where they are and are still wtm's.
#[test]
fn worktrees_made_before_the_repository_moved_are_still_managed() {
    let repo = RepoBuilder::new("lifecycle-moved").build();
    repo.new_worktree(&["before"]);
    let moved = repo.root.join("moved");
    std::fs::rename(&repo.main, &moved).unwrap();
    let wtm = |args: &[&str]| {
        let mut command = repo.wtm();
        command.current_dir(&moved).args(args);
        command
    };

    wtm(&["ls"]).assert().success().stdout(contains("before"));
    wtm(&["new", "before", "--branch", "other"])
        .assert()
        .code(2);
    // The worktree's `.git` file still names the old path, so this only
    // succeeds because `rm` repairs the link before asking git anything.
    wtm(&["rm", "before"]).assert().success();
    wtm(&["ls"]).assert().success().stdout("");
}

/// Every path under `root`, relative to it, except inside `skip`.
fn paths_under(root: &Path, skip: &[&Path]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if skip.contains(&path.as_path()) {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                pending.push(path.clone());
            }
            found.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    found.sort();
    found
}

/// wtm stores nothing. After a whole lifecycle the home directory, which
/// holds every XDG directory in these tests, is exactly as it was, and the
/// data root is empty again, including the directory a slashed name made.
#[test]
fn a_full_lifecycle_leaves_no_state_behind() {
    let repo = RepoBuilder::new("lifecycle-zero").build();
    let skip = [repo.main.as_path(), repo.data.as_path()];
    let before = paths_under(&repo.root, &skip);

    repo.new_worktree(&["feat/task"]);
    repo.wtm().args(["init", "feat/task"]).assert().success();
    repo.wtm().arg("ls").assert().success();
    repo.wtm().args(["rm", "feat/task"]).assert().success();
    repo.wtm().args(["gc", "--wait"]).assert().success();

    assert_eq!(paths_under(&repo.root, &skip), before);
    assert_eq!(paths_under(&repo.data, &[]), Vec::<PathBuf>::new());
}
