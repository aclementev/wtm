mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::{RepoBuilder, TestRepo};
use proptest::prelude::*;

/// Directory names, two of which the ignore grammar names whole.
const DIRS: &[&str] = &["a", "b", "build", "node_modules"];
/// File names, one per extension the grammars know about.
const NAMES: &[&str] = &["one.txt", "two.log", "three.env", "four.rs"];
const IGNORE: &[&str] = &["*.log", "build/", "node_modules/", "*.env"];
/// Exact file, directory with a trailing slash, `*.ext` and negation.
const INCLUDE: &[&str] = &[
    "a/one.txt",
    "*.env",
    "build/",
    "node_modules/",
    "*.log",
    "!*.log",
    "!b/",
];

#[derive(Clone, Copy, Debug)]
enum Fate {
    Tracked,
    /// Tracked, then modified in the source after the commit.
    Dirty,
    /// Never added. Ignored or not depends on the patterns drawn.
    Untracked,
}

#[derive(Debug)]
struct Layout {
    files: BTreeMap<String, Fate>,
    ignore: Vec<&'static str>,
    include: Vec<&'static str>,
    mode: &'static str,
    /// Start the branch at the builder's commit instead of the source's
    /// HEAD, so creation has to move the tree after filling it.
    older_base: bool,
}

fn file_path() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(prop::sample::select(DIRS), 0..3),
        prop::sample::select(NAMES),
    )
        .prop_map(|(dirs, name)| {
            let mut path: Vec<&str> = dirs;
            path.push(name);
            path.join("/")
        })
}

fn layout() -> impl Strategy<Value = Layout> {
    let fate = prop_oneof![
        Just(Fate::Tracked),
        Just(Fate::Dirty),
        Just(Fate::Untracked)
    ];
    (
        prop::collection::btree_map(file_path(), fate, 1..12),
        prop::sample::subsequence(IGNORE, 0..=IGNORE.len()),
        prop::sample::subsequence(INCLUDE, 0..=INCLUDE.len()),
        prop::sample::select(&["auto", "checkout"][..]),
        any::<bool>(),
    )
        .prop_map(|(files, ignore, include, mode, older_base)| Layout {
            files,
            ignore,
            include,
            mode,
            older_base,
        })
}

/// A second commit on top of the builder's, then the uncommitted state.
///
/// Tracked files are added with `-f`, so a tracked file can sit inside a
/// directory the ignore patterns name. Git never collapses such a directory,
/// and a walk that trusted the pattern would lose the file.
fn build(label: &str, layout: &Layout) -> TestRepo {
    let repo = RepoBuilder::new(label).symlink_to_dir().submodule().build();

    repo.write(".gitignore", &layout.ignore.join("\n"));
    repo.git(&["add", ".gitignore"]);
    for (path, fate) in &layout.files {
        if let Fate::Tracked | Fate::Dirty = fate {
            repo.write(path, &format!("committed {path}\n"));
            repo.git(&["add", "-f", path]);
        }
    }
    repo.git(&["commit", "-q", "--allow-empty", "-m", "layout"]);

    for (path, fate) in &layout.files {
        match fate {
            Fate::Tracked => {}
            Fate::Dirty => repo.write(path, &format!("modified {path}\n")),
            Fate::Untracked => repo.write(path, &format!("untracked {path}\n")),
        }
    }
    if !layout.include.is_empty() {
        repo.write(".worktreeinclude", &layout.include.join("\n"));
    }
    repo
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    File(Vec<u8>),
    Symlink(PathBuf),
}

