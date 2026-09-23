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
    ignored: Vec<(String, usize)>,
    dirty: bool,
    symlink_to_dir: bool,
    submodule: bool,
}

impl RepoBuilder {
    pub fn new(label: &str) -> RepoBuilder {
        RepoBuilder {
            label: label.to_string(),
            files: 3,
            ignored: Vec::new(),
            dirty: false,
            symlink_to_dir: false,
            submodule: false,
        }
    }

    pub fn files(mut self, n: usize) -> RepoBuilder {
        self.files = n;
        self
    }

    pub fn ignored(mut self, dir: &str, n: usize) -> RepoBuilder {
        self.ignored.push((dir.to_string(), n));
        self
    }

    /// Leaves one tracked file modified in the main worktree.
    pub fn dirty_file(mut self) -> RepoBuilder {
        self.dirty = true;
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
        let root = scratch(&self.label);
        let main = root.join("repo");
        let data = root.join("data");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&data).unwrap();

        let repo = TestRepo { root, main, data };
        repo.git(&["init", "-q", "-b", "main", "."]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "user.name", "Test"]);

        for i in 0..self.files {
            repo.write(&format!("file{i}.txt"), &format!("contents of file {i}\n"));
        }
        for (dir, count) in &self.ignored {
            repo.write(".gitignore", &format!("{dir}/\n"));
            for i in 0..*count {
                repo.write(&format!("{dir}/generated{i}"), "ignored\n");
            }
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

        if self.dirty {
            repo.write("file0.txt", "modified in the source\n");
        }
        repo
    }
}

pub struct TestRepo {
    pub root: PathBuf,
    pub main: PathBuf,
    pub data: PathBuf,
}

impl TestRepo {
    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.main.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Writes a file in the main worktree with the executable bit set, which
    /// `wtm` requires of an init hook.
    pub fn executable(&self, relative: &str, script: &str) -> PathBuf {
        self.write(relative, script);
        let path = self.main.join(relative);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Runs git with the same scrubbed environment as the binary, so oracle
    /// and subject always see the same repository.
    pub fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.arg("-C").arg(cwd).args(args);
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

    /// A `wtm` invocation isolated from the developer's own home directory, so
    /// no real configuration file can influence a test.
    ///
    /// Background reaping is off. `wtm rm` spawns a reaper for the entry it
    /// has just made, so anything asserting what is in the trash would
    /// otherwise be racing it. Tests that want a real reaper say so.
    pub fn wtm(&self) -> assert_cmd::Command {
        let mut command = self.wtm_reaping();
        command.env("WTM_NO_REAPER", "1");
        command
    }

    /// As `wtm`, but background reapers are left switched on.
    pub fn wtm_reaping(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::cargo_bin("wtm").expect("build wtm");
        command
            .current_dir(&self.main)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("share"))
            .env("WTM_DIR", &self.data)
            .env_remove("WTM_DEBUG")
            .env_remove("WTM_NO_REAPER");
        command
    }

    pub fn trash(&self, repo_id: &str) -> PathBuf {
        self.data.join(repo_id).join(".trash")
    }

    /// Fills the trash with entries that no worktree ever occupied. A sweep
    /// cannot tell the difference, and this is far cheaper than creating and
    /// removing that many worktrees.
    pub fn plant_trash(&self, repo_id: &str, entries: usize, files: usize) {
        let trash = self.trash(repo_id);
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

    pub fn worktree_path(&self, repo_id: &str, name: &str) -> PathBuf {
        self.data.join(repo_id).join(name)
    }

    /// The repo id `wtm` computes for this repository, read back from the tool
    /// rather than recomputed, so tests never duplicate the formula.
    pub fn repo_id(&self) -> String {
        let output = self.wtm().args(["doctor", "--json"]).output().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        json["repo"]["id"].as_str().unwrap().to_string()
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

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
