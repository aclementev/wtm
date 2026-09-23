mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::RepoBuilder;
use wtm::git::Git;
use wtm::repo::{self, Repo};
use wtm::workspace::Workspace;

// These replace stored state. Every one is a fact `wtm` could have written
// down and instead derives.

fn json(output: Vec<u8>) -> serde_json::Value {
    serde_json::from_slice(&output).expect("json output")
}

#[test]
fn creation_time_comes_from_gits_metadata_directory() {
    let repo = RepoBuilder::new("derive-created").build();
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    repo.wtm().args(["new", "task"]).assert().success();
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let listing = json(repo.wtm().args(["ls", "--json"]).output().unwrap().stdout);
    let created = listing[0]["created"].as_u64().expect("a creation time");

    assert!(
        created + 1 >= before && created <= after + 1,
        "created {created} is not within a second of [{before}, {after}]"
    );
}

#[test]
fn base_is_the_merge_base_with_the_default_branch_for_a_tag_or_a_raw_sha() {
    let repo = RepoBuilder::new("derive-base").build();
    let first = repo.git(&["rev-parse", "HEAD"]);
    repo.git(&["tag", "v1"]);
    repo.write("file0.txt", "second commit\n");
    repo.git(&["commit", "-qam", "second"]);

    for (name, base) in [("from-tag", "v1"), ("from-sha", first.as_str())] {
        repo.wtm()
            .args(["new", name, "--base", base])
            .assert()
            .success();
    }

    let listing = json(repo.wtm().args(["ls", "--json"]).output().unwrap().stdout);
    for worktree in listing.as_array().unwrap() {
        assert_eq!(
            worktree["base"].as_str(),
            Some(first.as_str()),
            "{} branched from the wrong commit",
            worktree["name"]
        );
    }
}

#[test]
fn gits_metadata_directory_name_is_read_back_and_never_computed() {
    let repo = RepoBuilder::new("derive-gitdir").build();
    // Both names end in `task`, so git derives `task` for one and `task1` for
    // the other. Joining the worktree name would resolve both to the same path.
    repo.wtm().args(["new", "a/task"]).assert().success();
    repo.wtm().args(["new", "b/task"]).assert().success();

    let git = Git::new().unwrap();
    let discovered = Repo::discover(&git, &repo.main).unwrap();
    let id = repo.repo_id();
    let first = git.gitdir_of(&repo.worktree_path(&id, "a/task")).unwrap();
    let second = git.gitdir_of(&repo.worktree_path(&id, "b/task")).unwrap();

    assert_ne!(first, second);
    assert!(first.is_dir() && second.is_dir());
    assert!(
        [&first, &second]
            .iter()
            .any(|p| p.file_name().unwrap() != "task"),
        "git should have renamed one of {first:?} and {second:?}"
    );

    // Both are still listed with a creation time, which is the fact that
    // depends on resolving the metadata directory correctly.
    let workspace = Workspace::new(discovered, repo.data.clone());
    let views = repo::view(&git, &workspace).unwrap();
    assert_eq!(views.len(), 2);
    assert!(views.iter().all(|v| v.created.is_some()));
}
