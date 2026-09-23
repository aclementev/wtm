# Using wtm

## The loop

One worktree per task. Make it, work in it, throw it away:

```sh
wtm new fix-login      # a worktree on a new branch fix-login, and you're in it
# edit, test, commit
wtm rm fix-login       # gone at once; the branch stays
```

`wtm rm` keeps the branch, so picking the work up again later is just
`wtm new fix-login`. When the branch already exists, `wtm new` checks it out
as it is instead of making a new one.

## Commands

### `wtm new <name>`

Creates a worktree and prints its path. That path is the only thing it
writes to stdout.

The name becomes the directory, and the branch too unless you pass
`--branch`. It may contain slashes (`feat/login`) and must otherwise stick
to letters, digits, `.`, `_` and `-`.

| flag | what it does |
|---|---|
| `--base <ref>` | start the new branch here instead of the remote's default branch |
| `--branch <name>` | name the branch differently from the worktree |
| `--fetch` | `git fetch` first, so the base is up to date |
| `--init <path>` | run this hook instead of the configured one |
| `--no-init` | run no hook |
| `--clone-mode auto\|cow\|checkout` | how to fill the worktree; see below |

What happens to the branch:

- **It doesn't exist yet.** `wtm` creates it at the base.
- **It exists and nothing has it checked out.** `wtm` checks it out as it
  is. Passing `--base` here is an error, since the branch already has its
  own history.
- **Another worktree has it checked out.** `wtm` refuses and tells you
  where. Git won't allow it either.

`--clone-mode auto`, the default, clones the main checkout when the
filesystem supports it and falls back to a normal checkout when it doesn't.
`cow` fails instead of falling back. `checkout` never tries to clone.

`wtm new` also refuses when the main checkout is in the middle of a rebase,
merge, cherry-pick, revert or bisect. Finish or abort it first.

### `wtm ls [--json]`

Lists this repository's worktrees: name, branch, the commit it branched
from, age, and path. A status column shows up when something is off:
`missing` if the directory is gone, `locked` if someone ran `git worktree
lock` on it.

### `wtm cd [<name>]`

Prints a worktree's path, or the main checkout's with no name. With the
shell function installed, it takes you there.

### `wtm rm <name>`

Removes a worktree and returns straight away, whatever its size. The files
are moved aside and deleted in the background.

It refuses when the worktree has uncommitted changes or is locked. Commit,
or pass `--force` to throw the changes away. It also refuses if you're
standing inside it.

| flag | what it does |
|---|---|
| `--force` | remove it anyway |
| `-d`, `--delete-branch` | delete the branch too, unless it has unmerged commits |
| `-D`, `--force-delete-branch` | delete the branch even if it has unmerged commits |
| `--wait` | delete the files before returning instead of in the background |

`--wait` is worth using in a sandbox that kills background processes when
the command ends. Without it the files still get deleted later, by
the next `wtm` command or `wtm gc`.

### `wtm init [<name>]`

Runs the init hook again in a worktree, the one you're in if you don't name
one. Handy after fixing a hook that failed.

### `wtm gc [--wait]`

Empties the trash of every repository and tidies up after git. You rarely
need it, since any `wtm` command starts cleaning its own repository's trash.
With `--wait` it does the work in the foreground and tells you how much it
deleted.

### `wtm doctor [--json]`

Explains what `wtm new` will do here and why. It says whether it can
clone, and if not, what's in the way. It also shows the git version, where the
repository and the worktrees live, and what `.worktreeinclude` matches.
Start here when creation is slower than you expected.

### `wtm config [--json]`

Prints every setting and where its value came from. See
[configuration](configuration.md).

### Everywhere

`--repo <path>` runs against another repository instead of the one you're
in. `--dir <path>` uses another data root. `-q`/`--quiet` silences
progress messages.

## Output and exit codes

Stdout only ever carries the result: a path, a listing, JSON. Progress,
warnings and errors go to stderr. So this is safe in scripts:

```sh
cd "$(wtm new task)"
```

| code | meaning |
|---|---|
| 0 | done |
| 1 | failed |
| 2 | usage error: a bad name, an unknown worktree, a hook that can't run |
| 3 | the worktree was created, but the init hook failed |
| 4 | refused, and `--force` would do it anyway |

On exit 3 the worktree is there and usable, and its path is still printed.

## Shell integration

A program can't change its parent shell's directory, so `wtm` ships a small
shell function that does it for you:

```sh
eval "$(wtm shell zsh)"                  # ~/.zshrc
eval "$(wtm shell bash)"                 # ~/.bashrc
eval (wtm shell fish | string collect)   # ~/.config/fish/config.fish
```

After `wtm new` or `wtm cd` the function changes into the printed
directory. Everything else passes straight through, and the exit code is
always the program's own.

One fish quirk: `wtm new x 2>/dev/null` still shows progress there, because
fish ignores that redirect for output written inside a command
substitution. Use `--quiet` instead.

## Coding agents

`wtm agent skill` prints a skill that teaches an agent the loop above,
what the exit codes mean and what to do when `wtm rm` refuses. It follows
the [Agent Skills](https://agentskills.io) format, so any agent that reads
that format can use it. Save it as `SKILL.md` in a directory named `wtm`
wherever your agent looks for skills. For Claude Code:

```sh
mkdir -p ~/.claude/skills/wtm
wtm agent skill > ~/.claude/skills/wtm/SKILL.md
```
