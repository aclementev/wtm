mod common;

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use proptest::prelude::*;
use wtm::exclude::{Class, ExcludeSet};
use wtm::name::WorktreeName;
use wtm::repo::{Repo, RepoId};
use wtm::workspace::Workspace;

/// A workspace over paths that need not exist, since every method under
/// test is a path join or its inverse.
fn workspace() -> Workspace {
    let main = PathBuf::from("/repos/monorepo");
    Workspace::new(
        Repo {
            id: RepoId::for_main_worktree(&main),
            common_dir: main.join(".git"),
            bare: false,
            main,
        },
        PathBuf::from("/data/root"),
    )
}

#[test]
fn two_paths_to_the_same_directory_share_an_id() {
    let dir = common::repo::scratch("repoid-symlink");
    let real = dir.join("monorepo");
    let link = dir.join("link-to-monorepo");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let through_link = std::fs::canonicalize(&link).unwrap();
    assert_eq!(
        RepoId::for_main_worktree(&real),
        RepoId::for_main_worktree(&through_link)
    );
}

#[test]
fn directories_sharing_a_basename_get_different_ids() {
    let a = RepoId::for_main_worktree(Path::new("/one/monorepo"));
    let b = RepoId::for_main_worktree(Path::new("/two/monorepo"));

    assert_ne!(a, b);
    assert!(a.as_str().starts_with("monorepo-"));
    assert!(b.as_str().starts_with("monorepo-"));
}

proptest! {
    #[test]
    fn an_id_is_one_readable_component_and_eight_hex_digits(path in "(/[A-Za-z0-9._-]{1,12}){1,5}") {
        let id = RepoId::for_main_worktree(Path::new(&path));
        let (basename, hash) = id.as_str().rsplit_once('-').expect("id has a hash suffix");

        prop_assert!(!id.as_str().contains('/'));
        prop_assert!(!basename.is_empty());
        prop_assert_eq!(hash.len(), 8);
        prop_assert!(hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    /// The point of validating a name is that it can be joined onto a path we
    /// later delete from, so every accepted name must stay inside the
    /// repository's directory.
    #[test]
    fn an_accepted_name_stays_inside_the_repository_directory(text in ".{0,40}") {
        let Ok(name) = WorktreeName::from_str(&text) else {
            return Ok(());
        };
        let workspace = workspace();
        let repo_dir = workspace.repo_dir();
        let worktree_dir = workspace.dir(&name);

        prop_assert!(worktree_dir.starts_with(&repo_dir));
        prop_assert_ne!(&worktree_dir, &repo_dir);
        prop_assert!(!worktree_dir.components().any(|c| c == Component::ParentDir));
    }

    #[test]
    fn parsing_a_name_is_idempotent(text in ".{0,40}") {
        let Ok(name) = WorktreeName::from_str(&text) else {
            return Ok(());
        };
        prop_assert_eq!(WorktreeName::from_str(&name.to_string()).unwrap(), name);
    }
}

#[test]
fn names_that_could_escape_the_repository_directory_are_refused() {
    for bad in [
        "",
        "..",
        "../elsewhere",
        "feat/../../escape",
        "/absolute",
        "-flag",
        "a//b",
        "a/",
    ] {
        assert!(
            WorktreeName::from_str(bad).is_err(),
            "{bad:?} should not parse"
        );
    }
}

#[test]
fn a_name_is_recovered_from_the_path_it_produces() {
    let workspace = workspace();
    let name = WorktreeName::from_str("feat/login").unwrap();

    let path: PathBuf = workspace.dir(&name);
    assert_eq!(workspace.name_of(&path), Some(name));
    assert_eq!(workspace.name_of(Path::new("/elsewhere/feat")), None);
    assert_eq!(workspace.name_of(&workspace.trash().join("gone-abc")), None);
}

/// Every path the exclusion properties query: all of them up to four deep
/// over a three-letter alphabet, so each named path, its ancestors, its
/// children and its unrelated neighbours are all asked about.
fn every_path() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::new()];
    let mut all = Vec::new();
    for _ in 0..4 {
        paths = paths
            .iter()
            .flat_map(|p| ["a", "b", "c"].map(|c| p.join(c)))
            .collect();
        all.extend(paths.iter().cloned());
    }
    all
}

