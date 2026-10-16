// SPDX-License-Identifier: AGPL-3.0-only

//! Top-level `launcher` binary. All game logic lives in the workspace crates:
//! `launcher-core` (manifest/download/java/launch), `launcher-auth`
//! (Microsoft/Xbox/Minecraft), `launcher-ui` (Tauri v2 backend + static
//! HTML/CSS/JS frontend in `crates/ui/frontend/dist`).

use launcher_ui::run;

fn main() {
    // `PRISM_MSA_CLIENT_ID` passthrough: the backend resolves it at login time
    // via `AuthConfig::from_env` (runtime env, else the compile-time value
    // baked in by `launcher-auth`). Log which source is active without ever
    // printing the secret itself.
    match std::env::var("PRISM_MSA_CLIENT_ID") {
        Ok(v) if !v.trim().is_empty() => {
            eprintln!("46Anarchy launcher (client-ID Azure via PRISM_MSA_CLIENT_ID).");
        }
        _ => {
            eprintln!("46Anarchy launcher (pas de PRISM_MSA_CLIENT_ID : utilisez les réglages).");
        }
    }
    if let Err(e) = run() {
        eprintln!("erreur lancement : {e}");
        std::process::exit(1);
    }
}
