// SPDX-License-Identifier: AGPL-3.0-only

//! Tauri WebView wiring (feature `tauri-backend` only).
//!
//! Thin `#[tauri::command]` wrappers over [`commands`](crate::commands); all
//! game/auth logic stays in headless, unit-tested code. Requires the system
//! WebKit/GTK stack — enable only for `cargo tauri dev` / `cargo tauri build`
//! (see the `src-tauri` harness), never for plain `cargo build` / `cargo test`.

use std::sync::Arc;

use tauri::{AppHandle, Emitter, State};

use crate::commands::{self, new_shared_state, JavaProgressEmitter, ProgressEmitter, SharedState};
use crate::state::{
    AccountInfoDto, BuildInfoDto, ConsoleTailDto, DeviceCodeInfo, GameOutcomeDto, GameStatusDto,
    InstallSummaryDto, JavaInfoDto, JavaProgressPayload, JavaRuntimesDto, ManifestStatusDto,
    ModInfo, PackFlavorSwapDto, PatchNotesFeedDto, PlayResultDto, ProgressPayload, ServerStatusDto,
    SessionInfo, SettingsDto, UninstallReportDto, VerifyReportDto, VersionInfo,
};

/// Tauri-managed backend state.
pub struct TauriState {
    /// Mutex-guarded backend (see [`AppState`]).
    pub inner: SharedState,
}

fn emit_of(app: &AppHandle) -> Option<Arc<ProgressEmitter>> {
    let app = app.clone();
    Some(Arc::new(move |p: ProgressPayload| {
        let _ = app.emit("install-progress", &p);
    }))
}

fn java_emit_of(app: &AppHandle) -> Option<Arc<JavaProgressEmitter>> {
    let app = app.clone();
    Some(Arc::new(move |p: JavaProgressPayload| {
        let _ = app.emit("java-progress", &p);
    }))
}

#[tauri::command]
/// Current session (`None` = logged out → login modal).
pub async fn get_session(state: State<'_, TauriState>) -> Result<Option<SessionInfo>, String> {
    Ok(commands::get_session(&state.inner).await)
}

#[tauri::command]
/// Cached manifest status (no network).
pub async fn get_manifest_status(
    state: State<'_, TauriState>,
) -> Result<ManifestStatusDto, String> {
    Ok(commands::get_manifest_status(&state.inner).await)
}

#[tauri::command]
/// Fetch the CDN manifest, update freshness.
pub async fn refresh_manifest(state: State<'_, TauriState>) -> Result<ManifestStatusDto, String> {
    commands::refresh_manifest(&state.inner).await
}

#[tauri::command]
/// Full install / update (`LOGIN_REQUIRED` when logged out).
/// `include_loki` opts into the Loki cheat client (`mods/loki.jar`);
/// `None`/absent (old callers) defaults to `false` — no Loki download.
pub async fn start_install(
    app: AppHandle,
    state: State<'_, TauriState>,
    include_loki: Option<bool>,
) -> Result<InstallSummaryDto, String> {
    commands::start_install(&state.inner, emit_of(&app), include_loki.unwrap_or(false)).await
}

#[tauri::command]
/// File freshness check (no downloads).
pub async fn verify_files(state: State<'_, TauriState>) -> Result<VerifyReportDto, String> {
    commands::verify_files(&state.inner).await
}

#[tauri::command]
/// Uninstall the game payload (settings, runtimes and logs are kept).
pub async fn uninstall_game(state: State<'_, TauriState>) -> Result<UninstallReportDto, String> {
    commands::uninstall_game(&state.inner).await
}

#[tauri::command]
/// Abort the running install, if any (frontend `Annuler` button).
pub async fn cancel_install(state: State<'_, TauriState>) -> Result<(), String> {
    commands::cancel_install(&state.inner).await
}

