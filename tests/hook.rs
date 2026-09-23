mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::RepoBuilder;

/// Records what the hook was given, inside the worktree so a test can read it
/// back once `wtm` has returned.
const RECORD: &str =
    "#!/bin/sh\nenv | grep '^WTM_HOOK_' | sort > hook-env.txt\npwd > hook-pwd.txt\n";

fn variable(worktree: &Path, key: &str) -> String {
    let text = std::fs::read_to_string(worktree.join("hook-env.txt")).expect("the hook ran");
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("{key} was not exported:\n{text}"))
        .to_string()
}

#[test]
fn the_hook_runs_inside_the_new_worktree_with_its_variables() {
    let repo = RepoBuilder::new("hook-env").build();
    repo.executable("wtm-init.sh", RECORD);
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "add the hook"]);
    let head = repo.git(&["rev-parse", "HEAD"]);

    let worktree = repo.new_worktree(&["feat/login"]);

    assert_eq!(
        variable(&worktree, "WTM_HOOK_ROOT"),
        worktree.display().to_string()
    );
    assert_eq!(variable(&worktree, "WTM_HOOK_NAME"), "feat/login");
    assert_eq!(variable(&worktree, "WTM_HOOK_BRANCH"), "feat/login");
    assert_eq!(variable(&worktree, "WTM_HOOK_BASE_SHA"), head);
    assert_eq!(
        variable(&worktree, "WTM_HOOK_MAIN"),
        repo.main.display().to_string()
    );
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

    let output = repo
        .wtm()
        .env("GIT_DIR", "/somewhere/else/.git")
        .env("GIT_WORK_TREE", "/somewhere/else")
        .args(["new", "task"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let worktree = String::from_utf8(output.stdout).unwrap();
    let leaked = std::fs::read_to_string(Path::new(worktree.trim_end()).join("git-env.txt"))
        .expect("the hook ran");
    assert!(leaked.trim().is_empty(), "the hook inherited {leaked:?}");
}

/// A failed hook keeps the worktree and still prints its path, so the exit
/// code is the only sign. The hook's own output goes to stderr, and
/// `--quiet` discards it.
#[test]
fn a_failing_hook_keeps_a_usable_worktree_and_exits_three() {
    let repo = RepoBuilder::new("hook-fail").build();
    repo.executable(
        "wtm-init.sh",
        "#!/bin/sh\necho 'on stdout'\necho 'on stderr' >&2\nexit 7\n",
    );

    for (name, quiet) in [("loud", false), ("quiet", true)] {
        let mut command = repo.wtm();
        if quiet {
            command.arg("--quiet");
        }
        let output = command.args(["new", name]).output().unwrap();
        assert_eq!(output.status.code(), Some(3), "{name}");

        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.lines().count(), 1, "{name}: {stdout:?}");
        let worktree = Path::new(stdout.trim_end());
        assert_eq!(repo.git_in(worktree, &["branch", "--show-current"]), name);

        let stderr = String::from_utf8(output.stderr).unwrap();
        for line in ["on stdout", "on stderr"] {
            assert_eq!(stderr.contains(line), !quiet, "{name}: {stderr}");
        }
    }

    repo.wtm()
        .args(["new", "skipped", "--no-init"])
        .assert()
        .success();
    repo.executable("wtm-init.sh", "#!/bin/sh\ntrue\n");
    repo.wtm().args(["init", "loud"]).assert().success();
}

/// A hook that could never run is refused before anything is made, so a
/// mistyped path costs no worktree. An absent default is the exception:
/// most repositories have no hook at all.
#[test]
fn a_hook_that_cannot_run_is_refused_before_anything_is_made() {
    let repo = RepoBuilder::new("hook-unusable").build();
    repo.new_worktree(&["no-hook"]);

    repo.wtm()
        .args(["new", "typo", "--init", "setup-typo.sh"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("setup-typo.sh"));
    repo.write("wtm-init.sh", "#!/bin/sh\ntrue\n");
    repo.wtm().args(["new", "not-executable"]).assert().code(2);

    let listed = common::listing(&repo);
    assert_eq!(listed.len(), 1, "{listed:?}");
}

/// A hook may prompt a person at a terminal, but under an agent or CI
/// nobody answers, so it must read end of file rather than wait. The caller
/// here holds a pipe open on stdin and never writes to it.
#[test]
fn a_hook_reading_stdin_does_not_wait_when_no_terminal_is_attached() {
    let repo = RepoBuilder::new("hook-stdin").build();
    repo.executable("wtm-init.sh", "#!/bin/sh\ncat > stdin.txt\n");

    let mut child = repo
        .process()
        .env("WTM_NO_REAPER", "1")
        .args(["new", "task"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _held_open = child.stdin.take();

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.is_some_and(|s| s.success()),
        "wtm new waited on the hook reading stdin"
    );
}

#[test]
fn init_reruns_the_hook_in_the_current_worktree_and_refuses_outside_one() {
    let repo = RepoBuilder::new("hook-rerun").build();
    let worktree = repo.new_worktree(&["task"]);
    repo.executable("wtm-init.sh", RECORD);

    repo.wtm()
        .current_dir(&worktree)
        .arg("init")
        .assert()
        .success();
    assert_eq!(variable(&worktree, "WTM_HOOK_NAME"), "task");
    assert_eq!(variable(&worktree, "WTM_HOOK_BRANCH"), "task");
    assert_eq!(
        variable(&worktree, "WTM_HOOK_BASE_SHA"),
        repo.git(&["rev-parse", "HEAD"])
    );

    repo.wtm().arg("init").assert().code(2);
}
