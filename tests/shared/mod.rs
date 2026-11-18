//! The test data every SDK shares: the contract (contract/) and the scenarios (scenarios/). Both live in the
//! monorepo's sdk/ folder, found by walking up from this crate. To run the tests from a copy of this SDK outside
//! the monorepo, set GRVT_SDK_SHARED to a folder that holds contract/ and scenarios/.
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("GRVT_SDK_SHARED") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|dir| dir.join("contract/contract.json").is_file() && dir.join("scenarios").is_dir())
        .expect("no contract/ and scenarios/ above this SDK; set GRVT_SDK_SHARED")
        .to_path_buf()
}

/// Parses a shared JSON file, e.g. `load("scenarios/orderbook.json")`.
pub fn load(path: &str) -> serde_json::Value {
    let file = root().join(path);
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
}
