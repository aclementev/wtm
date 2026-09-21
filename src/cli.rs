use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "wtm",
    version,
    about = "Create, list and remove git worktrees for very large repositories",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Operate on the repository at this path instead of the current directory
    #[arg(long, global = true, value_name = "PATH")]
    pub repo: Option<PathBuf>,

    /// Data root holding the worktrees
    #[arg(long, global = true, value_name = "PATH")]
    pub dir: Option<PathBuf>,

    /// Silence progress messages
    #[arg(long, short, global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a worktree and print its path
    New(NewArgs),
    /// List the worktrees of this repository
    Ls(LsArgs),
    /// Print the path of a worktree
    Cd(CdArgs),
    /// Remove a worktree
    Rm(RmArgs),
    /// Rerun the init hook in a worktree
    Init(InitArgs),
    /// Empty the trash and prune stale git metadata
    Gc(GcArgs),
    /// Report on this repository, the data root and the creation method
    Doctor(JsonArgs),
    /// Print the effective configuration and where each value came from
    Config(JsonArgs),
    /// Print a shell function that changes directory after `new` and `cd`
    Shell(ShellArgs),
    /// Output for coding agents
    Agent(AgentArgs),
}

#[derive(Args, Debug)]
pub struct NewArgs {
    /// Name of the worktree; also the branch name after branch_prefix
    pub name: String,

    /// Commit the new branch starts from
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Init hook to run, overriding the configured one
    #[arg(long, value_name = "PATH")]
    pub init: Option<PathBuf>,

    /// Do not run the init hook
    #[arg(long, conflicts_with = "init")]
    pub no_init: bool,

    /// Fetch the base's remote before creating
    #[arg(long)]
    pub fetch: bool,

    /// How to populate the worktree
    #[arg(long, value_name = "MODE", value_enum, default_value = "auto")]
    pub clone_mode: CloneMode,

    /// Branch name, when it differs from the worktree name
    #[arg(long, value_name = "NAME")]
    pub branch: Option<String>,
}

#[derive(Args, Debug)]
pub struct LsArgs {
    #[command(flatten)]
    pub json: JsonArgs,
}

#[derive(Args, Debug)]
pub struct CdArgs {
    /// Worktree to print; the main worktree when omitted
    pub name: Option<String>,
}

#[derive(Args, Debug)]
pub struct RmArgs {
    /// Worktree to remove
    pub name: String,

    /// Remove even when the worktree has uncommitted changes
    #[arg(long)]
    pub force: bool,

    /// Unlink synchronously instead of returning as soon as the path is gone
    #[arg(long)]
    pub wait: bool,

    /// Delete the branch too, refusing if it is unmerged
    #[arg(long = "delete-branch", short = 'd')]
    pub delete_branch: bool,

    /// Delete the branch too, even if it is unmerged
    #[arg(long = "force-delete-branch", short = 'D')]
    pub force_delete_branch: bool,
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Worktree to run the hook in; the current one when omitted
    pub name: Option<String>,

    /// Init hook to run, overriding the configured one
    #[arg(long, value_name = "PATH")]
    pub init: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct GcArgs {
    /// Sweep in the foreground instead of spawning a background reaper
    #[arg(long)]
    pub wait: bool,

    /// Detach into the background, lower priority and sweep. How wtm
    /// re-executes itself as a reaper; not meant to be typed.
    #[arg(long, hide = true, requires = "trash")]
    pub detach: bool,

    /// Sweep only this directory instead of every trash under the data root.
    #[arg(long, hide = true, value_name = "PATH")]
    pub trash: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct JsonArgs {
    /// Emit JSON instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShellArgs {
    /// Shell to print a wrapper for
    #[arg(value_enum)]
    pub shell: Shell,
}

#[derive(Args, Debug)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub command: Option<AgentCommand>,
}

#[derive(Subcommand, Debug)]
pub enum AgentCommand {
    /// Print a skill file describing wtm to a coding agent
    Skill,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum CloneMode {
    /// Copy-on-write when the filesystem supports it, checkout otherwise
    #[default]
    Auto,
    /// Copy-on-write, failing when it is unavailable
    Cow,
    /// Always let git write the files
    Checkout,
}
