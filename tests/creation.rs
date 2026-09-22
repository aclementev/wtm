mod common;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{RepoBuilder, TestRepo};
use proptest::prelude::*;
use wtm::clone::fake::Copier;
use wtm::clone::{self, Cloner, Walker};
use wtm::create;
use wtm::exclude::ExcludeSet;
use wtm::git::Git;
use wtm::ui::Ui;

/// Directory names, two of which the ignore grammar names whole.
const DIRS: &[&str] = &["a", "b", "build", "node_modules"];
/// File names, one per extension the grammars know about.
const NAMES: &[&str] = &["one.txt", "two.log", "three.env", "four.rs"];
const IGNORE: &[&str] = &["*.log", "build/", "node_modules/", "*.env"];
/// Exact file, directory with a trailing slash, `*.ext` and negation.
const INCLUDE: &[&str] = &["a/one.txt", "*.env", "build/", "node_modules/", "*.log", "!*.log", "!b/"];

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
    let fate = prop_oneof![Just(Fate::Tracked), Just(Fate::Dirty), Just(Fate::Untracked)];
    (
        prop::collection::btree_map(file_path(), fate, 1..12),
        prop::sample::subsequence(IGNORE, 0..=IGNORE.len()),
        prop::sample::subsequence(INCLUDE, 0..=INCLUDE.len()),
    )
        .prop_map(|(files, ignore, include)| Layout {
            files,
            ignore,
            include,
        })
}

/// A second commit on top of the builder's, then the uncommitted state.
///
/// Tracked files are added with `-f`, so a tracked file can sit inside a
/// directory the ignore patterns name. Git never collapses such a directory,
/// and a walk that trusted the pattern would lose the file.
fn build(label: &str, layout: &Layout) -> TestRepo {
    let repo = RepoBuilder::new(label)
        .symlink_to_dir()
        .submodule()
        .build();

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

/// What a new worktree should hold: the tree `git worktree add` checks out,
/// plus each included file and its directories as they are in the source.
/// Both halves come from git, so this cannot share a bug with our matching.
fn expected(repo: &TestRepo) -> BTreeMap<PathBuf, Entry> {
    let oracle = repo.root.join("oracle");
    repo.git(&["worktree", "add", "-q", "--detach", &oracle.display().to_string(), "HEAD"]);

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
    let missing: Vec<_> = expected.keys().filter(|p| !actual.contains_key(*p)).collect();
    let extra: Vec<_> = actual.keys().filter(|p| !expected.contains_key(*p)).collect();
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

/// The checks a listing comparison makes implicitly, stated on their own so
/// a failure names the rule that broke.
///
/// `linked` being a symlink here is not the `CLONE_NOFOLLOW` regression:
/// `reset --hard` replaces a directory copy with the tracked symlink, so
/// the finished tree looks right either way. The walk tests below catch it.
fn assert_fixture_rules(repo: &TestRepo, layout: &Layout, dest: &Path) {
    let linked = fs::symlink_metadata(dest.join("linked")).unwrap();
    assert!(linked.file_type().is_symlink(), "linked is not a symlink");
    assert_eq!(
        fs::read_dir(dest.join("sub")).unwrap().count(),
        0,
        "the submodule directory must be empty"
    );

    let included = included(repo);
    for (path, fate) in &layout.files {
        if let Fate::Untracked = fate {
            let carried = fs::symlink_metadata(dest.join(path)).is_ok();
            assert_eq!(
                carried,
                included.contains(&PathBuf::from(path)),
                "{path} is untracked and carried only if included"
            );
        }
    }
}

/// On APFS the walk must run, so a filesystem that stopped cloning fails
/// the test instead of quietly taking the checkout path. Elsewhere the
/// filesystem decides, and ext4 checking out must still pass.
const CLONE_MODE: &str = if cfg!(target_os = "macos") { "cow" } else { "auto" };

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, ..ProptestConfig::default() })]

    /// TESTING.md 2.3, end to end through the binary.
    #[test]
    fn a_new_worktree_equals_git_worktree_add_plus_includes(layout in layout()) {
        let repo = build("creation-diff", &layout);

        let output = repo
            .wtm()
            .args(["new", "subject", "--no-init", "--clone-mode", CLONE_MODE])
            .output()
            .unwrap();
        prop_assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let dest = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end());

        assert_fixture_rules(&repo, &layout, &dest);
        assert_same_tree(&listing(&dest), &expected(&repo));
    }

    /// The same comparison with the walk driven by the `fake` cloner, which
    /// runs on any filesystem. On one that cannot clone, the test above
    /// takes the checkout path and this is the only one that walks.
    #[test]
    fn the_walk_with_a_copier_equals_git_worktree_add_plus_includes(layout in layout()) {
        let repo = build("creation-walk", &layout);
        let dest = repo.root.join("walked");
        repo.git(&["worktree", "add", "-q", "--no-checkout", "--detach", &dest.display().to_string(), "HEAD"]);

        create::populate_by_clone(&Git::new().unwrap(), &Ui::new(true), &repo.main, &dest, &Copier)
            .unwrap();

        assert_fixture_rules(&repo, &layout, &dest);
        assert_same_tree(&listing(&dest), &expected(&repo));
    }
}

