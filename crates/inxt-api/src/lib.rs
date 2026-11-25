//! Client for Internxt Drive.
//!
//! Internxt is end-to-end encrypted: file contents are encrypted on the client
//! with AES-256-CTR using keys derived from the account mnemonic, and the server
//! only ever sees ciphertext. This crate implements the same protocol the
//! official open-source clients use.

pub mod auth;
pub mod client;
pub mod crypto;
pub mod drive;
pub mod error;
pub mod jwt;
pub mod network;
pub mod share;
pub mod types;

pub use client::{Client, ClientConfig, Credentials};
pub use error::{ApiError, Result};
