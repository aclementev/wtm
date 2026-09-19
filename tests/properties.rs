mod common;

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use proptest::prelude::*;
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
        prop_assert!(!name.trash_stem().contains('/'));
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
    for bad in ["", "..", "../elsewhere", "feat/../../escape", "/absolute", "-flag", "a//b", "a/"] {
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
