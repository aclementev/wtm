mod common;

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestRepo;
use common::repo::scratch;
use wtm::git::Oid;
use wtm::index::{self, HashAlgo};

const COMMITTED: &str = "deep/a/b/c/d/e/shared-prefix-1.txt";

/// A repository whose paths exercise v4 prefix compression and whose files
/// all carry a 2020 mtime. Without the old mtime every entry would sit in
/// git's racy window, and git would verify its content no matter what stat
/// data we wrote, so no test could tell good stat data from bad.
fn fixture(label: &str, object_format: &str) -> TestRepo {
    let root = scratch(label);
    let repo = TestRepo {
        main: root.join("repo"),
        data: root.join("data"),
        root,
    };
    fs::create_dir_all(&repo.main).unwrap();
    fs::create_dir_all(&repo.data).unwrap();
    let format = format!("--object-format={object_format}");
    repo.git(&["init", "-q", "-b", "main", &format, "."]);
    repo.git(&["config", "user.email", "test@example.com"]);
    repo.git(&["config", "user.name", "Test"]);

    for path in [
        COMMITTED,
        "deep/a/b/c/d/e/shared-prefix-2.txt",
        "deep/a/b/other.txt",
        "ñandú/日本語.txt",
        "a",
        "a-b",
        "top.txt",
        "tool.sh",
    ] {
        repo.write(path, &format!("contents of {path}\n"));
    }
    repo.write("empty", "");
    fs::set_permissions(repo.main.join("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("deep", repo.main.join("link")).unwrap();
    let status = Command::new("find")
        .args([
            ".",
            "-path",
            "./.git",
            "-prune",
            "-o",
            "-exec",
            "touch",
            "-h",
            "-t",
            "202001010000",
            "{}",
            "+",
        ])
        .current_dir(&repo.main)
        .status()
        .unwrap();
    assert!(status.success());
    repo.git(&["add", "-A"]);

    // A gitlink without a real submodule behind it, which is all the index
    // sees of one. The empty directory is what a fresh checkout leaves.
    let target = repo.git(&["hash-object", "-w", "top.txt"]);
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{target},sub"),
    ]);
    fs::create_dir(repo.main.join("sub")).unwrap();
    repo.git(&["commit", "-q", "-m", "initial commit"]);
    repo
}

fn index_path(repo: &TestRepo) -> PathBuf {
    repo.main.join(".git/index")
}

fn algo(repo: &TestRepo) -> HashAlgo {
    HashAlgo::of(&Oid::from_hex(repo.git(&["rev-parse", "HEAD"]))).unwrap()
}

/// Entries whose stat data does not match the file on disk. Git decides
/// this from stat alone and reads no content, which makes it the oracle
/// for whether our stat data is what git would have written.
fn stat_mismatches(repo: &TestRepo) -> Vec<String> {
    let out = repo.git(&["diff-files", "--name-only", "-z"]);
    out.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Stat data is trusted only when the source's ctime is whole seconds
/// older than `since`, and a fixture has just been written. Tests that
/// need its files trusted start on the next second.
fn wait_for_next_second() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    std::thread::sleep(Duration::from_nanos(
        1_000_000_000 - u64::from(now.subsec_nanos()),
    ));
}

/// Zeroes the stat data of every entry, as `wtm new` finds the index.
fn zeroed_index(repo: &TestRepo, version: u32) {
    repo.git(&["config", "index.version", &version.to_string()]);
    repo.git(&["read-tree", "HEAD"]);
    let bytes = fs::read(index_path(repo)).unwrap();
    assert_eq!(
        bytes[4..8],
        version.to_be_bytes(),
        "git wrote a different version"
    );
}

#[test]
fn the_parser_finds_every_entry_git_lists() {
    for object_format in ["sha1", "sha256"] {
        for version in [2u32, 3, 4] {
            let repo = fixture("index-parse", object_format);
            // Intent-to-add sets the extended flags, which only v3 and up
            // can hold; git would raise a v2 index to v3 to store it.
            if version > 2 {
                repo.write("added-later.txt", "later\n");
                repo.git(&["add", "-N", "added-later.txt"]);
            }
            repo.git(&["update-index", "--index-version", &version.to_string()]);
            let bytes = fs::read(index_path(&repo)).unwrap();
            assert_eq!(bytes[4..8], version.to_be_bytes());

            let ours: Vec<(String, u16, Vec<u8>)> = index::entries(&bytes, algo(&repo))
                .unwrap_or_else(|| panic!("v{version} {object_format} was refused"))
                .into_iter()
                .map(|e| (format!("{:o}", e.mode), e.stage(), e.path))
                .collect();
            let git: Vec<(String, u16, Vec<u8>)> = repo
                .git(&["ls-files", "-s", "-z"])
                .split('\0')
                .filter(|r| !r.is_empty())
                .map(|record| {
                    // "<mode> <oid> <stage>\t<path>"
                    let (meta, path) = record.split_once('\t').unwrap();
                    let fields: Vec<&str> = meta.split(' ').collect();
                    (
                        fields[0].to_string(),
                        fields[2].parse().unwrap(),
                        path.as_bytes().to_vec(),
                    )
                })
                .collect();
            assert_eq!(ours, git, "v{version} {object_format}");
        }
    }
}

