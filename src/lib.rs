pub mod cli;
pub mod clone;
pub mod commands;
pub mod config;
pub mod create;
pub mod docs;
pub mod error;
pub mod exclude;
pub mod git;
pub mod hook;
pub mod index;
pub mod name;
pub mod reaper;
pub mod remove;
pub mod repo;
pub mod shell;
pub mod ui;
pub mod workspace;

use std::str::FromStr;

use cli::{AgentCommand, Cli, Command};
use config::FlagOverrides;
use error::{Error, Result};
use git::Git;
use name::WorktreeName;
use repo::Repo;
use ui::Ui;
use workspace::Workspace;

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
                    ui.emit(docs::skill());
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
            reaper::detach_self()?;
            let failed = reaper::sweep(std::slice::from_ref(trash), &ui).failed;
            return if failed.is_empty() {
                Ok(0)
            } else {
                Err(Error::Undeleted {
                    root: trash.clone(),
                })
            };
        }
    }

    // Build order: the repository locates the project configuration, and the
    // configuration locates the data root.
    let git = Git::new()?;
    // Kept apart from `from`: with `--repo` the two are not even in the same
    // tree, and a relative `--init` follows the caller, not the repository.
    let cwd = std::env::current_dir().map_err(|e| Error::io("current directory", e))?;
    let from = cli.repo.clone().unwrap_or_else(|| cwd.clone());
    let repo = Repo::discover(&git, &from)?;
    let config = config::load(
        &flags(&cli),
        &|key| std::env::var(key).ok(),
        Some(&config::project_config_file(&repo.main)),
        Some(&config::global_config_file()),
        &cwd,
        &repo.main,
    )?;
    let workspace = Workspace::new(repo, workspace::canonical_root(&config.dir.value));

    // Removal leaves bytes for later, so every command that gets this far
    // pays one readdir to start collecting them. A reaper is only spawned
    // when there is something to sweep.
    if !matches!(cli.command, Command::Gc(_)) && has_entries(&workspace.trash()) {
        reaper::spawn_detached_reaper(&workspace.trash())?;
    }

    match cli.command {
        Command::New(args) => {
            let options = create::Options {
                branch: args.branch.clone(),
                no_init: args.no_init,
                clone_mode: args.clone_mode,
                fast_index: std::env::var_os("WTM_NO_FAST_INDEX").is_none(),
            };
            create::run(
                &git,
                &ui,
                &workspace,
                &config,
                WorktreeName::from_str(&args.name)?,
                &options,
            )?;
            Ok(0)
        }
        Command::Ls(args) => commands::ls(&git, &ui, &workspace, args.json.json),
        Command::Cd(args) => commands::cd(&ui, &workspace, args.name.as_deref()),
        Command::Rm(args) => {
            let options = remove::Options {
                force: args.force,
                wait: args.wait,
                delete_branch: args.delete_branch,
                force_delete_branch: args.force_delete_branch,
            };
            remove::remove(
                &git,
                &ui,
                &workspace,
                &WorktreeName::from_str(&args.name)?,
                &options,
            )
        }
        Command::Doctor(args) => commands::doctor(&git, &ui, &workspace, args.json),
        Command::Config(args) => commands::config(&ui, &config, args.json),
        Command::Init(args) => commands::init(&git, &ui, &workspace, &config, args.name.as_deref()),
        Command::Gc(args) => commands::gc(&git, &ui, &workspace, args.wait),
        Command::Shell(_) | Command::Agent(_) => unreachable!("answered above"),
    }
}

fn has_entries(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
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
