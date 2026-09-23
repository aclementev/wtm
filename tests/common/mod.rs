#![allow(dead_code, unused_imports)]

pub mod repo;

pub use repo::{RepoBuilder, TestRepo, count_entries, entries_in};

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Stat data is trusted only when the source's ctime is at least a whole
/// second older than `since`'s second, and a fixture has just been
/// written. Tests that need its files trusted start two seconds on.
pub fn wait_until_trusted() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    std::thread::sleep(Duration::from_nanos(
        2_000_000_000 - u64::from(now.subsec_nanos()),
    ));
}

/// Parses `wtm ls --json`.
pub fn listing(repo: &TestRepo) -> Vec<serde_json::Value> {
    let output = repo.wtm().args(["ls", "--json"]).output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).expect("ls --json prints JSON")
}
