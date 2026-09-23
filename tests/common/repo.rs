use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Temporary directories live under `target/tmp` rather than `/tmp`, so
/// `cargo clean` takes them with it and CI can mount a filesystem that
/// clones there.
pub fn scratch(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/tmp")
        .join(format!("{label}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create scratch directory");
    std::fs::canonicalize(&root).expect("canonicalize scratch directory")
}

pub struct RepoBuilder {
    label: String,
    files: usize,
    symlink_to_dir: bool,
    submodule: bool,
}

impl RepoBuilder {
    pub fn new(label: &str) -> RepoBuilder {
        RepoBuilder {
            label: label.to_string(),
            files: 3,
            symlink_to_dir: false,
            submodule: false,
        }
    }

    pub fn files(mut self, n: usize) -> RepoBuilder {
        self.files = n;
        self
    }

    /// Commits `linked`, a top-level symlink to the tracked directory `real`.
    /// A clone that follows it arrives as a copy of the directory.
    pub fn symlink_to_dir(mut self) -> RepoBuilder {
        self.symlink_to_dir = true;
        self
    }

    /// Commits a populated submodule at `sub`. Its checkout in the main
    /// worktree holds a `.git` file pointing at the main repository's
    /// gitdir, which no other worktree can use.
    pub fn submodule(mut self) -> RepoBuilder {
        self.submodule = true;
        self
    }

    pub fn build(self) -> TestRepo {
        let repo = TestRepo::empty(&self.label);
        repo.git(&["init", "-q", "-b", "main", "."]);
        repo.identify();

        for i in 0..self.files {
            repo.write(&format!("file{i}.txt"), &format!("contents of file {i}\n"));
        }
        if self.symlink_to_dir {
            repo.write("real/inside.txt", "behind a symlink\n");
            std::os::unix::fs::symlink("real", repo.main.join("linked")).unwrap();
        }
        if self.submodule {
            repo.add_submodule("sub");
        }
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "initial commit"]);
        repo
    }
}

pub struct TestRepo {
    pub root: PathBuf,
    pub main: PathBuf,
    pub data: PathBuf,
}

impl TestRepo {
    /// A scratch root with an empty `repo` directory and a data root beside
    /// it, for fixtures that make the repository themselves.
    pub fn empty(label: &str) -> TestRepo {
        let root = scratch(label);
        let repo = TestRepo {
            main: root.join("repo"),
            data: root.join("data"),
            root,
        };
        std::fs::create_dir_all(&repo.main).unwrap();
        std::fs::create_dir_all(&repo.data).unwrap();
        repo
    }

    /// A `git clone` of `origin`, so `origin/HEAD` and fetching are real.
    pub fn clone_of(origin: &TestRepo, label: &str) -> TestRepo {
        let repo = TestRepo::empty(label);
        let url = origin.main.display().to_string();
        repo.git(&["clone", "-q", &url, "."]);
        repo.identify();
        repo
    }

    fn identify(&self) {
        self.git(&["config", "user.email", "test@example.com"]);
        self.git(&["config", "user.name", "Test"]);
    }