/// Every path under `root` with what it is and holds, the worktree's own
/// `.git` file left out. Symlinks are read, never followed.
fn listing(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
        for entry in fs::read_dir(root.join(rel)).unwrap() {
            let entry = entry.unwrap();
            if rel.as_os_str().is_empty() && entry.file_name() == ".git" {
                continue;
            }
            let path = rel.join(entry.file_name());
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                out.insert(path, Entry::Symlink(fs::read_link(entry.path()).unwrap()));
            } else if kind.is_dir() {
                out.insert(path.clone(), Entry::Dir);
                visit(root, &path, out);
            } else {
                out.insert(path, Entry::File(fs::read(entry.path()).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, Path::new(""), &mut out);
    out
}

/// The untracked files `.worktreeinclude` names, as git matches them.
fn included(repo: &TestRepo) -> Vec<PathBuf> {
    let file = repo.main.join(".worktreeinclude");
    if !file.exists() {
        return Vec::new();
    }
    let arg = format!("--exclude-from={}", file.display());
    repo.git(&["ls-files", "-z", "-o", "-i", &arg])
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// What a new worktree should hold: the tree `git worktree add` checks out
/// at `base`, plus each included file and its directories as they are in
/// the source. Both halves come from git, so this cannot share a bug with
/// our matching.
fn expected(repo: &TestRepo, base: &str) -> BTreeMap<PathBuf, Entry> {
    let oracle = repo.root.join("oracle");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        &oracle.display().to_string(),
        base,
    ]);

    let mut tree = listing(&oracle);
    let source = listing(&repo.main);
    for path in included(repo) {
        for ancestor in path.ancestors().filter(|a| !a.as_os_str().is_empty()) {
            tree.insert(ancestor.to_path_buf(), source[ancestor].clone());
        }
    }
    tree
}

/// Names each path that differs rather than printing two whole trees.
fn assert_same_tree(actual: &BTreeMap<PathBuf, Entry>, expected: &BTreeMap<PathBuf, Entry>) {
    let missing: Vec<_> = expected
        .keys()
        .filter(|p| !actual.contains_key(*p))
        .collect();
    let extra: Vec<_> = actual
        .keys()
        .filter(|p| !expected.contains_key(*p))
        .collect();
    let differing: Vec<_> = expected
        .iter()
        .filter(|(p, e)| actual.get(*p).is_some_and(|a| a != *e))
        .map(|(p, e)| (p, e, &actual[p]))
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty() && differing.is_empty(),
        "missing {missing:?}\nextra {extra:?}\ndiffering (path, expected, actual) {differing:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]

    /// Both sides of the comparison come from git, so the test cannot agree
    /// with a bug in our own matching. The fixture carries a top-level
    /// symlink to a directory and a submodule, so the comparison also covers
    /// both staying what `git worktree add` makes of them: a symlink and an
    /// empty directory. Both creation paths run on every platform, and on
    /// the clone path an older base makes git move the cloned tree.
    #[test]
    fn a_new_worktree_equals_git_worktree_add_plus_includes(layout in layout()) {
        let repo = build("creation-diff", &layout);
        let base = if layout.older_base { "HEAD~1" } else { "HEAD" };

        let dest = repo.new_worktree(&[
            "subject", "--no-init", "--base", base, "--clone-mode", layout.mode,
        ]);

        assert_same_tree(&listing(&dest), &expected(&repo, base));
    }
}

/// Git copies the sparse patterns of the worktree it runs in into every
/// worktree it adds, so a sparse main checkout would silently give sparse
/// worktrees on both creation paths.
#[test]
fn a_sparse_main_checkout_still_gives_full_worktrees() {
    let repo = RepoBuilder::new("creation-sparse").build();
    repo.write("kept/a.txt", "inside the cone\n");
    repo.write("dropped/b.txt", "outside the cone\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "two directories"]);
    repo.git(&["sparse-checkout", "set", "kept"]);
    assert!(!repo.main.join("dropped/b.txt").exists());

    for mode in ["auto", "checkout"] {
        let dest = repo.new_worktree(&[mode, "--clone-mode", mode]);

        assert!(dest.join("dropped/b.txt").is_file(), "{mode}");
        assert_eq!(repo.git_in(&dest, &["status", "--porcelain"]), "", "{mode}");
    }
    assert!(
        !repo.main.join("dropped/b.txt").exists(),
        "the main checkout changed"
    );
}
