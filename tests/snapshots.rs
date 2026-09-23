mod common;

use std::path::PathBuf;
use std::process::Command;

use clap::CommandFactory;

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

/// The skill is written by hand, so a renamed flag or command would leave it
/// teaching something that no longer exists. Every one it names in code is
/// checked against the definitions `--help` is built from.
#[test]
fn the_agent_skill_names_only_commands_and_flags_that_exist() {
    let mut cli = wtm::cli::Cli::command();
    cli.build();
    let commands: Vec<&str> = cli.get_subcommands().map(|c| c.get_name()).collect();
    let flags: Vec<&str> = std::iter::once(&cli)
        .chain(cli.get_subcommands())
        .flat_map(|c| c.get_arguments().filter_map(|a| a.get_long()))
        .collect();

    let skill = run(&["agent", "skill"]);
    // Splitting on backticks puts every inline span and fenced block at an
    // odd index, which is where commands and flags are written.
    for code in skill.split('`').skip(1).step_by(2) {
        let words: Vec<&str> = code.split_whitespace().collect();
        for pair in words.windows(2) {
            let program = pair[0].trim_start_matches(|c: char| !c.is_alphanumeric());
            let command = pair[1].trim_end_matches(|c: char| !c.is_alphanumeric());
            if program == "wtm" && command.starts_with(|c: char| c.is_ascii_lowercase()) {
                assert!(
                    commands.contains(&command),
                    "the skill names `wtm {command}`"
                );
            }
        }
        for word in &words {
            if let Some(flag) = word.strip_prefix("--") {
                let flag = flag.trim_end_matches(|c: char| !c.is_alphanumeric());
                assert!(flags.contains(&flag), "the skill names `--{flag}`");
            }
        }
    }
}

/// Agents that follow the Agent Skills format refuse a skill whose front
/// matter breaks its limits, and the description is the part most often
/// edited.
#[test]
fn the_agent_skill_front_matter_meets_the_agent_skills_limits() {
    let skill = run(&["agent", "skill"]);
    let front = skill
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(front, _)| front)
        .expect("front matter between --- lines");
    let field = |key: &str| {
        front
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}: ")))
            .unwrap_or_default()
    };

    let name = field("name");
    let valid_name = !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    assert!(valid_name, "name {name:?}");
    assert!((1..=1024).contains(&field("description").len()));
    assert!(field("compatibility").len() <= 500);
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