fn named_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec(prop::sample::select(&["a", "b", "c"][..]), 1..=4)
        .prop_map(|parts| parts.iter().collect())
}

/// The lists as git produces them. Included paths are files, so nothing
/// either query names sits below one; the untracked query named each of
/// them too, which is the ordinary case `from_lists` has to resolve.
fn lists() -> impl Strategy<Value = (Vec<PathBuf>, Vec<PathBuf>)> {
    (
        prop::collection::vec(named_path(), 0..6),
        prop::collection::vec(named_path(), 0..4),
        any::<bool>(),
    )
        .prop_map(|(mut excluded, included, also_untracked)| {
            let below_an_include =
                |p: &PathBuf| included.iter().any(|i| p != i && p.starts_with(i));
            let included: Vec<PathBuf> = included
                .iter()
                .filter(|p| !below_an_include(p))
                .cloned()
                .collect();
            excluded.retain(|p| !below_an_include(p));
            if also_untracked {
                excluded.extend(included.iter().cloned());
            }
            (excluded, included)
        })
}

/// Paths the walk can actually ask about: never one below an included file.
fn queries(included: &[PathBuf]) -> impl Iterator<Item = PathBuf> + '_ {
    every_path()
        .into_iter()
        .filter(|p| !included.iter().any(|i| p != i && p.starts_with(i)))
}

proptest! {
    /// An include wins over an exclusion at the same path and over any
    /// excluded ancestor, however far up it sits.
    #[test]
    fn an_included_path_is_cloned_whole_however_deep_its_exclusion((excluded, included) in lists()) {
        let set = ExcludeSet::from_lists(excluded, included.clone());
        for path in &included {
            prop_assert_eq!(set.classify(path), Class::CloneWhole, "{:?}", path);
        }
    }

    /// Anything else would stop the walk before it reached the include:
    /// `Skip` drops it and `CloneWhole` carries its excluded siblings along.
    #[test]
    fn every_ancestor_of_an_included_path_is_recursed((excluded, included) in lists()) {
        let set = ExcludeSet::from_lists(excluded, included.clone());
        for path in &included {
            for ancestor in path.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()) {
                prop_assert_eq!(set.classify(ancestor), Class::Recurse, "{:?} above {:?}", ancestor, path);
            }
        }
    }

    /// Git collapses an ignored directory to one path, so nothing below it
    /// is ever named. The walk must still skip all of it.
    #[test]
    fn a_path_under_an_exclusion_with_nothing_included_below_is_skipped((excluded, included) in lists()) {
        let set = ExcludeSet::from_lists(excluded.clone(), included.clone());
        for path in queries(&included) {
            let under_exclusion = excluded.iter().any(|e| path.starts_with(e));
            let include_at_or_below = included.iter().any(|i| i.starts_with(&path));
            if under_exclusion && !include_at_or_below {
                prop_assert_eq!(set.classify(&path), Class::Skip, "{:?}", path);
            }
        }
    }

    /// Cloning such a directory whole would carry the excluded path inside
    /// it, which is how a secret or a dirty file reaches a new worktree.
    #[test]
    fn a_kept_directory_holding_an_exclusion_is_recursed((excluded, included) in lists()) {
        let set = ExcludeSet::from_lists(excluded.clone(), included.clone());
        for path in queries(&included) {
            let under_exclusion = excluded.iter().any(|e| path.starts_with(e));
            let exclusion_below = excluded
                .iter()
                .any(|e| e != &path && e.starts_with(&path) && !included.contains(e));
            if !under_exclusion && exclusion_below {
                prop_assert_eq!(set.classify(&path), Class::Recurse, "{:?}", path);
            }
        }
    }
}
