#![allow(dead_code, unused_imports)]

pub mod repo;

pub use repo::{RepoBuilder, TestRepo, count_entries};

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
