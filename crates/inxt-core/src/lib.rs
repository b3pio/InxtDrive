//! Platform-independent engine behind the virtual drive.
//!
//! The engine keeps the whole remote namespace in memory (persisted to
//! SQLite), serves reads from a sparse block cache that is filled on demand
//! with ranged, decrypted downloads, and pushes local changes back to the
//! cloud in the background. Filesystem adapters (WinFsp on Windows, FUSE on
//! Linux) only translate OS callbacks into [`Engine`] calls.

pub mod cache;
pub mod engine;
pub mod names;
mod platform;
pub use platform::private_dir;
mod push;
mod remote;
pub mod tree;

pub use engine::{Activity, Attr, Change, ChangeKind, Engine, EngineConfig, Finished, FsError, Problem, Status, Transfer};
pub use tree::{NodeId, ROOT};

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
