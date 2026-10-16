// SPDX-License-Identifier: AGPL-3.0-only

//! Shared error type for `launcher-core`.

use thiserror::Error;

/// Errors produced by manifest parsing, hashing, downloads and instance bookkeeping.
#[derive(Error, Debug)]
pub enum CoreError {
    /// HTTP / transport failure (reqwest).
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// `manifest.json` / `cdn_*.json` deserialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Downloaded (or cached) bytes do not match the manifest `sha1`.
    #[error("hash mismatch for {file}: expected {expected}, got {actual}")]
    HashMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    /// CDN-level failure (unreachable manifest, empty payload, ...).
    #[error("cdn failure: {0}")]
    CdnFailure(String),
    /// A parallel download task panicked / was cancelled.
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// The user cancelled the run (the frontend `Annuler` button sets the
    /// shared flag; in-flight attempts finish, the rest never starts).
    /// Carries the full French sentence (e.g. `"Installation annulée."`).
    #[error("{0}")]
    Cancelled(String),
    /// A manifest-provided path escapes the game dir (absolute path,
    /// parent-dir traversal, Windows prefix / root).
    #[error("unsafe path rejected: {0}")]
    UnsafePath(String),
}

/// Convenience alias used across the crate's public API.
pub type Result<T> = std::result::Result<T, CoreError>;
