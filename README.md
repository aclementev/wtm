# wtm

Git worktrees for very large repositories, fast enough to give every task,
or every coding agent, its own full checkout.

`wtm new` copies your checkout with copy-on-write cloning instead of
writing every file out of git, so a new worktree of a huge monorepo takes
seconds rather than minutes. `wtm rm` returns at once and deletes the
files in the background. The result is a plain git worktree, with no
sparse checkout, no virtual filesystem and no daemon.

## Install

Download a prebuilt binary from
[Releases](https://github.com/aclementev/wtm/releases), for macOS on Apple
silicon (`aarch64-apple-darwin`) or Linux on x86_64 or ARM (the `musl`
builds, which run on any distribution). For example:

```sh
curl -fsSL https://github.com/aclementev/wtm/releases/latest/download/wtm-aarch64-apple-darwin.tar.gz | tar xz -C ~/.local/bin wtm
```

The binaries are not notarized. If you download one with a browser on
macOS, clear the quarantine flag before running it with
`xattr -d com.apple.quarantine wtm`. A `curl` download needs no such step.

Or build from source:

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
against git itself: a new worktree must match what `git worktree add`
produces, and the index must be one git trusts. The shell wrappers run in
real zsh, bash and fish, so all three need to be installed. CI runs the
suite on APFS, on XFS with reflinks, and on ext4, which has no cloning.
`cargo doc --open` shows how the modules fit together.
