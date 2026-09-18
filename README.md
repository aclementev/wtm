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

Status: designed, implementation starting. The design is settled and
documented:

- [DESIGN.md](DESIGN.md) — behaviour: layout, configuration, every command,
  the creation and removal algorithms, the hook contract
- [ARCHITECTURE.md](ARCHITECTURE.md) — modules, key types and signatures
- [TESTING.md](TESTING.md) — how correctness is verified
- [PLAN.md](PLAN.md) — the implementation steps, in order

The measurements behind the design (benchmarks against `git worktree add`
on synthetic and real repositories, filesystem experiments, a survey of
existing tools) were produced in a research phase whose notes are kept out
of this repository to keep it about the tool.
