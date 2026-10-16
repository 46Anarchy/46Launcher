// SPDX-License-Identifier: AGPL-3.0-only

//! `launcher-ui`: Tauri v2 backend for the 46Anarchy launcher.
//!
//! [`state`] holds the mutex-guarded backend value (headless, unit-tested);
//! [`commands`] implements every `invoke` command as plain `async` functions;
//! [`tauri_app`] (feature `tauri-backend` only) adds the WebView wiring.
//! The frontend is static HTML/CSS/JS in `frontend/dist` (no npm build).
//!
//! Entry point: [`run`], called from the top-level `launcher` binary.
//! `PRISM_MSA_CLIENT_ID` needs no explicit plumbing here: [`state`] resolves
//! it at login time via `AuthConfig::from_env` (env → build default), unless
//! the user set a custom client-ID in the settings screen.

pub mod commands;
pub mod patch_notes;
pub mod server_status;
pub mod state;
#[cfg(feature = "tauri-backend")]
pub mod tauri_app;

pub use state::{AppState, SettingsDto};

/// Launch the 46Anarchy app.
///
/// - Feature `tauri-backend` (system WebKit present): full Tauri WebView app.
/// - Default portable build: no WebView linked; prints how to run the UI
///   (Tauri dev harness or static frontend preview) and exits successfully.
#[cfg(feature = "tauri-backend")]
pub fn run() -> Result<(), String> {
    tauri_app::run_app()
}

/// Portable fallback (see [`run`]).
#[cfg(not(feature = "tauri-backend"))]
pub fn run() -> Result<(), String> {
    eprintln!("46Anarchy launcher (build portable : WebView Tauri non compilé).");
    eprintln!("- App complète : `cargo tauri dev` (tauri-cli + WebKit système),");
    eprintln!("  ou `cargo run -p launcher --features launcher-ui/tauri-backend`.");
    eprintln!("- Aperçu statique de l'interface :");
    eprintln!("  `python3 -m http.server 1420 --directory crates/ui/frontend/dist`");
    eprintln!("  puis ouvrez http://localhost:1420 (prévisualisation statique :");
    eprintln!("  backend Tauri indisponible, ouvrez dans l'app Tauri).");
    Ok(())
}