#[tauri::command]
/// Abort the running Java 8 download, if any (frontend `Annuler` button).
pub async fn cancel_java_download(state: State<'_, TauriState>) -> Result<(), String> {
    commands::cancel_java_download(&state.inner).await
}

#[tauri::command]
/// Current settings.
pub async fn get_settings(state: State<'_, TauriState>) -> Result<SettingsDto, String> {
    Ok(commands::get_settings(&state.inner).await)
}

#[tauri::command]
/// One-shot boot notices (experimental-gate migration toast, if any).
pub async fn get_boot_notices(state: State<'_, TauriState>) -> Result<Vec<String>, String> {
    Ok(commands::get_boot_notices(&state.inner).await)
}

#[tauri::command]
/// Persist settings to `<gameDir>/launcher-settings.json`.
pub async fn save_settings(
    state: State<'_, TauriState>,
    settings: SettingsDto,
) -> Result<SettingsDto, String> {
    commands::save_settings(&state.inner, settings).await
}

#[tauri::command]
/// Switch the pack flavor (`true` = experimental): persists the flags and
/// stashes/restores the modern mod stack on disk immediately (see
/// `commands::set_pack_flavor`). The frontend logs `lines` to the Console.
pub async fn set_pack_flavor(
    state: State<'_, TauriState>,
    experimental: bool,
) -> Result<PackFlavorSwapDto, String> {
    commands::set_pack_flavor(&state.inner, experimental).await
}

#[tauri::command]
/// Probe the effective Java binary.
pub async fn resolve_java_info(state: State<'_, TauriState>) -> Result<JavaInfoDto, String> {
    Ok(commands::resolve_java_info(&state.inner).await)
}

#[tauri::command]
/// Total physical RAM in MiB (memory slider ceiling).
pub async fn get_system_memory(state: State<'_, TauriState>) -> Result<u64, String> {
    Ok(commands::get_system_memory(&state.inner).await)
}

#[tauri::command]
/// Both JDK statuses for the JDK pane (never fails hard).
pub async fn get_java_runtimes(state: State<'_, TauriState>) -> Result<JavaRuntimesDto, String> {
    Ok(commands::get_java_runtimes(&state.inner).await)
}

#[tauri::command]
/// Java 8 readiness of the effective binary (drives the install prompt).
pub async fn check_java_status(state: State<'_, TauriState>) -> Result<JavaInfoDto, String> {
    Ok(commands::check_java_status(&state.inner).await)
}

#[tauri::command]
/// Download the Temurin 8 JRE into `<gameDir>/java` (`java-progress` events).
pub async fn download_java8(
    app: AppHandle,
    state: State<'_, TauriState>,
) -> Result<JavaInfoDto, String> {
    commands::download_java8(&state.inner, java_emit_of(&app)).await
}

#[tauri::command]
/// Download GraalVM 21 into `<gameDir>/java21` (`java-progress` events).
/// Flavor-managed (no manual button): the home Java card provisions the
/// current flavor's runtime through this command on experimental.
pub async fn download_graalvm21(
    app: AppHandle,
    state: State<'_, TauriState>,
) -> Result<JavaInfoDto, String> {
    commands::download_graalvm21(&state.inner, java_emit_of(&app)).await
}

#[tauri::command]
/// Device-code step 1 (returns `verification_uri` + `user_code`).
pub async fn request_login(state: State<'_, TauriState>) -> Result<DeviceCodeInfo, String> {
    commands::request_login(&state.inner).await
}

#[tauri::command]
/// Device-code step 2 (long poll → full session on approval).
pub async fn poll_login(state: State<'_, TauriState>) -> Result<SessionInfo, String> {
    commands::poll_login(&state.inner).await
}

#[tauri::command]
/// Abort the in-flight device-code login, if any (login modal `Annuler`).
/// Best-effort: always succeeds, even when idle.
pub async fn cancel_login(state: State<'_, TauriState>) -> Result<(), String> {
    commands::cancel_login(&state.inner).await
}

