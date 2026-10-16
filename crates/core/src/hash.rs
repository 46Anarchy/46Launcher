// SPDX-License-Identifier: AGPL-3.0-only

//! SHA1 helpers: hex digests + manifest `sha1` verification.

use std::io::Read;
use std::path::Path;

use sha1::{Digest, Sha1};

use crate::error::{CoreError, Result};

/// Hex SHA1 of in-memory bytes (lowercase).
pub fn sha1_hex(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// `true` when `data` hashes to `expected` (comparison is case-insensitive and
/// ignores surrounding whitespace, as CDN values are lowercase hex).
pub fn verify_bytes(data: &[u8], expected: &str) -> bool {
    sha1_hex(data).eq_ignore_ascii_case(expected.trim())
}

/// Stream `path` through SHA1 without loading it fully into memory.
pub fn hash_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Check a file on disk against the manifest `sha1`.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch (no error — callers
/// treat `false` as "needs re-download"). Only hard I/O failures are `Err`.
pub fn verify_file(path: &Path, expected_sha1: &str) -> Result<bool> {
    Ok(hash_file(path)?.eq_ignore_ascii_case(expected_sha1.trim()))
}

/// Like [`verify_file`] but returns [`CoreError::HashMismatch`] on mismatch,
/// for call sites that want to fail the download instead of re-queueing.
pub fn ensure_file_hash(path: &Path, expected_sha1: &str) -> Result<()> {
    let actual = hash_file(path)?;
    if actual.eq_ignore_ascii_case(expected_sha1.trim()) {
        Ok(())
    } else {
        Err(CoreError::HashMismatch {
            file: path.display().to_string(),
            expected: expected_sha1.to_string(),
            actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sha1_known_vector() {
        // Well-known FIPS test vector: SHA1("abc").
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert!(verify_bytes(
            b"abc",
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        ));
        assert!(verify_bytes(
            b"abc",
            "A9993E364706816ABA3E25717850C26C9CD0D89D"
        ));
        assert!(!verify_bytes(
            b"abc",
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        ));
    }

    #[test]
    fn verify_file_roundtrip() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hello 46anarchy").unwrap();
        tmp.flush().unwrap();
        let expected = sha1_hex(b"hello 46anarchy");
        assert!(verify_file(tmp.path(), &expected).unwrap());
        assert!(!verify_file(tmp.path(), &sha1_hex(b"something else")).unwrap());
    }
}
