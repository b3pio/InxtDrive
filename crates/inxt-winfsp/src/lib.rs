//! Windows drive-letter adapter for the InxtDrive engine, built on WinFsp.

#[cfg(windows)]
mod fs;
#[cfg(windows)]
mod security;

#[cfg(windows)]
pub use fs::{Drive, MountOptions};
