mod common;

use common::RepoBuilder;
use std::collections::BTreeSet;
use std::path::Path;

/// Records what the hook was given, inside the worktree so a test can read it
/// back once `wtm` has returned.
const RECORD: &str =
    "#!/bin/sh\nenv | grep '^WTM_HOOK_' | sort > hook-env.txt\npwd > hook-pwd.txt\n";

fn variables(worktree: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(worktree.join("hook-env.txt")).expect("the hook ran");
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn the_hook_sees_every_variable_and_runs_inside_the_new_worktree() {
    let repo = RepoBuilder::new("hook-env").build();
    repo.executable("wtm-init.sh", RECORD);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "add the hook"]);
    let head = repo.git(&["rev-parse", "HEAD"]);

    repo.wtm().args(["new", "feat/login"]).assert().success();
    let worktree = repo.worktree_path(&repo.repo_id(), "feat/login");
    let seen: Vec<(String, String)> = variables(&worktree);
    let value = |key: &str| {
        seen.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{key} was not exported; got {seen:?}"))
    };

    assert_eq!(value("WTM_HOOK_ROOT"), worktree.display().to_string());
    assert_eq!(value("WTM_HOOK_NAME"), "feat/login");
    assert_eq!(value("WTM_HOOK_BRANCH"), "feat/login");
    assert_eq!(value("WTM_HOOK_BASE_REF"), "origin/HEAD");
    assert_eq!(value("WTM_HOOK_BASE_SHA"), head);
    assert_eq!(value("WTM_HOOK_MAIN"), repo.main.display().to_string());
    assert_eq!(value("WTM_HOOK_REPO_ID"), repo.repo_id());
    // Which one it is depends on the filesystem the tests run on, so this
    // asserts only that a real method was named. The spelling is pinned in
    // the clone tests, which first establish that cloning works here.
    let method = value("WTM_HOOK_METHOD");
    assert!(
        matches!(method.as_str(), "cow" | "checkout"),
        "WTM_HOOK_METHOD was {method:?}"
    );
    assert_eq!(seen.len(), 8, "no variable beyond the contract: {seen:?}");

    let cwd = std::fs::read_to_string(worktree.join("hook-pwd.txt")).unwrap();
    assert_eq!(Path::new(cwd.trim_end()), worktree);
}

/// A hook runs git, so an inherited `GIT_DIR` would aim it at whatever the
/// caller's shell had set rather than the worktree it was handed.
#[test]
fn the_hook_does_not_inherit_the_callers_git_variables() {
    let repo = RepoBuilder::new("hook-scrub").build();
    repo.executable(
        "wtm-init.sh",
        "#!/bin/sh\nenv | grep -E '^GIT_(DIR|WORK_TREE|INDEX_FILE|COMMON_DIR|OBJECT_DIRECTORY)=' \
> git-env.txt\ntrue\n",
    );

    repo.wtm()
        .env("GIT_DIR", "/somewhere/else/.git")
        .env("GIT_WORK_TREE", "/somewhere/else")
        .args(["new", "task"])
        .assert()
        .success();

    let leaked = std::fs::read_to_string(
        repo.worktree_path(&repo.repo_id(), "task")
            .join("git-env.txt"),
    )
    .expect("the hook ran");
    assert!(leaked.trim().is_empty(), "the hook inherited {leaked:?}");
}

#[test]
fn a_failing_hook_keeps_a_usable_worktree_and_exits_three() {
    let repo = RepoBuilder::new("hook-fail").build();
    repo.executable(
        "wtm-init.sh",
        "#!/bin/sh\necho 'noise on stdout'\necho 'the reason' >&2\nexit 7\n",
    );

    let output = repo.wtm().args(["new", "task"]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));

    // The path alone, even though the hook wrote to its own stdout.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let worktree = repo.worktree_path(&repo.repo_id(), "task");
    assert_eq!(stdout, format!("{}\n", worktree.display()));

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("noise on stdout"), "{stderr}");
    assert!(stderr.contains("the reason"), "{stderr}");
    assert!(stderr.contains("exit 7"), "{stderr}");
    assert!(stderr.contains("wtm init task"), "the rerun hint: {stderr}");

    // The worktree is not a casualty. Git knows it, and it is checked out.
    assert_eq!(
        repo.git_in(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "task"
    );
    assert!(worktree.join("file0.txt").is_file());

    repo.executable("wtm-init.sh", "#!/bin/sh\ntrue\n");
    repo.wtm().args(["init", "task"]).assert().success();
}