/// Runs the walk alone, before git gets the chance to repair what it did.
fn assert_walk_keeps_a_symlink_to_a_directory(label: &str, cloner: &dyn Cloner) {
    let repo = RepoBuilder::new(label).symlink_to_dir().build();
    let dest = repo.root.join("walked");
    repo.git(&["worktree", "add", "-q", "--no-checkout", "--detach", &dest.display().to_string(), "HEAD"]);

    let git = Git::new().unwrap();
    let set = ExcludeSet::compute(&git, &repo.main).unwrap();
    Walker::new(cloner, &set, &Ui::new(true), &repo.main, &dest)
        .run()
        .unwrap();

    let linked = fs::symlink_metadata(dest.join("linked")).unwrap();
    assert!(
        linked.file_type().is_symlink(),
        "{} followed a top-level symlink and copied the directory behind it",
        cloner.name()
    );
}

#[test]
fn the_walk_with_a_copier_keeps_a_symlink_to_a_directory() {
    assert_walk_keeps_a_symlink_to_a_directory("creation-symlink-fake", &Copier);
}

/// The `CLONE_NOFOLLOW` regression. Without the flag, `clonefile` given a
/// symlink clones what it points at.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "needs a filesystem that clones, which every APFS volume is"
)]
fn the_walk_with_the_platform_cloner_keeps_a_symlink_to_a_directory() {
    let cloner = clone::platform_cloner();
    assert_walk_keeps_a_symlink_to_a_directory("creation-symlink", cloner.as_ref());
}

/// Listings cannot tell a clone from a checkout that arrived at the same
/// contents, and a checkout is what a mistake in the creation sequence
/// falls back to without a word. A checkout stamps the current time; a
/// clone keeps the source's, so a file dated 2001 tells them apart.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "needs a filesystem that clones, which every APFS volume is"
)]
fn a_cloned_worktree_keeps_the_source_mtime_of_an_untouched_file() {
    let repo = RepoBuilder::new("creation-mtime").build();
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    File::options()
        .write(true)
        .open(repo.main.join("file1.txt"))
        .unwrap()
        .set_modified(past)
        .unwrap();
    // The dirty query compares stat without refreshing, so until the index
    // learns the new mtime the file counts as modified and is left for git
    // to write.
    repo.git(&["update-index", "--refresh", "-q"]);

    let output = repo
        .wtm()
        .args(["new", "subject", "--no-init", "--clone-mode", "cow"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dest = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end());

    let mtime = fs::metadata(dest.join("file1.txt")).unwrap().modified().unwrap();
    assert_eq!(mtime, past, "the file was written, not cloned");
}
