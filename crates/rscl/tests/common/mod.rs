//! Helpers shared by the integration tests.
#![allow(dead_code)]
use std::path::{Path, PathBuf};

/// Where `cargo` put the rscl binary and the demo bridge library.
pub fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_rscl"))
        .parent()
        .unwrap()
        .to_path_buf()
}
