// SPDX-License-Identifier: AGPL-3.0-only

//! Build script: runs `tauri_build` (provides `OUT_DIR` for
//! `tauri::generate_context!`) only with the `tauri-backend` feature.
//! Plain `cargo build` / `cargo test` never compile the `tauri_app`
//! module, so they skip this and stay portable (no system webkit).
//!
//! The Tauri config lives in the `src-tauri` harness, not in this crate,
//! so it is pointed at explicitly (paths relative to this manifest dir).

/// Émet `LAUNCHER_GIT_HASH` (hash court HEAD, `dev` en repli) sans
/// jamais faire échouer la compilation (ce dépôt peut ne pas avoir de HEAD).
fn emit_launcher_git_hash() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=LAUNCHER_GIT_HASH={hash}");
    for candidate in ["../../.git/HEAD", ".git/HEAD"] {
        if std::path::Path::new(candidate).exists() {
            println!("cargo:rerun-if-changed={candidate}");
        }
    }
    println!("cargo:rerun-if-env-changed=LAUNCHER_GIT_HASH");
    println!("cargo:rerun-if-env-changed=LAUNCHER_GITHUB_REPO");
}

/// FNV-1a 32 bits sur les octets (8 hex minuscules). Doit rester
/// identique à `build_identity_hash` dans `src/commands.rs` et au
/// snippet Python du rapport (même constante, même ordre des fichiers).
fn build_identity_hash(bytes: &[u8]) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// Hache les fichiers existants dans l'ordre donné (introuvables ignorés).
/// `None` quand aucun octet n'a été lu (ne fait jamais échouer le build).
fn hash_files_in_order(paths: &[std::path::PathBuf]) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    for p in paths {
        if let Ok(bytes) = std::fs::read(p) {
            buf.extend_from_slice(&bytes);
        }
    }
    if buf.is_empty() { None } else { Some(build_identity_hash(&buf)) }
}

/// Émet `LAUNCHER_FRONTEND_HASH` (triplet `frontend/dist/`) et
/// `LAUNCHER_BACKEND_HASH` (`src/*.rs` triés) sans jamais faire échouer
/// la compilation (`"?"` quand un groupe est illisible ou absent).
fn emit_build_identity() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let frontend: Vec<std::path::PathBuf> = ["main.js", "index.html", "styles.css"]
        .iter()
        .map(|n| manifest.join("frontend").join("dist").join(n))
        .collect();
    let mut backend: Vec<std::path::PathBuf> = std::fs::read_dir(manifest.join("src"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "rs"))
                .collect()
        })
        .unwrap_or_default();
    backend.sort();
    for p in frontend.iter().chain(backend.iter()) {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!(
        "cargo:rustc-env=LAUNCHER_FRONTEND_HASH={}",
        hash_files_in_order(&frontend).unwrap_or_else(|| "?".to_string())
    );
    println!(
        "cargo:rustc-env=LAUNCHER_BACKEND_HASH={}",
        hash_files_in_order(&backend).unwrap_or_else(|| "?".to_string())
    );
}

#[cfg(feature = "tauri-backend")]
fn main() {
    emit_launcher_git_hash();
    emit_build_identity();
    tauri_build::try_build(
        tauri_build::Attributes::new().config_path("../../src-tauri/tauri.conf.json"),
    )
    .expect("tauri build script failed")
}

#[cfg(not(feature = "tauri-backend"))]
fn main() {
    emit_launcher_git_hash();
    emit_build_identity();
}
