//! Helpers shared by the integration tests.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A directory under `tests/fixtures/`.
pub fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// The selection records of a sampled corpus, one JSON object per line of
/// its `selection.jsonl`.
pub fn rows(directory: &Path) -> Vec<Value> {
    fs::read_to_string(directory.join("selection.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
