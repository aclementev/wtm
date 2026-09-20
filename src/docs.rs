use clap::CommandFactory;

use crate::cli::Cli;

/// The skill file `wtm agent skill` prints. The command reference is generated
/// from the same clap definitions that produce `--help`, so the two cannot
/// drift; only the prose below is written by hand.
pub fn skill() -> String {
    let mut out = String::new();
    out.push_str(FRONT_MATTER);
    out.push_str(PROSE);
    out.push_str("\n## Commands\n");

    let command = Cli::command();
    for sub in command.get_subcommands() {
        out.push_str(&format!("\n### `wtm {}`\n\n", sub.get_name()));
        if let Some(about) = sub.get_about() {
            out.push_str(&format!("{about}.\n"));
        }
        // Hidden arguments are how wtm re-executes itself; an agent reading
        // this should no more type them than a person reading `--help`.
        let arguments: Vec<String> = sub
            .get_arguments()
            .filter(|arg| arg.get_id() != "help" && !arg.is_hide_set())
            .map(describe)
            .collect();
        if !arguments.is_empty() {
            out.push('\n');
            for line in arguments {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    out.push_str(FOOTER);
    out
}

fn describe(arg: &clap::Arg) -> String {
    let name = match arg.get_long() {
        Some(long) => format!("`--{long}`"),
        None => format!("`<{}>`", arg.get_id()),
    };
    let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
    format!("- {name}: {help}")
}

const FRONT_MATTER: &str = "---\nname: wtm\ndescription: \
Create, use and remove a dedicated git worktree for each task.\n---\n\n";

const PROSE: &str = r#"# wtm

`wtm` gives every task its own full checkout of the repository. Work in a
worktree, never in the main checkout. The main checkout is shared, and
changing branches there disturbs everything else running against it.

The loop for one task:

```sh
cd "$(wtm new fix-login)"
# work, then commit
wtm rm fix-login
```

`wtm new` prints exactly one line on stdout, the absolute path of the new
worktree, which is what makes `cd "$(wtm new ...)"` safe. Progress, warnings
and errors go to stderr. Every other command does the same, keeping stdout
for the paths, lists and JSON it was asked for.

`wtm rm` refuses a worktree with uncommitted changes and exits 4. Commit the
work, or pass `--force` to discard it. Removal keeps the branch unless you
pass `-d`.

`wtm rm` returns as soon as the path is gone, which is before the files are
unlinked: it renames the worktree into a trash directory and leaves a
background process to empty it. The name is free again immediately, so
`wtm new fix-login` straight after `wtm rm fix-login` works. Pass `--wait` to
do the unlinking up front instead, or run `wtm gc --wait` to empty the trash
now.

Run `wtm ls --json` to see what exists, with each worktree's branch, the
commit it branched from, and its age.

## Exit codes

- `0` success
- `1` failure
- `2` usage error
- `3` the worktree was created but its init hook failed; the path is still
  printed and the worktree is usable
- `4` refused, and `--force` would override it. The worktree is dirty, or
  git has it locked
"#;

const FOOTER: &str = r#"
## The init hook

If the repository has a `wtm-init.sh`, it runs inside each new worktree after
creation, with `WTM_HOOK_ROOT`, `WTM_HOOK_NAME`, `WTM_HOOK_BRANCH`,
`WTM_HOOK_BASE_REF`, `WTM_HOOK_BASE_SHA`, `WTM_HOOK_MAIN`,
`WTM_HOOK_REPO_ID` and `WTM_HOOK_METHOD` in its environment. Skip it with
`--no-init`, rerun it with `wtm init`.

Variables named `WTM_HOOK_*` describe the worktree to the hook. Variables
named `WTM_<KEY>`, such as `WTM_DIR`, go the other way. They configure `wtm`
itself, and setting one changes what later commands do.
"#;