#[tauri::command]
/// Restore a session from the keyring refresh token.
pub async fn refresh_session(state: State<'_, TauriState>) -> Result<SessionInfo, String> {
    commands::refresh_session(&state.inner).await
}

#[tauri::command]
/// Forget the ACTIVE session + clear its keyring entry.
pub async fn logout(state: State<'_, TauriState>) -> Result<(), String> {
    commands::logout(&state.inner).await
}

#[tauri::command]
/// All connected accounts for the switcher (active flagged).
pub async fn get_accounts(state: State<'_, TauriState>) -> Result<Vec<AccountInfoDto>, String> {
    Ok(commands::get_accounts(&state.inner).await)
}

#[tauri::command]
/// Switch the active account (refreshes on demand when needed).
pub async fn set_active_account(
    state: State<'_, TauriState>,
    uuid: String,
) -> Result<SessionInfo, String> {
    commands::set_active_account(&state.inner, uuid).await
}

#[tauri::command]
/// Force-refresh ONE account's tokens without changing the active account.
pub async fn refresh_account(
    state: State<'_, TauriState>,
    uuid: String,
) -> Result<AccountInfoDto, String> {
    commands::refresh_account(&state.inner, uuid).await
}

#[tauri::command]
/// Forget ONE account by UUID (session + keyring entry + roster).
pub async fn remove_account(state: State<'_, TauriState>, uuid: String) -> Result<bool, String> {
    Ok(commands::remove_account(&state.inner, uuid).await)
}

#[tauri::command]
/// Avatar PNG through the backend (disk-cached `data:` URL, never a
/// remote WebView fetch; failures fall back to the frontend glyph).
pub async fn get_avatar(state: State<'_, TauriState>, uuid: String) -> Result<String, String> {
    commands::get_avatar(&state.inner, uuid).await
}

#[tauri::command]
/// Mods manager rows (manifest = required/locked, local `.jar` = toggleable).
pub async fn get_mods_list(state: State<'_, TauriState>) -> Result<Vec<ModInfo>, String> {
    commands::get_mods_list(&state.inner).await
}

#[tauri::command]
/// Enable/disable an optional mod (`<id>.jar` ↔ `<id>.jar.disabled`).
pub async fn toggle_mod(
    state: State<'_, TauriState>,
    id: String,
    enabled: bool,
) -> Result<ModInfo, String> {
    commands::toggle_mod(&state.inner, id, enabled).await
}

#[tauri::command]
/// Reveal the `mods` folder in the OS file manager.
pub async fn open_mods_folder(state: State<'_, TauriState>) -> Result<String, String> {
    commands::open_mods_folder(&state.inner).await
}

#[tauri::command]
/// Reveal the game root folder in the OS file manager.
pub async fn open_game_folder(state: State<'_, TauriState>) -> Result<String, String> {
    commands::open_game_folder(&state.inner).await
}

#[tauri::command]
/// Open an allowlisted `https://` URL in the OS default browser
/// (always external, never in the WebView).
pub async fn open_external(url: String) -> Result<String, String> {
    commands::open_external(url).await
}

#[tauri::command]
/// JOUER: login gate → ensure install → detached Forge spawn.
/// Forwards live game output as `console-line` events when the
/// `console_live` setting is on, and the post-launch verdict
/// (`running` / `exited` / `crashed`) as `game-outcome` events
/// (see `commands::play_with_outcome`).
pub async fn play(app: AppHandle, state: State<'_, TauriState>) -> Result<PlayResultDto, String> {
    let app2 = app.clone();
    let console = Arc::new(move |line: String| {
        let _ = app2.emit("console-line", &line);
    });
    let app3 = app.clone();
    let outcome = Arc::new(move |o: crate::state::GameOutcomeDto| {
        let _ = app3.emit("game-outcome", &o);
    });
    commands::play_with_outcome(&state.inner, emit_of(&app), Some(console), Some(outcome)).await
}

