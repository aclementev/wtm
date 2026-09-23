# How wtm works

You don't need any of this to use `wtm`. It's here for when something is
slower than expected, when you wonder whether it's safe, or when you're
curious how a worktree of a huge repository can appear so fast.

## Creating: clone, don't check out

`git worktree add` writes every file out of git's object store. On a
repository with a few hundred thousand files that can take minutes, and it's
most of why people avoid worktrees on big monorepos.

`wtm` copies your main checkout instead, using the filesystem's
copy-on-write cloning: `clonefile` on macOS, reflinks on Linux. A clone
shares the original's disk blocks until one side changes, so it costs
almost no space and almost no time. The result is an ordinary git
worktree. Every file is really there. There's no sparse checkout, no
virtual filesystem and no daemon.

The clone skips what shouldn't come along. `wtm` asks git what is ignored,
what is untracked and what has uncommitted changes, and leaves all of that
behind, except the untracked files `.worktreeinclude` asks for. It never
matches gitignore patterns itself, because git is the only thing that gets
them exactly right. Git reports a fully ignored directory like
`node_modules` as a single entry, so skipping it costs one check, and any
directory with nothing excluded inside is cloned in one call.

Files you've changed but not committed are left out too. The new worktree
gets the committed version, the way `git worktree add` would. Submodule
directories come out empty, also as git leaves them. The init hook is the
place to run `git submodule update --init`.

Then the new worktree has to move from your main checkout's commit to the
branch you asked for. Git only rewrites the files that differ between the
two, so starting from a recent base is fast.

## The index shortcut

There's a catch. Git keeps an index of every file's size, timestamps and
inode, and uses it to tell whether a file changed without reading it. A
cloned file has a new inode and new timestamps, so a fresh index doesn't
describe any of them. Git would then read every file in the worktree once
to be sure. On a big repository that's most of a second, often more than
the clone itself.

So `wtm` fills in the index itself, from the clones it just made. It only
changes the stat fields, which sit at a fixed spot in each entry, so no
entry moves and nothing else in the file changes. If the index is in a
format `wtm` doesn't fully understand, it leaves it alone, warns you, and
lets git do the slow check. That costs time, never correctness.

It's also careful about files that change while it works. An entry is only
filled when the source file hasn't changed since `wtm` asked git which
files were dirty, judged by its ctime, which nothing can set back. Anything
touched in the last second or two is left for git to check. That costs
next to nothing.

If you ever suspect the shortcut, set `WTM_NO_FAST_INDEX=1`. Git then
checks every file itself, and the result is the same, only slower.

## When it checks out instead

Cloning needs the worktrees on the same filesystem as your repository, and
a filesystem that supports it. APFS does. On Linux, btrfs, XFS with
reflinks, bcachefs and recent ZFS do. ext4 and tmpfs don't.

When cloning isn't possible, `wtm new` falls back to a regular checkout
with one git worker per CPU core. That's slower but correct, and it says
why on the way. It does the same for a bare repository, which has no files
to clone. `wtm doctor` spells out which case you're in.

A sparse main checkout is fine. New worktrees get every tracked file
either way, even though git on its own would copy the sparse patterns
into them.

## Removing: rename now, delete later

Deleting a big tree takes as long as there are files. `wtm rm` doesn't
wait for that. It renames the worktree into a trash directory next to it,
which is instant whatever the size, tells git the worktree is gone, and
starts a low-priority background process to delete the files.

That process detaches completely, so closing your terminal doesn't stop it
and `$(wtm rm x)` doesn't hang on it. If something kills it anyway, nothing
is lost. Every `wtm` command checks the trash and starts another one.
Each entry is locked while it's deleted, so two cleaners never fight over
the same files, and a half-deleted entry gets finished later.

`wtm rm --wait` deletes in place and returns when the files are gone, which
is what you want where background processes don't survive the command.

## No state

`wtm` keeps no records. There's no database, no registry and no lock file.
It works everything out from git and the filesystem when it needs it:

- which worktrees exist comes from `git worktree list`;
- which of them are `wtm`'s comes from where they sit, under the data root;
- when one was made comes from its directory's creation time;
- what it branched from comes from `git merge-base`.

The one thing that isn't recorded anywhere is whether the init hook
succeeded. `wtm new` tells you with exit code 3 at the time, and that's it.

## Where things live

```
~/.local/share/wtm/worktrees/
  monorepo-3f9a1c2e/          one directory per repository
    fix-login/                a worktree
    feat/search/              names can nest
    .trash/                   removed worktrees waiting to be deleted
```

The repository's directory is its name plus a short hash of its path, so
two clones called `monorepo` don't collide. Moving a repository gives it a
new hash, but its old worktrees keep working. `wtm` still finds them, and
repairs git's links to them the next time you `cd`, `init` or `rm` one.
Deleting a repository leaves its worktrees orphaned in that directory, and
`rm -rf` is the way to clear them.

## Safety

- `wtm` never changes files in your main checkout.
- It deletes nothing outside its trash, apart from a worktree it was
  halfway through creating when something failed.
- `wtm rm` won't remove uncommitted work without `--force`, and it keeps
  the branch unless you ask.
- Untracked files, like secrets in `.env`, stay behind unless
  `.worktreeinclude` names them.
- `wtm-init.sh` runs as you, like any other script in the repository.