/// `--quiet` covers the hook's output but never the reason for an exit code,
/// which would otherwise leave an unexplainable 3.
#[test]
fn quiet_discards_the_hooks_output_but_not_its_failure() {
    let repo = RepoBuilder::new("hook-quiet").build();
    repo.executable(
        "wtm-init.sh",
        "#!/bin/sh\necho chatter\necho chatter >&2\nexit 4\n",
    );

    let output = repo
        .wtm()
        .args(["--quiet", "new", "task"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("chatter"), "{stderr}");
    assert!(stderr.contains("exit 4"), "{stderr}");
    assert!(stderr.contains("wtm init task"), "{stderr}");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout,
        format!(
            "{}\n",
            repo.worktree_path(&repo.repo_id(), "task").display()
        )
    );
}

/// wtm reports the outcome in band and nowhere else, so a worktree whose
/// hook failed must be indistinguishable on disk from one whose hook passed.
/// Comparing the two catches any status file a later change might add.
#[test]
fn a_hooks_outcome_is_never_written_down() {
    let repo = RepoBuilder::new("hook-stateless").build();
    let metadata = |name: &str| -> BTreeSet<String> {
        let gitdir = repo.git_in(
            &repo.worktree_path(&repo.repo_id(), name),
            &["rev-parse", "--git-dir"],
        );
        std::fs::read_dir(&gitdir)
            .expect("the worktree has git metadata")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };

    repo.executable("wtm-init.sh", "#!/bin/sh\ntrue\n");
    repo.wtm().args(["new", "passed"]).assert().success();

    repo.executable("wtm-init.sh", "#!/bin/sh\nexit 1\n");
    repo.wtm().args(["new", "failed"]).assert().code(3);

    assert_eq!(metadata("passed"), metadata("failed"));
}

#[test]
fn a_missing_default_hook_is_silent_but_a_configured_one_is_refused() {
    let repo = RepoBuilder::new("hook-missing").build();

    // Nothing configured and no wtm-init.sh, so not worth a word.
    let quiet = repo.wtm().args(["new", "nohook"]).output().unwrap();
    assert!(quiet.status.success());
    assert!(
        !String::from_utf8_lossy(&quiet.stderr).contains("init"),
        "an absent default hook was mentioned: {:?}",
        String::from_utf8_lossy(&quiet.stderr)
    );

    let refused = repo
        .wtm()
        .args(["new", "typo", "--init", "setup-typo.sh"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("setup-typo.sh"), "{stderr}");
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(
        stderr.contains("flag"),
        "the layer that set it is named: {stderr}"
    );
    assert!(
        !repo.worktree_path(&repo.repo_id(), "typo").exists(),
        "a hook that could never run must not cost a worktree"
    );
}

#[test]
fn a_hook_that_is_not_executable_is_refused_before_anything_is_created() {
    let repo = RepoBuilder::new("hook-perm").build();
    repo.write("wtm-init.sh", "#!/bin/sh\ntrue\n");

    let output = repo.wtm().args(["new", "task"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is not executable"), "{stderr}");
    assert!(!repo.worktree_path(&repo.repo_id(), "task").exists());
}

#[test]
fn no_init_skips_a_hook_that_would_have_failed() {
    let repo = RepoBuilder::new("hook-skip").build();
    repo.executable("wtm-init.sh", "#!/bin/sh\nexit 9\n");

    repo.wtm()
        .args(["new", "task", "--no-init"])
        .assert()
        .success();
}

#[test]
fn init_reruns_in_the_current_worktree_and_refuses_outside_one() {
    let repo = RepoBuilder::new("hook-rerun").build();
    repo.wtm().args(["new", "task"]).assert().success();
    let worktree = repo.worktree_path(&repo.repo_id(), "task");

    // An explicit rerun with no hook anywhere says so, rather than looking
    // like it did something.
    let nothing = repo
        .wtm()
        .current_dir(&worktree)
        .arg("init")
        .output()
        .unwrap();
    assert!(nothing.status.success());
    assert!(
        String::from_utf8_lossy(&nothing.stderr).contains("nothing to run"),
        "{:?}",
        String::from_utf8_lossy(&nothing.stderr)
    );

    repo.executable("wtm-init.sh", RECORD);
    repo.wtm()
        .current_dir(&worktree)
        .arg("init")
        .assert()
        .success();

    let seen = variables(&worktree);
    let value = |key: &str| {
        seen.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(value("WTM_HOOK_NAME"), "task");
    assert_eq!(value("WTM_HOOK_BRANCH"), "task");
    assert_eq!(value("WTM_HOOK_BASE_SHA"), repo.git(&["rev-parse", "HEAD"]));
    assert_eq!(
        value("WTM_HOOK_METHOD"),
        "",
        "a rerun cannot know the method"
    );

    let outside = repo.wtm().arg("init").output().unwrap();
    assert_eq!(outside.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&outside.stderr).contains("not inside a wtm worktree"),
        "{:?}",
        String::from_utf8_lossy(&outside.stderr)
    );
}
