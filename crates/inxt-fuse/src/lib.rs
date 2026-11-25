//! Linux folder adapter for the InxtDrive engine, built on FUSE.

#[cfg(target_os = "linux")]
mod fs;

#[cfg(target_os = "linux")]
pub use fs::{Drive, MountOptions};