#[tauri::command]
/// État du jeu par sondage (`{running:false}` quand inconnu, jamais
/// d'erreur) : secours quand les événements `game-outcome` n'atteignent pas
/// la WebView — le frontend alterne avec l'écouteur existant (inchangé).
pub async fn get_game_status(state: State<'_, TauriState>) -> Result<GameStatusDto, String> {
    Ok(commands::get_game_status(&state.inner).await)
}

#[tauri::command]
/// Dernier `install-progress` émis (`None` quand aucun run n'a encore émis,
/// jamais d'erreur) : secours quand les événements n'atteignent pas la
/// WebView — le frontend sonde toutes les 500 ms pendant l'installation et
/// rejoue le payload via `renderProgress` (événements inchangés).
pub async fn get_install_progress(
    state: State<'_, TauriState>,
) -> Result<Option<ProgressPayload>, String> {
    Ok(commands::get_install_progress(&state.inner).await)
}

#[tauri::command]
/// Dernier `java-progress` émis (`None` quand aucun téléchargement n'a
/// encore émis, jamais d'erreur) : même secours que `get_install_progress`
/// pour la modale Java (`renderJavaProgress`, silencieux).
pub async fn get_java_progress(
    state: State<'_, TauriState>,
) -> Result<Option<JavaProgressPayload>, String> {
    Ok(commands::get_java_progress(&state.inner).await)
}

#[tauri::command]
/// File console sondable (`{ lines, next }`, jamais d'erreur) : secours
/// quand les événements `console-line` n'atteignent pas la WebView — le
/// frontend sonde toutes les 1 s pendant le jeu ou sur l'onglet Console et
/// rejoue via `glog` (événements inchangés, aucun toast du sondage).
pub async fn get_console_tail(
    state: State<'_, TauriState>,
    since: usize,
) -> Result<ConsoleTailDto, String> {
    Ok(commands::get_console_tail(&state.inner, since).await)
}

#[tauri::command]
/// Dernier verdict `game-outcome` (`None` quand aucun lancement n'a encore
/// produit de verdict, jamais d'erreur) : secours quand les événements
/// n'atteignent pas la WebView — le sondage jeu 3 s y rejoue les plantages
/// via le même traitement que l'événement (premier signal gagne).
pub async fn get_last_outcome(
    state: State<'_, TauriState>,
) -> Result<Option<GameOutcomeDto>, String> {
    Ok(commands::get_last_outcome(&state.inner).await)
}

#[tauri::command]
/// Live Server List Ping (`{online:false, error}` on failure, never fake).
pub async fn get_server_status(state: State<'_, TauriState>) -> Result<ServerStatusDto, String> {
    Ok(commands::get_server_status(&state.inner).await)
}

#[tauri::command]
/// Patch-notes feed + reachability verdict (newest first, max 5, never errors).
pub async fn get_patch_notes(state: State<'_, TauriState>) -> Result<PatchNotesFeedDto, String> {
    Ok(commands::get_patch_notes(&state.inner).await)
}

#[tauri::command]
/// Launcher version for the bottom bar (never errors, worst case `"dev"`).
pub async fn get_launcher_version() -> Result<VersionInfo, String> {
    Ok(commands::get_launcher_version().await)
}

#[tauri::command]
/// Tampon d'identité du build (jamais d'erreur, `"?"` quand inconnu) : dit
/// EXACTEMENT quel code la fenêtre exécute — le frontend le journalise.
pub async fn get_build_info() -> BuildInfoDto {
    commands::get_build_info().await
}

/// Mise à jour disponible pour le launcher (`check_launcher_update`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LauncherUpdateDto {
    /// Version publiée (ex. `0.3.0`).
    pub version: String,
    /// Note de version (optionnelle).
    pub notes: Option<String>,
}

