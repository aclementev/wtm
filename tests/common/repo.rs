use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Temporary directories live under `target/tmp` rather than `/tmp`: on macOS
/// `/tmp` is a separate APFS volume, and cloning across volumes fails.
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
}

impl RepoBuilder {
    pub fn new(label: &str) -> RepoBuilder {
        RepoBuilder {
            label: label.to_string(),
            files: 3,
            ignored: Vec::new(),
            dirty: false,
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

    pub fn build(self) -> TestRepo {
        let root = scratch(&self.label);
        let main = root.join("repo");
        let data = root.join("data");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&data).unwrap();

        let repo = TestRepo {
            root,
            main,
            data,
        };
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
        String::from_utf8_lossy(&output.stdout).trim_end().to_string()
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.main, args)
    }

    /// A `wtm` invocation isolated from the developer's own home directory, so
    /// no real configuration file can influence a test.
    pub fn wtm(&self) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::cargo_bin("wtm").expect("build wtm");
        command
            .current_dir(&self.main)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("share"))
            .env("WTM_DIR", &self.data)
            .env_remove("WTM_DEBUG");
        command
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

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
