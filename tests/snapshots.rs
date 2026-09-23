mod common;

use std::path::PathBuf;
use std::process::Command;

use common::RepoBuilder;

const COMMANDS: [&str; 10] = [
    "new", "ls", "cd", "rm", "init", "gc", "doctor", "config", "shell", "agent",
];

fn wtm_binary() -> PathBuf {
    assert_cmd::cargo::cargo_bin("wtm")
}

fn run(args: &[&str]) -> String {
    let output = Command::new(wtm_binary())
        .args(args)
        .output()
        .expect("run wtm");
    String::from_utf8(output.stdout).expect("utf-8 output")
}

#[test]
fn help_text_is_reviewed_not_drifted() {
    insta::assert_snapshot!("help", run(&["--help"]));
    for command in COMMANDS {
        insta::assert_snapshot!(format!("help-{command}"), run(&[command, "--help"]));
    }
}

/// The skill is generated from the same clap definitions as `--help`, so this
/// snapshot also catches a flag whose description changed in only one place.
#[test]
fn the_agent_skill_is_reviewed_not_drifted() {
    insta::assert_snapshot!("agent-skill", run(&["agent", "skill"]));
}

#[test]
fn shell_wrappers_are_reviewed_not_drifted() {
    for shell in ["zsh", "bash", "fish"] {
        insta::assert_snapshot!(format!("shell-{shell}"), run(&["shell", shell]));
    }
}

const SHELLS: [&str; 3] = ["zsh", "bash", "fish"];

/// Wraps `body` in whatever each shell needs to define the wtm function.
/// Fish has no `$(...)`, and `string collect` keeps the multi-line function
/// text intact through its command substitution.
fn wrapper_script(shell: &str, body: &str) -> String {
    match shell {
        "fish" => format!("eval (wtm shell fish | string collect); {body}"),
        _ => format!("eval \"$(wtm shell {shell})\"; {body}"),
    }
}

fn in_shell(
    shell: &std::path::Path,
    repo: &common::TestRepo,
    script: &str,
) -> std::process::Output {
    Command::new(shell)
        .arg("-c")
        .arg(script)
        .current_dir(&repo.main)
        .env("PATH", path_with_wtm())
        .env("HOME", &repo.root)
        .env("XDG_CONFIG_HOME", repo.root.join("config"))
        .env("XDG_DATA_HOME", repo.root.join("share"))
        .env("WTM_DIR", &repo.data)
        .output()
        .expect("run the shell")
}

/// Printing a wrapper that does not work in the shell it names would be
/// invisible to a snapshot, so each one is evaluated for real.
#[test]
fn each_wrapper_changes_directory_in_its_own_shell() {
    for shell in SHELLS {
        let Some(shell_path) = which(shell) else {
            panic!("{shell} is not installed; the wrapper it prints is untested");
        };

        let repo = RepoBuilder::new(&format!("wrapper-{shell}")).build();
        let output = in_shell(
            &shell_path,
            &repo,
            &wrapper_script(shell, "wtm new task >/dev/null; pwd"),
        );

        let printed = String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string();
        let expected = repo.worktree_path(&repo.repo_id(), "task");
        assert_eq!(
            printed,
            expected.display().to_string(),
            "{shell} wrapper did not change directory; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// The wrapper must return the binary's exit code. It is a shell function,
/// so a careless one returns the status of its last command, a successful
/// `printf` or `cd`, and every failure then looks like success.
#[test]
fn each_wrapper_returns_the_exit_code_of_the_binary() {
    for shell in SHELLS {
        let Some(shell_path) = which(shell) else {
            panic!("{shell} is not installed; the wrapper it prints is untested");
        };

        let repo = RepoBuilder::new(&format!("wrapper-rc-{shell}")).build();
        let status = if shell == "fish" { "$status" } else { "$?" };
        let output = in_shell(
            &shell_path,
            &repo,
            &wrapper_script(shell, &format!("wtm cd nope; echo rc={status}")),
        );

        assert!(
            String::from_utf8_lossy(&output.stdout).contains("rc=2"),
            "{shell} wrapper lost the exit code; stdout was {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

fn path_with_wtm() -> String {
    let dir = wtm_binary().parent().unwrap().display().to_string();
    match std::env::var("PATH") {
        Ok(rest) => format!("{dir}:{rest}"),
        Err(_) => dir,
    }
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(|dir| PathBuf::from(dir).join(program))
        .find(|candidate| candidate.is_file())
}