#[tauri::command]
/// Vérifie la mise à jour du launcher (manifeste signé `latest.json` de la
/// dernière release GitHub). `None` = à jour, hors-ligne ou manifeste
/// absent : jamais d'erreur, jamais de bruit.
pub async fn check_launcher_update(app: AppHandle) -> Result<Option<LauncherUpdateDto>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let Ok(updater) = app.updater() else {
        return Ok(None);
    };
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(LauncherUpdateDto {
            version: update.version.clone(),
            notes: update.body.clone(),
        })),
        Ok(None) => Ok(None),
        Err(_) => Ok(None),
    }
}

#[tauri::command]
/// Télécharge et installe la mise à jour signée puis relance le launcher
/// (Windows : l'installeur relance l'app ; Linux AppImage/macOS : `restart`
/// ici). `Ok(())` sans effet quand rien n'est à jour.
pub async fn install_launcher_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app
        .updater()
        .map_err(|e| format!("updater indisponible : {e}"))?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Ok(());
    };
    update
        .download_and_install(|_chunk, _total| {}, || {})
        .await
        .map_err(|e| format!("mise à jour impossible : {e}"))?;
    app.restart()
}

/// Linux-only per-view WebKit RAM diet for the main window.
///
/// Single-page launcher with zero canvas/WebGL/audio/video (verified by grep
/// over `frontend/dist`: no `<canvas` / `<audio` / `<video` / `AudioContext` /
/// `WebGL`), so every media/GL/page-cache subsystem below is dead weight.
/// REVERT: delete this fn + its call in `run_app` setup + the `webkit2gtk`
/// dep in `crates/ui/Cargo.toml` — WebKit defaults return, nothing persisted.
#[cfg(target_os = "linux")]
fn apply_webkit_ram_settings(app: &tauri::AppHandle) {
    use tauri::Manager;
    use webkit2gtk::{SettingsExt, WebViewExt};

    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.with_webview(|webview| {
        let view: webkit2gtk::WebView = webview.inner();
        let Some(settings) = view.settings() else {
            return;
        };
        // No back/forward navigation in a single-page launcher: the page
        // cache only pins dead DOM/pages in RAM.
        settings.set_enable_page_cache(false);
        // No `<audio>` / `AudioContext` in the UI: WebAudio graphs + threads.
        settings.set_enable_webaudio(false);
        // No `<canvas>` / WebGL in the UI: GL shaders + GPU buffers.
        settings.set_enable_webgl(false);
        // No camera/mic capture UI: media-stream capture machinery.
        settings.set_enable_media_stream(false);
        // No streaming `<video>` playback: Media Source buffers.
        settings.set_enable_mediasource(false);
        // No DRM video: Encrypted Media CDM modules.
        settings.set_enable_encrypted_media(false);
        // No peer connections: WebRTC threads + ICE/network churn.
        settings.set_enable_webrtc(false);
        // NOTE (wheel-debug): `hardware-acceleration-policy=NEVER` (CPU
        // raster, ~20MB saved) was reverted to the WebKit default while
        // chasing dead mouse-wheel scrolling — see README Troubleshooting.
        // To re-enable: `settings.set_hardware_acceleration_policy(
        // HardwareAccelerationPolicy::Never);` (needs `HardwareAccelerationPolicy`
        // in the `webkit2gtk` import above).
    });
}

