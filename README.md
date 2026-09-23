# wtm

A worktree manager for very large repositories, built for agentic
workflows: give every task its own full checkout, create it in about the
time a shell prompt takes to return, and throw it away instantly.

It creates a git worktree by copy-on-write cloning an existing checkout,
using `clonefile` on APFS and reflinks on Linux, rather than writing every
file out of the object store. The result is an ordinary linked worktree
with every file present and readable: no sparse checkout, no virtual
filesystem, no daemon. Removal renames the tree out of the way and unlinks
it in the background, so it returns immediately whatever the size.

## Install

```sh
cargo install --git https://github.com/aclementev/wtm
```

It needs git 2.36 or later, and runs on macOS and Linux.

Then add the shell function, so `wtm new` and `wtm cd` change directory:

```sh
eval "$(wtm shell zsh)"     # in ~/.zshrc
eval "$(wtm shell bash)"    # in ~/.bashrc
eval (wtm shell fish | string collect)   # in ~/.config/fish/config.fish
```

## Use

```sh
wtm new fix-login          # new worktree on a new branch fix-login, and cd into it
# work, commit
wtm rm fix-login           # gone at once; the branch is kept unless you pass -d
```

| command | what it does |
|---|---|
| `wtm new <name>` | create a worktree and print its path; `--base <ref>` to start elsewhere, `--branch <name>` for a different branch name |
| `wtm ls [--json]` | list this repository's worktrees with branch, base, age and path |
| `wtm cd [<name>]` | print a worktree's path, or the main checkout's |
| `wtm rm <name>` | remove it; refuses uncommitted changes unless `--force` |
| `wtm init [<name>]` | rerun the init hook in a worktree |
| `wtm gc [--wait]` | empty the trash now |
| `wtm doctor` | say how `wtm new` will populate a worktree here, and why |
| `wtm config` | print the effective configuration and where each value came from |
| `wtm agent skill` | print a skill file that teaches a coding agent the loop above |

Stdout carries only the result (a path, a list, JSON), so
`cd "$(wtm new x)"` is safe. Everything else goes to stderr. Exit codes: 0
success, 1 failure, 2 usage error, 3 created but the init hook failed, 4
refused and `--force` would override it.

## Per-repository setup

Two optional files at the root of the repository:

- **`wtm-init.sh`**, an executable run inside every new worktree: install
  dependencies, `git submodule update --init`, and the like. It sees
  `WTM_HOOK_ROOT`, `WTM_HOOK_NAME`, `WTM_HOOK_BRANCH`, `WTM_HOOK_BASE_REF`,
  `WTM_HOOK_BASE_SHA`, `WTM_HOOK_MAIN`, `WTM_HOOK_REPO_ID` and
  `WTM_HOOK_METHOD`. If it fails the worktree is kept and `wtm new` exits 3.
  `--no-init` skips it.
- **`.worktreeinclude`**, gitignore patterns naming untracked files to copy
  into every new worktree, such as `.env.local`. Nothing else that is
  untracked or ignored is carried over.

## Configuration

`~/.config/wtm/config.toml`:

```toml
dir = "~/.local/share/wtm/worktrees"   # where worktrees live
branch_prefix = "me/"                  # `wtm new foo` creates branch me/foo
fetch = false                          # git fetch before creating
init = "~/bin/my-hook.sh"              # a hook of your own
```

`<repo>/.wtm/config.toml` may set `base` (the default is the remote's
default branch) and `init`. Every key can also be set with a `WTM_<KEY>`
environment variable, and all but `branch_prefix` with a flag; `wtm config`
shows which one won.

## Where things are

Worktrees live in `~/.local/share/wtm/worktrees/<repo>-<hash>/<name>`, and
`wtm` stores nothing else anywhere: git and the filesystem answer every
question it asks.

Cloning needs the worktrees on the same filesystem as the repository. When
they are not, or the filesystem cannot clone (ext4, for one), `wtm new`
falls back to an ordinary parallel checkout, which is correct and just
slower. `wtm doctor` says which it will do and why.

Moving a repository is fine: its worktrees stay where they are and `wtm`
keeps managing them. Deleting a repository leaves its worktrees behind in
that directory, and `rm -rf` removes them.

## Design

- [DESIGN.md](DESIGN.md): behaviour, the creation and removal algorithms,
  the hook contract
- [ARCHITECTURE.md](ARCHITECTURE.md): modules, key types and signatures
- [TESTING.md](TESTING.md): how correctness is verified
- [PLAN.md](PLAN.md): the order it was built in
