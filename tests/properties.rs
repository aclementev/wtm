use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use proptest::prelude::*;
use wtm::clone::exclude::{Class, ExcludeSet};
use wtm::git::GitVersion;
use wtm::name::WorktreeName;
use wtm::repo::Repo;

proptest! {
    /// The point of validating a name is that it is joined onto a path wtm
    /// later deletes from, so every accepted name must stay inside the
    /// repository's directory.
    #[test]
    fn an_accepted_name_stays_inside_the_repository_directory(text in ".{0,40}") {
        let Ok(name) = WorktreeName::from_str(&text) else {
            return Ok(());
        };
        let repo = Repo::new(
            PathBuf::from("/repos/monorepo"),
            false,
            Path::new("/data/root"),
            GitVersion::default(),
        );
        let worktree_dir = repo.dir(&name);

        prop_assert!(worktree_dir.starts_with(repo.repo_dir()));
        prop_assert_ne!(&worktree_dir, &repo.repo_dir());
        prop_assert!(!worktree_dir.components().any(|c| c == Component::ParentDir));
    }
}

/// Random text almost never spells an escape, so the ones that matter are
/// named here.
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
        "feat/./x",
    ] {
        assert!(
            WorktreeName::from_str(bad).is_err(),
            "{bad:?} should not parse"
        );
    }
}

/// Every path up to four deep over a three-letter alphabet, so each named
/// path, its ancestors, its children and its unrelated neighbours are all
/// asked about.
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
/// either query names sits below one, and the untracked query usually
/// names each included path too, which `from_lists` has to resolve.
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

/// What the walk should do with `path`, stated over the plain lists rather
/// than a trie. An include wins over any exclusion at or above it; a
/// directory holding something to keep and something to skip is walked
/// into; anything under an exclusion is skipped; and everything else is
/// cloned in one call, which is what makes creation fast.
fn model(path: &Path, excluded: &[PathBuf], included: &[PathBuf]) -> Class {
    let strictly_below = |outer: &Path, inner: &Path| inner != outer && inner.starts_with(outer);
    if included.iter().any(|i| i == path) {
        Class::CloneWhole
    } else if included.iter().any(|i| strictly_below(path, i)) {
        Class::Recurse
    } else if excluded.iter().any(|e| path.starts_with(e)) {
        Class::Skip
    } else if excluded
        .iter()
        .any(|e| strictly_below(path, e) && !included.contains(e))
    {
        Class::Recurse
    } else {
        Class::CloneWhole
    }
}

proptest! {
    #[test]
    fn every_path_is_classified_as_the_lists_say((excluded, included) in lists()) {
        let set = ExcludeSet::from_lists(excluded.clone(), included.clone());
        // The walk never looks below an included file.
        let asked = every_path()
            .into_iter()
            .filter(|p| !included.iter().any(|i| p != i && p.starts_with(i)));
        for path in asked {
            prop_assert_eq!(
                set.classify(&path),
                model(&path, &excluded, &included),
                "{:?} with excluded {:?} and included {:?}",
                path,
                excluded,
                included
            );
        }
    }
}
