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
    if buf.is_empty() {
        None
    } else {
        Some(build_identity_hash(&buf))
    }
}

/// Émet `LAUNCHER_FRONTEND_HASH` (triplet `frontend/dist/`) et
/// `LAUNCHER_BACKEND_HASH` (`src/*.rs` triés) sans jamais faire échouer
/// la compilation (`"?"` quand un groupe est illisible ou absent).
fn emit_build_identity() {
    let manifest =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
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

/// Minifie `frontend/dist/` EN PLACE, uniquement quand
/// `PROFILE == "release"` (`cargo tauri build`). En `debug`
/// (`cargo tauri dev`, `cargo test`, `cargo check`), ne fait rien : les
/// fichiers lisibles de `frontend/dist/` sont servis intacts.
///
/// Ordre d'exécution (vérifié) : le bundler Tauri lit `frontendDist`
/// (`../../src-tauri/tauri.conf.json` → `"../crates/ui/frontend/dist"`) au
/// moment du bundle, tandis que ce build script appartient à `launcher-ui`,
/// dont dépend le harness `src-tauri` (`src-tauri/Cargo.toml` :
/// `launcher-ui = { path = "../crates/ui", features = ["tauri-backend"] }`).
/// Cargo compile donc `launcher-ui` — et exécute ce script — AVANT que
/// Tauri n'embarque `frontend/dist`. La minification a lieu avant
/// `emit_build_identity` pour que `LAUNCHER_FRONTEND_HASH` décrive le code
/// réellement empaqueté.
///
/// Sûreté : `minifier::{js,css}::minify` ne fait que tokeniser puis
/// supprimer commentaires et espaces superflus — aucune réécriture
/// sémantique, aucun renommage de variable/fonction (les `id` chaînes et
/// les appels DOM de `main.js` survivent à comportement identique ;
/// `main.js` termine chaque instruction par `;`, donc pas de piège ASI ;
/// `styles.css` n'utilise ni `calc()` ni URI embarquée). `html-minifier`
/// (qui délègue le JS/CSS inline au même `minifier`) ne fait que replier
/// les espaces et retirer les commentaires HTML ; le contenu de
/// `<pre>`/`<textarea>` est préservé et `index.html` n'a aucun
/// `<script>`/`<style>` inline.
///
/// Conséquence d'arbre sale : un build release réécrit `frontend/dist/`
/// (fichiers suivis → `git status` les montre modifiés ; la passe est
/// idempotente : relancer le build converge). Restaurer après un build
/// local (`git checkout -- crates/ui/frontend/dist/`). La CI
/// (`.github/workflows/release.yml`) part d'un checkout frais
/// (`actions/checkout@v4`) et ne commite jamais : les fichiers minifiés y
/// meurent avec le runner éphémère, donc aucun changement CI requis.
///
/// Ne fait jamais échouer le build : fichier absent/illisible, entrée non
/// UTF-8 ou erreur de minification → `cargo:warning`, fichier gardé tel quel.
fn minify_frontend_for_release() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    println!("cargo:rerun-if-env-changed=PROFILE");
    let manifest =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dist = manifest.join("frontend").join("dist");
    if let Some(src) = read_utf8_for_minify(&dist.join("main.js")) {
        let minified = minifier::js::minify(&src).to_string();
        write_if_shrunk(&dist.join("main.js"), src.len(), minified.as_bytes());
    }
    if let Some(src) = read_utf8_for_minify(&dist.join("styles.css")) {
        match minifier::css::minify(&src) {
            Ok(minified) => {
                write_if_shrunk(
                    &dist.join("styles.css"),
                    src.len(),
                    minified.to_string().as_bytes(),
                );
            }
            Err(e) => println!(
                "cargo:warning=frontend minify: styles.css not minified ({e}), left untouched"
            ),
        }
    }
    match std::fs::read(dist.join("index.html")) {
        Ok(bytes) => {
            let mut html = html_minifier::HTMLMinifier::new();
            match html.digest(&bytes) {
                Ok(()) => {
                    let original_len = bytes.len();
                    write_if_shrunk(&dist.join("index.html"), original_len, html.get_html());
                }
                Err(e) => println!(
                    "cargo:warning=frontend minify: index.html not minified ({e}), left untouched"
                ),
            }
        }
        Err(e) => println!("cargo:warning=frontend minify: cannot read index.html ({e}), skipped"),
    }
}

/// Lit un fichier texte pour la minification (`None` + warning si absent
/// ou non UTF-8 : le build continue avec le fichier d'origine).
fn read_utf8_for_minify(path: &std::path::Path) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(src) => Some(src),
            Err(e) => {
                println!(
                    "cargo:warning=frontend minify: {} is not valid UTF-8 ({e}), left untouched",
                    path.display()
                );
                None
            }
        },
        Err(e) => {
            println!(
                "cargo:warning=frontend minify: cannot read {} ({e}), skipped",
                path.display()
            );
            None
        }
    }
}

/// Réécrit le fichier uniquement si la sortie est non vide et plus petite
/// (garde-fou : une sortie vide ou gonflée signale un problème → on garde
/// l'original et on avertit au lieu de casser le bundle).
fn write_if_shrunk(path: &std::path::Path, original_len: usize, minified: &[u8]) {
    if minified.is_empty() && original_len > 0 {
        println!(
            "cargo:warning=frontend minify: {} minified to empty, left untouched",
            path.display()
        );
        return;
    }
    if minified.len() >= original_len {
        return;
    }
    if let Err(e) = std::fs::write(path, minified) {
        println!(
            "cargo:warning=frontend minify: cannot write {} ({e})",
            path.display()
        );
    }
}

#[cfg(feature = "tauri-backend")]
fn main() {
    emit_launcher_git_hash();
    minify_frontend_for_release();
    emit_build_identity();
    // Only the final binary (harness) crate may run tauri_build's Windows
    // resource embedding: this library crate has no bins, so its own
    // `resource.lib` falls back to `rustc-link-lib` (which propagates
    // transitively), while the harness links its own copy too — link.exe
    // then fails with CVT1100 duplicate resource (type:VERSION) → LNK1123.
    // Skipped on Windows targets via the TARGET triple (correct even when
    // cross-compiling); Linux/macOS behavior is unchanged.
    // `generate_context!` keeps working without it: it reads the config
    // file directly, capabilities resolve from `src-tauri/capabilities/`,
    // and the target triple falls back to host (correct on native builds).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        tauri_build::try_build(
            tauri_build::Attributes::new().config_path("../../src-tauri/tauri.conf.json"),
        )
        .expect("tauri build script failed")
    }
}

#[cfg(not(feature = "tauri-backend"))]
fn main() {
    emit_launcher_git_hash();
    minify_frontend_for_release();
    emit_build_identity();
}
