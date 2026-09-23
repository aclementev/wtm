//! `wtm`, a git worktree manager for very large repositories. The binary
//! is a thin `main` over [`run`], so the tests reach everything through
//! this library.
//!
//! [`run`] builds three values and hands them down as plain arguments: a
//! [`ui::Ui`], the [`config::Config`], and the [`repo::Repo`]. The
//! repository says where its project config lives, and the config says
//! where the data root is, so the order is fixed.
//!
//! The commands own the order of their steps: [`create`] for `wtm new`,
//! [`remove`] for `wtm rm`, and [`commands`] for the rest. Below them,
//! [`clone`] fills a worktree by copy-on-write, [`trash`] deletes trees in
//! the background, and [`hook`] runs the init hook. [`repo`] and [`git`]
//! answer questions for everyone.
//!
//! Some rules the design depends on:
//!
//! - Git answers every question about the repository. Nothing here matches
//!   ignore patterns or builds the path of git's metadata directory, which
//!   git renames on collision.
//! - Nothing is stored. A new fact needs a new way to derive it, never a
//!   state file.
//! - A tree only moves by `rename(2)`. A copy across filesystems would turn
//!   an instant removal into a slow one without saying so.
//! - [`create::run`] is the only way a worktree gets made.

pub mod cli;
pub mod clone;
pub mod commands;
pub mod config;
pub mod create;
pub mod error;
pub mod git;
pub mod hook;
pub mod name;
pub mod remove;
pub mod repo;
pub mod shell;
pub mod trash;
pub mod ui;

use std::str::FromStr;

use cli::{AgentCommand, Cli, Command};
use config::FlagOverrides;
use error::{Error, Result};
use name::WorktreeName;
use repo::Repo;
use ui::Ui;

/// The skill `wtm agent skill` prints, in the Agent Skills format so any
/// coding agent can load it. Flags live in `--help`; the skill teaches the
/// loop and what to do when a command refuses.
const SKILL: &str = include_str!("skill.md");

pub fn run(cli: Cli) -> Result<i32> {
    let ui = Ui::new(cli.quiet);

    // `shell` and `agent` print static text and must work outside a
    // repository, so they are handled before anything is discovered.
    match &cli.command {
        Command::Shell(args) => {
            ui.emit(shell::wrapper(args.shell));
            return Ok(0);
        }
        Command::Agent(args) => {
            return match args.command {
                Some(AgentCommand::Skill) => {
                    ui.emit(SKILL.trim_end());
                    Ok(0)
                }
                None => Err(Error::usage("usage: wtm agent skill")),
            };
        }
        _ => {}
    }

    // A reaper needs one directory and nothing else. Answering it here keeps
    // repository discovery, and its git invocation, out of the background
    // process entirely.
    if let Command::Gc(args) = &cli.command {
        if let (true, Some(trash)) = (args.detach, args.trash.as_ref()) {
            return trash::reap(&ui, trash).map(|()| 0);
        }
    }

    // Build order: the repository locates the project configuration, and the
    // configuration locates the data root.
    let git_version = git::check_version()?;
    // Kept apart from `from`: with `--repo` the two are not even in the same
    // tree, and a relative `--init` follows the caller, not the repository.
    let cwd = std::env::current_dir().map_err(|e| Error::io("current directory", e))?;
    let from = cli.repo.clone().unwrap_or_else(|| cwd.clone());
    let (main, bare) = repo::main_worktree(&from)?;
    let config = config::load(
        &flags(&cli),
        &|key| std::env::var(key).ok(),
        Some(&config::project_config_file(&main)),
        Some(&config::global_config_file()),
        &cwd,
        &main,
    )?;
    let repo = Repo::new(main, bare, &config.dir.value);

    // Removal leaves bytes for later, so every command that gets this far
    // pays one readdir to start collecting them. A reaper is only spawned
    // when there is something to sweep.
    if !matches!(cli.command, Command::Gc(_)) {
        trash::collect(&ui, &[repo.trash()], false)?;
    }

    match cli.command {
        Command::New(args) => {
            create::run(
                &ui,
                &repo,
                &config,
                WorktreeName::from_str(&args.name)?,
                args.branch.as_deref(),
                args.no_init,
                args.clone_mode,
            )?;
            Ok(0)
        }
        Command::Ls(args) => commands::ls(&ui, &repo, args.json.json),
        Command::Cd(args) => commands::cd(&ui, &repo, args.name.as_deref()),
        Command::Rm(args) => {
            let options = remove::Options {
                force: args.force,
                wait: args.wait,
                delete_branch: args.delete_branch,
                force_delete_branch: args.force_delete_branch,
            };
            remove::remove(&ui, &repo, &WorktreeName::from_str(&args.name)?, &options)
        }
        Command::Doctor(args) => commands::doctor(&ui, &repo, &git_version, args.json),
        Command::Config(args) => commands::config(&ui, &config, args.json),
        Command::Init(args) => commands::init(&ui, &repo, &config, args.name.as_deref()),
        Command::Gc(args) => commands::gc(&ui, &repo, args.wait),
        Command::Shell(_) | Command::Agent(_) => unreachable!("answered above"),
    }
}

/// Command-line values that correspond to configuration keys, collected into
/// the highest-precedence layer of the merge.
fn flags(cli: &Cli) -> FlagOverrides {
    let mut flags = FlagOverrides {
        dir: cli.dir.clone(),
        ..Default::default()
    };
    match &cli.command {
        Command::New(args) => {
            flags.base = args.base.clone();
            flags.fetch = args.fetch.then_some(true);
            flags.init = args.init.clone();
        }
        Command::Init(args) => flags.init = args.init.clone(),
        _ => {}
    }
    flags
}
