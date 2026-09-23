---
name: wtm
description: Use before changing code in a git repository, to do the work in a worktree of its own instead of the main checkout, and whenever you need to create, find or remove worktrees.
compatibility: Requires the wtm command-line tool and git 2.31 or later.
---

# Working in wtm worktrees

Do each task in its own worktree. The main checkout is shared with the user
and with other agents, and switching branches or leaving edits there
disturbs them. `wtm` makes a full checkout on its own branch in seconds.

## The loop

```sh
cd "$(wtm new fix/login-timeout)"
# edit, test, commit
cd "$(wtm cd)"
wtm rm fix/login-timeout
```

- `wtm new <name>` creates a worktree on branch `<name>` and prints its
  absolute path on stdout, and nothing else. Progress and errors go to
  stderr. Names may contain `/`.
- If your shell does not keep the working directory between commands, keep
  the printed path and use it every time: `cd <path> && make test`, or
  `git -C <path> status`.
- Commit before removing. `wtm rm` refuses a worktree with uncommitted
  changes, and refuses to run from inside it, so go back to the main
  checkout first.
- `wtm rm` keeps the branch. Add `-d` to delete it too once it is merged.

Run `wtm --help` or `wtm <command> --help` for every other flag.

## Common situations

- **Know what a new worktree contains.** Tracked files at the remote's
  default branch, plus any untracked files `.worktreeinclude` lists.
  Dependencies and build output are not copied. If the repository has a
  `wtm-init.sh`, `wtm new` runs it to set those up; otherwise install them
  yourself before building.
- **Start from what is in the main checkout** rather than the remote, for
  example to include local commits that are not pushed yet:
  `wtm new <name> --base HEAD`.
- **Start from the latest remote state:** `wtm new <name> --fetch`.
- **Build on another task's branch:** `wtm new feat/part-2 --base feat/part-1`.
- **Check out someone else's branch** to review or test it:
  `wtm new review/their-feature --fetch --base origin/their-feature`.
- **Resume earlier work.** If the branch already exists, `wtm new <name>`
  checks it out as it is. Don't pass `--base` then; it is an error.
- **Return to a worktree:** `cd "$(wtm cd <name>)"`.
- **Find existing work** before starting a duplicate: `wtm ls --json` lists
  every worktree with its branch, base commit, age and path.
- **Work in parallel.** Give each task, or each agent, its own worktree
  with a distinct name. A branch can only be checked out in one worktree
  at a time.
- **Creation was slow.** `wtm doctor` says whether the filesystem could
  clone, and why not.

## When a command fails

| exit | meaning | what to do |
|---|---|---|
| 1 | failure | read stderr |
| 2 | usage error, such as a bad or taken name | stderr says what is allowed |
| 3 | the worktree was created but its init hook failed | the path is printed and the worktree is usable; fix the cause, then `wtm init <name>` |
| 4 | refused: uncommitted changes, or git has it locked | commit the work; pass `--force` only when the user agrees to lose the changes |

## Rules

- Remove worktrees with `wtm rm`. `rm -rf` leaves git's record of the
  worktree behind, and `git worktree remove` takes as long as deleting
  every file.
- `wtm rm` deletes the files in a background process. If your sandbox kills
  background processes when a command ends, pass `--wait`.
- Do not skip the init hook with `--no-init` unless the user asks.
