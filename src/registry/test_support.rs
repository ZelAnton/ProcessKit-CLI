//! Shared in-tree registry fixtures.
//!
//! These helpers are public only because integration tests compile the library as
//! a normal dependency, without `cfg(test)`. The library itself is explicitly not
//! a supported API; production code never calls this module.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Liveness, REGISTRY_VERSION, Record};

/// Return a unique path that does not currently exist, suitable for an isolated
/// registry whose opening semantics are part of the test.
pub fn scratch_registry(tag: &str) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "processkit-cli-registry-fixture-{tag}-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// Write the record half of a fixture through the real serializable type, so a
/// format change cannot leave a hand-written JSON template silently stale.
fn write_record(dir: &Path, stem: &str, run_id: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("create the registry fixture directory");
    let record = Record {
        registry_version: REGISTRY_VERSION,
        run_id: run_id.to_string(),
        endpoint: None,
        started_at: "2026-07-22T00:00:00.000Z".to_string(),
        argv_sha256: None,
        hint: None,
        labels: BTreeMap::new(),
        jsonl: None,
        capture_dir: None,
        liveness: Liveness {
            kind: "advisory_lock".to_string(),
            lock_file: format!("{stem}.lock"),
        },
    };
    let path = dir.join(format!("{stem}.json"));
    let bytes = serde_json::to_vec(&record).expect("serialize the registry fixture record");
    fs::write(&path, bytes).expect("write the registry fixture record");
    path
}

/// Write a confirmed-stale record and unlocked sibling lock file.
pub fn write_stale_entry(dir: &Path, stem: &str, run_id: &str) -> PathBuf {
    let path = write_record(dir, stem, run_id);
    fs::write(dir.join(format!("{stem}.lock")), b"").expect("write the unlocked fixture lock file");
    path
}

/// Write a record whose lock path is a directory, making its liveness
/// deterministically unprobeable on every supported platform.
pub fn write_unprobeable_entry(dir: &Path, stem: &str, run_id: &str) -> PathBuf {
    let path = write_record(dir, stem, run_id);
    fs::create_dir(dir.join(format!("{stem}.lock")))
        .expect("create the directory at the fixture lock path");
    path
}