/// Run the 46Anarchy Tauri app (blocks until the window closes).
pub fn run_app() -> Result<(), String> {
    let inner = new_shared_state();
    let restore = inner.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(TauriState { inner })
        .setup(|app| {
            // Per-view WebKit RAM diet (Linux-only; other platforms compile
            // untouched). First in setup — before any content loads — so the
            // settings are in place for the first paint.
            // REVERT: delete this call + `apply_webkit_ram_settings` + the
            // `webkit2gtk` dep in `crates/ui/Cargo.toml`.
            #[cfg(target_os = "linux")]
            apply_webkit_ram_settings(app.handle());
            // Silent restore BEFORE the frontend initializes: by the time
            // the first `get_session` poll lands, auth state is settled —
            // the login modal can never flash over a session that is still
            // being restored.
            //
            // DANGER « jeton brûlé » : `refresh_session` fait tourner les
            // jetons côté Microsoft — abandonner son future en plein vol
            // (ancien `timeout` autour de l'appel direct) peut jeter une
            // rotation déjà émise par le serveur dont la réponse est perdue :
            // le trousseau garde alors un jeton brûlé, relu au prochain
            // démarrage comme un `invalid_grant` authentique qui vaporise le
            // compte. Le travail tourne donc dans une tâche détachée qui VA
            // TOUJOURS jusqu'au bout (jetons tournés sauvegardés, événement
            // `session-changed` émis, issue journalisée) ; seul le signal
            // d'achèvement est attendu 10s au maximum. Un dépassement ne borne
            // que le premier affichage — jamais le travail : un succès tardif
            // referme quand même la modale via l'événement existant, un échec
            // tardif laisse la modale (affichée à raison) en place.
            //
            // Chronométrage permanent (une poignée de lignes par démarrage,
            // jamais de bruit) : début/fin du setup + durée de restauration.
            let setup_started = std::time::Instant::now();
            eprintln!(
                "[launcher] démarrage setup t={}ms",
                crate::state::boot_wall_ms()
            );
            let handle = app.handle().clone();
            let (settled_tx, settled_rx) = tokio::sync::oneshot::channel::<()>();
            tauri::async_runtime::spawn(async move {
                let restore_started = std::time::Instant::now();
                match commands::refresh_session(&restore).await {
                    Ok(info) => {
                        eprintln!(
                            "[launcher] démarrage restauration session '{}' en {} ms t={}ms",
                            info.name,
                            restore_started.elapsed().as_millis(),
                            crate::state::boot_wall_ms()
                        );
                        let _ = handle.emit("session-changed", &info);
                    }
                    Err(e) => {
                        eprintln!(
                            "[launcher] restauration de session impossible en {} ms : {e}",
                            restore_started.elapsed().as_millis()
                        );
                    }
                }
                let _ = settled_tx.send(());
            });
            let outcome = tauri::async_runtime::block_on(async move {
                tokio::time::timeout(std::time::Duration::from_secs(10), settled_rx).await
            });
            if outcome.is_err() {
                eprintln!(
                    "[launcher] restauration de session : délai dépassé, démarrage sans session (la restauration continue en tâche de fond)."
                );
            }
            eprintln!(
                "[launcher] démarrage setup fin en {} ms t={}ms",
                setup_started.elapsed().as_millis(),
                crate::state::boot_wall_ms()
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_session,
            get_manifest_status,
            refresh_manifest,
            start_install,
            verify_files,
            uninstall_game,
            cancel_install,
            cancel_java_download,
            get_settings,
            get_boot_notices,
            save_settings,
            set_pack_flavor,
            resolve_java_info,
            check_java_status,
            get_system_memory,
            get_java_runtimes,
            download_java8,
            download_graalvm21,
            request_login,
            poll_login,
            cancel_login,
            refresh_session,
            logout,
            get_accounts,
            set_active_account,
            refresh_account,
            remove_account,
            get_avatar,
            play,
            get_game_status,
            get_install_progress,
            get_java_progress,
            get_console_tail,
            get_last_outcome,
            get_mods_list,
            toggle_mod,
            open_mods_folder,
            open_game_folder,
            open_external,
            get_server_status,
            get_patch_notes,
            get_launcher_version,
            get_build_info,
            check_launcher_update,
            install_launcher_update
        ])
        // Path relative to `crates/ui/` (this crate's manifest dir).
        .run(tauri::generate_context!("../../src-tauri/tauri.conf.json"))
        .map_err(|e| format!("erreur Tauri : {e}"))
}
