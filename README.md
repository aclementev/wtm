# wtm

Git worktrees for very large repositories, fast enough to give every task,
or every coding agent, its own full checkout.

`wtm new` copies your checkout with copy-on-write cloning instead of
writing every file out of git, so a new worktree of a huge monorepo takes
seconds rather than minutes. `wtm rm` returns at once and deletes the
files in the background. The result is a plain git worktree, with no
sparse checkout, no virtual filesystem and no daemon.

## Install

```sh
cargo install --git https://github.com/aclementev/wtm
```

You need git 2.31 or later, on macOS or Linux. Then add the shell function,
so `wtm new` and `wtm cd` can change your directory:

```sh
eval "$(wtm shell zsh)"                  # ~/.zshrc
eval "$(wtm shell bash)"                 # ~/.bashrc
eval (wtm shell fish | string collect)   # ~/.config/fish/config.fish
```

## Quickstart

```sh
cd ~/code/monorepo
wtm new fix/login     # new worktree on branch fix/login, and you're in it
# work, commit
wtm ls                # what you have open
wtm cd                # back to the main checkout
wtm rm fix/login      # gone at once; the branch stays
```

To set up every new worktree automatically, put an executable
`wtm-init.sh` at the root of the repository. It runs inside each new
worktree, so it can install dependencies or initialize submodules.

If `wtm new` is slower than you expected, run `wtm doctor`. It says
whether cloning works here, and why not if it doesn't.

## Docs

- [Usage](docs/usage.md): every command, exit codes, shell integration,
  coding agents
- [Configuration](docs/configuration.md): settings, the init hook,
  `.worktreeinclude`
- [How it works](docs/how-it-works.md): cloning, the index shortcut,
  removal, and why nothing is stored

## Development

```sh
cargo test
```

The tests build real repositories under `target/tmp` and check `wtm`
against git itself. Help text and the shell wrappers are
[insta](https://insta.rs) snapshots, so after changing them run
`cargo insta review`. The agent skill is `src/skill.md`, and a test checks
that every command and flag it mentions exists. CI runs the suite on APFS,
on XFS with reflinks, and on ext4, which covers the checkout fallback.
`cargo doc --open` shows how the modules fit together.