#[test]
fn a_filled_index_leaves_git_nothing_to_verify_and_git_still_sees_changes() {
    for version in [2, 4] {
        let repo = fixture("index-fill", "sha1");
        zeroed_index(&repo, version);
        fs::remove_file(repo.main.join("top.txt")).unwrap();

        let since = SystemTime::now() + Duration::from_secs(1);
        let filled = index::fill_stat(
            &index_path(&repo),
            &repo.main,
            &repo.main,
            algo(&repo),
            since,
        )
        .unwrap()
        .expect("git's own index should be understood");

        assert!(filled > 0);
        assert_eq!(stat_mismatches(&repo), ["top.txt"], "v{version}");

        let mut appended = fs::File::options()
            .append(true)
            .open(repo.main.join(COMMITTED))
            .unwrap();
        std::io::Write::write_all(&mut appended, b"appended\n").unwrap();
        // The same length as "contents of a-b\n".
        fs::write(repo.main.join("a-b"), "contents of XYZ\n").unwrap();
        fs::set_permissions(repo.main.join("empty"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(repo.main.join("link")).unwrap();
        std::os::unix::fs::symlink("ñandú", repo.main.join("link")).unwrap();
        let status = repo.git(&["-c", "core.quotePath=false", "status", "--porcelain"]);
        for path in ["top.txt", COMMITTED, "a-b", "empty", "link"] {
            assert!(
                status.contains(path),
                "v{version}: {path} missing from\n{status}"
            );
        }
    }
}

/// With `core.fileMode` false the executable bit on disk means nothing, so
/// the mode HEAD records has to survive the fill.
#[test]
fn the_mode_comes_from_head_and_not_the_filesystem() {
    let repo = fixture("index-mode", "sha1");
    repo.git(&["config", "core.fileMode", "false"]);
    zeroed_index(&repo, 2);
    fs::set_permissions(repo.main.join("a"), fs::Permissions::from_mode(0o755)).unwrap();

    let since = SystemTime::now() + Duration::from_secs(1);
    index::fill_stat(
        &index_path(&repo),
        &repo.main,
        &repo.main,
        algo(&repo),
        since,
    )
    .unwrap()
    .unwrap();

    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "");
}

/// The case that corrupts silently if the guard is wrong: an edit in the
/// source between git's dirty query and the clone, made by a tool that
/// keeps the old mtime. Only the ctime shows it.
#[test]
fn a_file_changed_after_since_is_left_for_git_to_check() {
    let repo = fixture("index-race", "sha1");
    zeroed_index(&repo, 2);
    wait_for_next_second();

    let since = SystemTime::now();
    let path = repo.main.join(COMMITTED);
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let original = fs::read(&path).unwrap();
    fs::write(&path, vec![b'X'; original.len()]).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();

    index::fill_stat(
        &index_path(&repo),
        &repo.main,
        &repo.main,
        algo(&repo),
        since,
    )
    .unwrap()
    .unwrap();

    assert_eq!(stat_mismatches(&repo), [COMMITTED]);
}

#[test]
fn a_damaged_index_is_refused() {
    let repo = fixture("index-damaged", "sha1");
    repo.git(&["update-index", "--index-version", "4"]);
    let bytes = fs::read(index_path(&repo)).unwrap();
    let algo = algo(&repo);
    assert!(index::entries(&bytes, algo).is_some());

    for length in 0..bytes.len() {
        assert!(
            index::entries(&bytes[..length], algo).is_none(),
            "{length} bytes accepted"
        );
    }
    let mut flipped = bytes.clone();
    flipped[bytes.len() / 2] ^= 1;
    assert!(index::entries(&flipped, algo).is_none());
}

/// Both creation paths must produce the same worktree, one of them with no
/// index code of ours involved. An untouched file keeping the source's
/// mtime shows the clone survived: a rewrite by git stamps the current time.
#[cfg(target_os = "macos")]
#[test]
fn creation_keeps_the_clone_with_and_without_the_fast_index() {
    let repo = fixture("index-e2e", "sha1");
    repo.write("top.txt", "modified in the source\n");
    wait_for_next_second();

    for (name, disable, expected) in [
        ("fast", false, "filled stat data for"),
        ("slow", true, "WTM_NO_FAST_INDEX is set"),
    ] {
        let mut command = repo.wtm();
        command.args(["new", name, "--clone-mode", "cow"]);
        if disable {
            command.env("WTM_NO_FAST_INDEX", "1");
        }
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{name}: {stderr}");
        assert!(stderr.contains(expected), "{name}: {stderr}");

        let dest = PathBuf::from(std::ffi::OsStr::from_bytes(output.stdout.trim_ascii_end()));
        assert_eq!(repo.git_in(&dest, &["status", "--porcelain"]), "", "{name}");
        assert_eq!(
            fs::read_to_string(dest.join("top.txt")).unwrap(),
            "contents of top.txt\n"
        );
        let mtime = |root: &PathBuf| {
            fs::metadata(root.join(COMMITTED))
                .unwrap()
                .modified()
                .unwrap()
        };
        assert_eq!(
            mtime(&dest),
            mtime(&repo.main),
            "{name}: the clone was rewritten"
        );
    }
}