    /// The environment of every process a test starts, git and `wtm` alike,
    /// so the fixture and the tool under test read the same configuration
    /// and none of it is the developer's. A home inside the scratch root
    /// keeps out `~/.gitconfig` and the user's `wtm` config, and
    /// `GIT_CONFIG_NOSYSTEM` keeps out the machine-wide gitconfig. Either can
    /// hold a setting an older git rejects, such as `merge.conflictStyle =
    /// zdiff3` before 2.35, which fails every checkout.
    pub fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("HOME", self.root.clone().into()),
            ("XDG_CONFIG_HOME", self.root.join("config").into()),
            ("XDG_DATA_HOME", self.root.join("share").into()),
            ("WTM_DIR", self.data.clone().into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ]
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.main.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Writes a file in the main worktree with the executable bit set, which
    /// `wtm` requires of an init hook.
    pub fn executable(&self, relative: &str, script: &str) {
        self.write(relative, script);
        let path = self.main.join(relative);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs git with the same scrubbed environment as the binary, so oracle
    /// and subject always see the same repository.
    pub fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        // A path read from a failed `wtm` run is empty, and `git -C ""` runs
        // in the test process's directory, which is this crate's own checkout.
        assert!(
            cwd.starts_with(&self.root),
            "git would run outside the test's scratch directory, in {cwd:?}"
        );
        let mut command = Command::new("git");
        command.arg("-C").arg(cwd).args(args).envs(self.env());
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
        ] {
            command.env_remove(key);
        }
        let output = command.output().expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.main, args)
    }

    /// Makes a one-commit repository beside the main one and adds it as a
    /// submodule at `path`. Git refuses a local path as a submodule URL
    /// unless the file protocol is allowed.
    fn add_submodule(&self, path: &str) {
        let origin = self.root.join(format!("{path}-origin"));
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::write(origin.join("module.txt"), "in the submodule\n").unwrap();
        self.git_in(&origin, &["init", "-q", "-b", "main", "."]);
        self.git_in(&origin, &["add", "-A"]);
        self.git_in(
            &origin,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-q",
                "-m",
                "module",
            ],
        );
        let url = origin.display().to_string();
        self.git(&[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &url,
            path,
        ]);
    }

    /// `wtm` as a plain process in the environment of [`TestRepo::env`], with
    /// background reapers left on. For tests that need to spawn it and hold
    /// its pipes.
    pub fn process(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("wtm"));
        command
            .current_dir(&self.main)
            .envs(self.env())
            .env_remove("WTM_DEBUG")
            .env_remove("WTM_NO_REAPER");
        command
    }

    /// As `process`, with background reaping off. `wtm rm` spawns a reaper
    /// for the entry it has just made, so anything asserting what is in the
    /// trash would otherwise be racing it.
    pub fn wtm(&self) -> assert_cmd::Command {
        let mut command = self.wtm_reaping();
        command.env("WTM_NO_REAPER", "1");
        command
    }

    pub fn wtm_reaping(&self) -> assert_cmd::Command {
        assert_cmd::Command::from_std(self.process())
    }

    /// Runs `wtm new` with `args` and returns the path it printed, which is
    /// the only thing on its stdout.
    pub fn new_worktree(&self, args: &[&str]) -> PathBuf {
        let output = self.wtm().arg("new").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "wtm new {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end())
    }

    /// Runs `wtm` and returns its stdout, failing the test with `wtm`'s stderr
    /// when it exits non-zero. The output of a failed run is empty, and a test
    /// that used it as a path would act on this crate's own checkout.
    pub fn wtm_stdout(&self, args: &[&str]) -> String {
        let assert = self.wtm().args(args).assert().success();
        String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 output")
    }

    /// The repo id `wtm` computes, read back from the tool rather than
    /// recomputed, for tests that plant entries in a trash before any
    /// worktree exists.
    pub fn repo_id(&self) -> String {
        let json: serde_json::Value =
            serde_json::from_str(&self.wtm_stdout(&["doctor", "--json"])).unwrap();
        json["repo"]["id"].as_str().unwrap().to_string()
    }

    pub fn trash(&self) -> PathBuf {
        self.data.join(self.repo_id()).join(".trash")
    }

    /// Whether `wtm new` would clone here, asked of `wtm` itself.
    pub fn clones(&self) -> bool {
        let json: serde_json::Value =
            serde_json::from_str(&self.wtm_stdout(&["doctor", "--json"])).unwrap();
        json["method"]["method"] == "cow"
    }

    /// Fills the trash with entries that no worktree ever occupied. A sweep
    /// cannot tell the difference, and this is far cheaper than creating and
    /// removing that many worktrees.
    pub fn plant_trash(&self, entries: usize, files: usize) {
        let trash = self.trash();
        for entry in 0..entries {
            for file in 0..files {
                let dir = trash
                    .join(format!("planted-{entry}"))
                    .join(format!("d{}", file % 20));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(format!("f{file}")), "x").unwrap();
            }
        }
    }

    pub fn is_registered(&self, path: &Path) -> bool {
        self.git(&["worktree", "list", "--porcelain"])
            .contains(&path.display().to_string())
    }

    pub fn branch_exists(&self, branch: &str) -> bool {
        !self.git(&["branch", "--list", branch]).is_empty()
    }
}

/// Every file and directory under `path`, the root included. Counting rather
/// than naming, so a test asserts that a tree is still there without
/// depending on what happens to be in it.
pub fn count_entries(path: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return usize::from(path.symlink_metadata().is_ok());
    };
    1 + entries
        .flatten()
        .map(|e| count_entries(&e.path()))
        .sum::<usize>()
}

/// The entries directly in a trash, none when it does not exist.
pub fn entries_in(trash: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(trash) else {
        return Vec::new();
    };
    entries.flatten().map(|entry| entry.path()).collect()
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
