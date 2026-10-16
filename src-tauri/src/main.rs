// SPDX-License-Identifier: AGPL-3.0-only

//! 46Anarchy — Tauri harness entry point (used by `cargo tauri dev/build`).
//!
//! The backend (state + invoke commands) lives in `launcher-ui`
//! (feature `tauri-backend`); this binary only forwards to it.

fn main() {
    if let Err(e) = launcher_ui::run() {
        eprintln!("erreur Tauri : {e}");
        std::process::exit(1);
    }
}
