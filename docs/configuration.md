# Configuring wtm

Most people need no configuration at all. When you do, there are two files,
and each one owns different settings.

## Your settings

`~/.config/wtm/config.toml` holds the settings that are about you and your
machine:

```toml
dir = "~/.local/share/wtm/worktrees"   # where worktrees live
branch_prefix = "alvaro/"              # `wtm new foo` makes branch alvaro/foo
fetch = true                           # git fetch before every `wtm new`
```

## The repository's settings

`<repo>/.wtm/config.toml` holds the settings that are about the project.
Commit it or ignore it, whichever suits the project.

```toml
base = "origin/develop"                # where new branches start
init = "scripts/setup-worktree.sh"     # the init hook, if not wtm-init.sh
```

Each file refuses the other's keys, and tells you which file they belong in.
The split is deliberate. Cloning someone's repository should never move
your worktrees, rename your branches or add a network round trip to every
`wtm new`. And a hook in your personal config would quietly replace the
`wtm-init.sh` of every project you work on.

## Every setting

| key | default | file | env | flag |
|---|---|---|---|---|
| `dir` | `~/.local/share/wtm/worktrees` | yours | `WTM_DIR` | `--dir` |
| `branch_prefix` | none | yours | `WTM_BRANCH_PREFIX` | |
| `fetch` | `false` | yours | `WTM_FETCH` | `--fetch` |
| `base` | the remote's default branch | the repository's | `WTM_BASE` | `--base` |
| `init` | `wtm-init.sh` | the repository's | `WTM_INIT` | `--init` |

A flag beats the environment, which beats a file, which beats the default.
`wtm config` shows the value in effect and where it came from. A leading
`~` in `dir` or `init` means your home directory, and an unknown key is an
error.

`base = "origin/HEAD"`, the default, means whatever branch the remote calls
its default. Without a remote, `wtm` tries `origin/main`, then
`origin/master`, then whatever the main checkout has checked out.

## The init hook

If the repository has an executable `wtm-init.sh` at its root, `wtm new`
runs it inside every new worktree. It's the place to install dependencies,
run `git submodule update --init`, copy in a build cache, and so on.

```sh
#!/bin/sh
set -eu
npm ci
git submodule update --init
```

It runs inside the new worktree and gets these variables, always set, so
`set -u` is safe:

| variable | value |
|---|---|
| `WTM_HOOK_ROOT` | the new worktree |
| `WTM_HOOK_NAME` | its name |
| `WTM_HOOK_BRANCH` | the branch checked out |
| `WTM_HOOK_BASE_SHA` | the commit the branch started from, or empty if there is none |
| `WTM_HOOK_MAIN` | the main checkout |

The rest of the environment is yours, minus `GIT_DIR` and the other
variables that would point git at the wrong repository.

Its output goes to stderr, so stdout stays the path alone. It can read from
the terminal when you're at one, and gets an empty stdin otherwise, so it
can never hang an agent waiting for input.

A hook that fails doesn't take the worktree with it. `wtm new` prints the
path, says the hook failed, and exits 3. Fix the hook and rerun it with
`wtm init`.

Where the hook comes from, first match wins:

1. `--init <path>`
2. `WTM_INIT`
3. `init` in the repository's config
4. `wtm-init.sh`

A relative path you type, in the flag or the variable, is relative to where
you are. One in the config file, and the default, is relative to the main
checkout, which means a hook there can be gitignored and still run. A hook
that doesn't exist or isn't executable stops `wtm new` before it makes
anything. The exception is a missing `wtm-init.sh`, which means the
project has no hook. `--no-init` skips the hook entirely.

## Carrying untracked files

A new worktree gets every tracked file and nothing else. To bring along
untracked ones, like `.env.local`, list them in `.worktreeinclude` at the
repository root, using gitignore patterns:

```
.env.local
config/*.local.yaml
```

`wtm doctor` shows how many paths the file matches. The file is read from
the main checkout. It isn't copied into new worktrees unless it lists
itself, because an untracked file nobody ignores would make every new
worktree look dirty, and `wtm rm` refuses dirty worktrees. Committing it is
the easy option.

## Variables that aren't settings

- `WTM_NO_FAST_INDEX` makes `wtm new` let git check every cloned file
  instead of trusting `wtm`'s shortcut. It's slower. Use it if you ever
  suspect the shortcut; see [how it works](how-it-works.md).
- `WTM_DEBUG` writes background cleanup output to
  `~/.cache/wtm/reaper.log` instead of discarding it.
- `WTM_NO_REAPER` stops `wtm` starting background cleanup at all. The tests
  use it.
