//! Core library for **harness-lock** (`hlock`).
//!
//! Implements the manifest, the lockfile, the content-addressed store, task
//! packs, the deterministic reference provider/harness, the search layer, and
//! the tuner — the same v0.1 surface as the (now reference) Python
//! implementation, in Rust. The [`flyte`] backend and the PyO3 Python bindings
//! live in sibling crates.
//!
//! The Python-facing API mirrors [`lock`](tuner::Tuner::tune) and the lockfile
//! model so the user surface stays stable across the language boundary.

pub mod adapters;
pub mod capture;
pub mod contract;
pub mod discover;
pub mod launch;
pub mod localize;
pub mod error;
pub mod hash;
pub mod lockfile;
pub mod manifest;
pub mod ops;
pub mod peripherals;
pub mod profiles;
pub mod providers;
pub mod search;
pub mod store;
pub mod sync;
pub mod tasks;
pub mod tuner;
pub mod worker;

#[cfg(feature = "flyte")]
pub mod flyte;

pub use adapters::HarnessAdapter;
pub use error::{Error, Result};
pub use hash::{content_hash, sha256_hex, sha256_text};
pub use lockfile::Lockfile;
pub use manifest::{find_manifest, Manifest};
pub use profiles::Registry;
pub use store::Store;
pub use tuner::Tuner;
pub use worker::Backend;

/// Schema version of the `harness.lock` lockfile.
pub const LOCK_VERSION: i64 = 1;
/// Default output filename for the lockfile.
pub const LOCKFILE_PATH: &str = "harness.lock";
/// Default manifest filename.
pub const MANIFEST_PATH: &str = "harness.toml";
