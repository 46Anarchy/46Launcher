// SPDX-License-Identifier: AGPL-3.0-only

//! Tauri `invoke` backend logic as plain `async` functions.
//!
//! Everything here operates on [`SharedState`] and plain DTOs — no WebView
//! types — so `cargo test` exercises the real backend headlessly. The thin
//! `#[tauri::command]` wrappers live in [`tauri_app`](crate::tauri_app)
//! (feature `tauri-backend` only) and only add event emission.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use launcher_auth::{
    authenticate_xbl, authorize_xsts, fetch_profile, login_with_xbox, poll_device_token,
    request_device_code, AuthConfig, DeviceCodeResponse, MinecraftSession,
};
use launcher_core::{
    build_client, check_java, compare_manifests, download_files, fetch_manifest, java8_status,
    java_warning_for, load_manifest, needs_update, plan_downloads_for_current_os, provision_java8,
    save_manifest, DownloadSummary, JavaProgressCallback, Manifest, UpdateStatus,
    DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_RETRIES, MANIFEST_URL,
};
use tokio::sync::{mpsc, Mutex};

use crate::state::{
    freshness_against_cache, missing_client_id_help, save_settings_file, settings_path,
    AccountInfoDto, AppState, BuildInfoDto, ConsoleTailDto, DeviceCodeInfo, GameOutcomeDto,
    InstallSummaryDto, JavaInfoDto, JavaProgressPayload, JavaRuntimesDto, ManifestStatusDto,
    ModInfo, PackFlavorSwapDto, PatchNotesFeedDto, PlayResultDto, ProgressPayload, ServerStatusDto,
    SessionInfo, SettingsDto, UninstallReportDto, VerifyReportDto, VersionInfo,
};

/// Mutex-guarded backend state shared with the Tauri app.
pub type SharedState = Arc<Mutex<AppState>>;

/// Progress sink for installs (`install-progress` events in the Tauri shell).
pub type ProgressEmitter = dyn Fn(ProgressPayload) + Send + Sync;

/// Game console line sink (`console-line` events in the Tauri shell; the
/// game still outlives us). Live tail honors the `console_live` setting;
/// crash-error lines always surface when a sink is wired.
pub type ConsoleEmitter = dyn Fn(String) + Send + Sync;

/// Progress sink for Java 8 downloads (`java-progress` events in the Tauri shell).
pub type JavaProgressEmitter = dyn Fn(JavaProgressPayload) + Send + Sync;

/// Build a fresh shared state (used by the Tauri `manage` hook and tests).
pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(AppState::boot()))
}

fn http_client() -> Result<reqwest::Client, String> {
    build_client().map_err(|e| format!("Client HTTP : {e}"))
}

fn client_id_locked(state: &AppState) -> Result<String, String> {
    crate::state::effective_client_id(&state.settings).ok_or_else(missing_client_id_help)
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Current session view (`None` = logged out → frontend shows the login modal).
pub async fn get_session(state: &SharedState) -> Option<SessionInfo> {
    state.lock().await.active_session().map(SessionInfo::from)
}

/// All known accounts for the switcher: live sessions first, then
/// roster-only entries (restored lazily on switch), active flagged.
/// Roster entries never saved live this boot still appear (inactive).
pub async fn get_accounts(state: &SharedState) -> Vec<AccountInfoDto> {
    let s = state.lock().await;
    let active = s.active_session().map(|a| a.uuid.clone());
    let mut out: Vec<AccountInfoDto> = s
        .sessions
        .iter()
        .map(|a| crate::state::AccountInfoDto {
            uuid: a.uuid.clone(),
            name: a.name.clone(),
            active: Some(a.uuid.clone()) == active,
        })
        .collect();
    for r in &s.settings.accounts {
        if !out.iter().any(|a| a.uuid == r.uuid) {
            out.push(crate::state::AccountInfoDto {
                uuid: r.uuid.clone(),
                name: r.name.clone(),
                active: false,
            });
        }
    }
    out
}

/// Forget the ACTIVE session + clear its keyring entry (ok if absent).
/// Other connected accounts are untouched (see [`remove_account`]).
pub async fn logout(state: &SharedState) -> Result<(), String> {
    let uuid = {
        let s = state.lock().await;
        match s.active_session() {
            Some(a) => a.uuid.clone(),
            None => return Ok(()),
        }
    };
    remove_account(state, uuid).await;
    state.lock().await.pending_login = None;
    Ok(())
}

/// Forget ONE account by UUID: drops its session, clears its keyring
/// entry, prunes the roster, and persists settings. Returns `false`
/// (no error) for unknown UUIDs.
pub async fn remove_account(state: &SharedState, uuid: String) -> bool {
    let game_dir;
    {
        let mut s = state.lock().await;
        let had_session = s.remove_session(&uuid);
        // Explicit user deletion: `sync_roster_locked` merges (it never
        // deletes), so the voluntary removal prunes the roster entry here —
        // including roster-only (quarantined) entries, which have no session.
        let before = s.settings.accounts.len();
        s.settings.accounts.retain(|a| a.uuid != uuid);
        if !had_session && s.settings.accounts.len() == before {
            return false;
        }
        sync_roster_locked(&mut s);
        game_dir = s.game_dir();
    }
    let _ = launcher_auth::clear_refresh_token(Some(&uuid));
    persist_roster(&game_dir, state).await;
    true
}

/// Switch the active account. Refreshes it on demand when its session
/// data is absent from memory (roster-only entry): same silent chain as
/// boot restore, same re-login routing when its token is dead.
pub async fn set_active_account(state: &SharedState, uuid: String) -> Result<SessionInfo, String> {
    let known = {
        let mut s = state.lock().await;
        if !s.set_active_account(&uuid) {
            return Err("Compte inconnu.".to_string());
        }
        s.active_session().is_some()
    };
    if known {
        let game_dir;
        {
            let mut s = state.lock().await;
            sync_roster_locked(&mut s);
            game_dir = s.game_dir();
        }
        persist_roster(&game_dir, state).await;
        return Ok(SessionInfo::from(
            state.lock().await.active_session().expect("just set"),
        ));
    }
    // Roster-only entry (fresh boot before its restore): refresh now.
    refresh_session_for(state, Some(uuid.clone())).await
}

/// Force-refresh ONE account's Microsoft/Minecraft tokens WITHOUT changing
/// which account is active (unlike [`refresh_session_for`], which activates).
/// Used by the Comptes view's per-row "Actualiser" button; selection itself
/// lives only in the top-right avatar menu ([`set_active_account`]).
pub async fn refresh_account(state: &SharedState, uuid: String) -> Result<AccountInfoDto, String> {
    // Verrou court : on ne garde que le `client_id`, la lecture trousseau
    // (D-Bus, synchrone) se fait hors mutex pour ne pas sérialiser les
    // autres commandes pendant le round-trip.
    let client_id = {
        let s = state.lock().await;
        client_id_locked(&s)?
    };
    let token = launcher_auth::load_refresh_token(Some(&uuid))
        .map_err(|e| format!("Stockage sécurisé : {e}"))?
        .ok_or_else(|| "Compte inconnu.".to_string())?;
    if token.trim().is_empty() {
        return Err("Compte inconnu.".to_string());
    }
    let refresh =
        launcher_auth::refresh_session(&AuthConfig::with_client_id(client_id), &token).await;
    let session = match refresh {
        Ok(s) => s,
        Err(e) => {
            // Dead refresh token (revoked/expired/rotated elsewhere):
            // drop the corpse so we never retry it, and route to re-login.
            // Anything else (network, parsing) keeps the token for retry.
            if launcher_auth::is_invalid_grant(&e) {
                let _ = launcher_auth::clear_refresh_token(Some(&uuid));
                // Jeton mort : mise en quarantaine — la session cadavre est
                // abandonnée mais l'entrée du registre est conservée (visible
                // en Comptes, reconnexion possible au lieu de disparaître).
                let game_dir;
                {
                    let mut s = state.lock().await;
                    s.quarantine_dead_session(&uuid);
                    game_dir = s.game_dir();
                }
                persist_roster(&game_dir, state).await;
                return Err("Session expirée — reconnectez ce compte.".to_string());
            }
            return Err(e.to_string());
        }
    };
    // Écriture trousseau (D-Bus) hors mutex : le verrou ne couvre que la
    // mutation mémoire ci-dessous.
    let _ = launcher_auth::save_refresh_token(Some(session.uuid.as_str()), &session.refresh_token);
    let (dto, game_dir) = {
        let mut s = state.lock().await;
        // NOTE: no `set_active_account` here — a manual refresh upserts the
        // session in place and must never steal the active pointer.
        let uuid = session.uuid.clone();
        let name = session.name.clone();
        s.upsert_session(session);
        sync_roster_locked(&mut s);
        // Read back against the stored state (the upsert activates a first
        // account when none was set): the flag must be truthful.
        let active = s.active_session().map(|a| a.uuid.clone()) == Some(uuid.clone());
        (AccountInfoDto { uuid, name, active }, s.game_dir())
    };
    persist_roster(&game_dir, state).await;
    Ok(dto)
}

/// Keep the persisted roster (uuid + name, no secrets) mirroring the live
/// sessions. MERGE, never a dry rebuild: live sessions first, then roster
/// entries without a session (quarantined accounts, accounts not yet restored
/// this boot) are preserved — a sync must never erase an account. Explicit
/// deletion (`remove_account`) prunes the roster entry itself before calling
/// here. Callers persist the file right after (needs `&SharedState`).
fn sync_roster_locked(s: &mut AppState) {
    let mut merged: Vec<crate::state::AccountRef> = s
        .sessions
        .iter()
        .map(|a| crate::state::AccountRef {
            uuid: a.uuid.clone(),
            name: a.name.clone(),
        })
        .collect();
    for kept in &s.settings.accounts {
        if !merged.iter().any(|a| a.uuid == kept.uuid) {
            merged.push(kept.clone());
        }
    }
    s.settings.accounts = merged;
    s.settings.active_account = s.active_session().map(|a| a.uuid.clone());
}

async fn persist_roster(game_dir: &std::path::Path, state: &SharedState) {
    let settings = state.lock().await.settings.clone();
    let _ = save_settings_file(&settings_path(game_dir), &settings);
}

/// `true` when the session is older than the Minecraft token lifetime
/// and must be refreshed before launch (Prism behavior).
pub fn session_is_stale(session: &MinecraftSession) -> bool {
    session.obtained.elapsed() > std::time::Duration::from_secs(crate::state::SESSION_MAX_AGE_SECS)
}

/// Restore a session from the keyring refresh token (silent boot restore).
/// Legacy single-account path first (default profile + one-time migration),
/// then the roster: the active roster entry (else the first one) is
/// refreshed when the default profile holds nothing. This covers logins
/// stored under UUID profiles by newer builds.
pub async fn refresh_session(state: &SharedState) -> Result<SessionInfo, String> {
    match refresh_session_for(state, None).await {
        Ok(info) => {
            migrate_legacy_default_token(state).await;
            Ok(info)
        }
        Err(e) => {
            let candidate = {
                let s = state.lock().await;
                restore_candidate(&s.settings.accounts, s.settings.active_account.as_deref())
            };
            match candidate {
                Some(uuid) => refresh_session_for(state, Some(uuid)).await,
                None => Err(e),
            }
        }
    }
}

/// Pick which roster entry to restore: the active one when still listed,
/// else the first roster entry, else nothing. Pure (unit-tested).
pub fn restore_candidate(
    accounts: &[crate::state::AccountRef],
    active: Option<&str>,
) -> Option<String> {
    if accounts.is_empty() {
        return None;
    }
    if let Some(want) = active {
        if accounts.iter().any(|a| a.uuid == want) {
            return Some(want.to_string());
        }
    }
    accounts.first().map(|a| a.uuid.clone())
}

/// Profile-scoped restore (`None` = legacy default profile, `Some(uuid)` =
/// per-account entry). On success the session is upserted, activated, and
/// the roster persisted. Shared by boot restore, switching, and staleness.
pub async fn refresh_session_for(
    state: &SharedState,
    profile: Option<String>,
) -> Result<SessionInfo, String> {
    let profile_id = profile.as_deref();
    // Verrou court : `client_id` seul, lecture trousseau (D-Bus) hors mutex.
    let client_id = {
        let s = state.lock().await;
        client_id_locked(&s)?
    };
    let token = launcher_auth::load_refresh_token(profile_id)
        .map_err(|e| format!("Stockage sécurisé : {e}"))?
        .ok_or_else(|| "Aucune session enregistrée.".to_string())?;
    if token.trim().is_empty() {
        return Err("Aucune session enregistrée.".to_string());
    }
    let refresh =
        launcher_auth::refresh_session(&AuthConfig::with_client_id(client_id), &token).await;
    let session = match refresh {
        Ok(s) => s,
        Err(e) => {
            // Dead refresh token (revoked/expired/rotated elsewhere):
            // drop the corpse so we never retry it, and route to re-login.
            // Anything else (network, parsing) keeps the token for retry.
            if launcher_auth::is_invalid_grant(&e) {
                let _ = launcher_auth::clear_refresh_token(profile_id);
                // Jeton mort : mise en quarantaine — la session cadavre est
                // abandonnée mais l'entrée du registre est conservée (visible
                // en Comptes, reconnexion possible au lieu de disparaître).
                // Chemin hérité (profil `None`, sans UUID) inchangé : rien à
                // quaranténer sans correspondance de compte.
                if let Some(uuid) = profile {
                    let game_dir;
                    {
                        let mut s = state.lock().await;
                        s.quarantine_dead_session(&uuid);
                        game_dir = s.game_dir();
                    }
                    persist_roster(&game_dir, state).await;
                }
                return Err(
                    crate::state::login_required_error() + " (session expirée, reconnectez-vous)."
                );
            }
            return Err(e.to_string());
        }
    };
    let info = SessionInfo::from(&session);
    // Écriture trousseau (D-Bus) hors mutex.
    let _ = launcher_auth::save_refresh_token(Some(session.uuid.as_str()), &session.refresh_token);
    let game_dir;
    {
        let mut s = state.lock().await;
        s.upsert_session(session);
        sync_roster_locked(&mut s);
        game_dir = s.game_dir();
    }
    persist_roster(&game_dir, state).await;
    Ok(info)
}

/// One-time migration: a token stored under the legacy default profile
/// (pre-multi-account builds) is re-homed onto its UUID profile after a
/// successful legacy restore, and the default entry is cleared. Skipped
/// when the roster already tracks the active account.
async fn migrate_legacy_default_token(state: &SharedState) {
    let uuid = {
        let s = state.lock().await;
        match s.active_session() {
            Some(a) => a.uuid.clone(),
            None => return,
        }
    };
    let already = {
        let s = state.lock().await;
        s.settings.accounts.iter().any(|a| a.uuid == uuid)
    };
    if already {
        return;
    }
    if let Ok(Some(t)) = launcher_auth::load_refresh_token(None) {
        if !t.trim().is_empty() {
            let _ = launcher_auth::save_refresh_token(Some(uuid.as_str()), &t);
            let _ = launcher_auth::clear_refresh_token(None);
        }
    }
}

// ---------------------------------------------------------------------------
// Login (device-code flow)
// ---------------------------------------------------------------------------

/// Step 1: request a device code (stores it for [`poll_login`]).
pub async fn request_login(state: &SharedState) -> Result<DeviceCodeInfo, String> {
    let client_id = {
        let s = state.lock().await;
        client_id_locked(&s)?
    };
    let device = request_device_code(&http_client()?, &AuthConfig::with_client_id(client_id))
        .await
        .map_err(|e| e.to_string())?;
    let info = DeviceCodeInfo::from(&device);
    state.lock().await.pending_login = Some(device);
    Ok(info)
}

/// Step 2: poll until the user approves, then run the full
/// MSA → XBL → XSTS → Minecraft chain and store the session.
/// Long-running (resolves when the user validates or the code expires).
///
/// Cancellable: `cancel_login` clears `pending_login`, and the Microsoft
/// poll below races a watcher on that same slot — the watcher wins in
/// ~500 ms, so a late phone approval after Annuler never creates a session.
pub async fn poll_login(state: &SharedState) -> Result<SessionInfo, String> {
    let (client_id, device) = {
        let s = state.lock().await;
        let client_id = client_id_locked(&s)?;
        let device: DeviceCodeResponse = s.pending_login.clone().ok_or_else(|| {
            "Aucune demande de connexion en cours. Relancez « Se connecter ».".to_string()
        })?;
        (client_id, device)
    };
    let config = AuthConfig::with_client_id(client_id);
    let client = http_client()?;
    let device_code = device.device_code.clone();
    let msa = {
        let watcher = wait_for_login_cancel(state.clone(), device_code.clone());
        tokio::select! {
            res = poll_device_token(&client, &config, &device) => res.map_err(|e| e.to_string())?,
            () = watcher => return Err("Connexion annulée.".to_string()),
        }
    };
    // Garde tardive : approbation arrivée entre la fin du sondage et ici
    // alors que l'utilisateur venait d'annuler (ou de relancer une demande).
    {
        let s = state.lock().await;
        ensure_login_not_cancelled(s.pending_login.as_ref(), &device_code)?;
    }
    let xbl = authenticate_xbl(&client, &msa.access_token)
        .await
        .map_err(|e| e.to_string())?;
    let xsts = authorize_xsts(&client, &xbl.token)
        .await
        .map_err(|e| e.to_string())?;
    let mc = login_with_xbox(&client, &xbl.uhs, &xsts.token)
        .await
        .map_err(|e| e.to_string())?;
    let profile = fetch_profile(&client, &mc.access_token)
        .await
        .map_err(|e| e.to_string())?;
    let session = MinecraftSession {
        uuid: profile.id,
        name: profile.name,
        mc_token: mc.access_token,
        refresh_token: msa.refresh_token,
        obtained: std::time::Instant::now(),
    };
    let info = SessionInfo::from(&session);
    // Écriture trousseau (D-Bus) hors mutex.
    let uuid = session.uuid.clone();
    let _ = launcher_auth::save_refresh_token(Some(uuid.as_str()), &session.refresh_token);
    let game_dir;
    {
        let mut s = state.lock().await;
        // Per-UUID keyring profile (multi-account) + roster sync; a fresh
        // login always becomes the active account.
        s.upsert_session(session);
        s.set_active_account(&uuid);
        sync_roster_locked(&mut s);
        game_dir = s.game_dir();
    }
    persist_roster(&game_dir, state).await;
    {
        let mut s = state.lock().await;
        s.pending_login = None;
    }
    Ok(info)
}

/// Annule la connexion Microsoft en cours, s'il y en a une : `pending_login`
/// est effacé, et le [`poll_login`] en vol s'interrompt en ~500 ms sans
/// jamais créer de session (garde d'annulation). Best-effort : réussit
/// toujours, même sans connexion en cours.
pub async fn cancel_login(state: &SharedState) -> Result<(), String> {
    state.lock().await.pending_login = None;
    Ok(())
}

/// `true` quand la demande `device_code` n'est plus la demande en cours
/// (annulée par [`cancel_login`] ou remplacée par un nouveau
/// [`request_login`]). Pur (testé sans réseau).
fn login_request_superseded(pending: Option<&DeviceCodeResponse>, device_code: &str) -> bool {
    match pending {
        Some(p) => p.device_code != device_code,
        None => true,
    }
}

/// Garde d'annulation : erreur quand la demande `device_code` n'est plus
/// d'actualité. Examinée sans réseau (l'annulation reste testée
/// hermétiquement, le sondage Microsoft ne l'étant pas).
fn ensure_login_not_cancelled(
    pending: Option<&DeviceCodeResponse>,
    device_code: &str,
) -> Result<(), String> {
    if login_request_superseded(pending, device_code) {
        return Err("Connexion annulée.".to_string());
    }
    Ok(())
}

/// Résout dès que la demande `device_code` est annulée ou remplacée
/// (scrutation douce de `pending_login`, sans réseau). Ne résout jamais
/// sinon : le `select!` de [`poll_login`] reste branché sur le sondage
/// Microsoft jusqu'à approbation ou expiration.
async fn wait_for_login_cancel(state: SharedState, device_code: String) {
    loop {
        {
            let s = state.lock().await;
            if login_request_superseded(s.pending_login.as_ref(), &device_code) {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Cached manifest status (no network).
pub async fn get_manifest_status(state: &SharedState) -> ManifestStatusDto {
    state.lock().await.manifest_status()
}

/// Fetch the CDN manifest, update freshness, return the new status.
pub async fn refresh_manifest(state: &SharedState) -> Result<ManifestStatusDto, String> {
    let game_dir = state.lock().await.game_dir();
    let fresh = fetch_manifest(&http_client()?, MANIFEST_URL)
        .await
        .map_err(|e| format!("Impossible de joindre le CDN : {e}"))?;
    let mut s = state.lock().await;
    s.update_status = freshness_against_cache(&game_dir, &fresh);
    s.manifest = Some(fresh);
    Ok(s.manifest_status())
}

/// Fetch-and-store the manifest when no usable one is in memory.
async fn ensure_manifest(state: &SharedState) -> Result<Manifest, String> {
    if let Some(m) = state.lock().await.manifest.clone() {
        return Ok(m);
    }
    let game_dir = state.lock().await.game_dir();
    let fresh = fetch_manifest(&http_client()?, MANIFEST_URL)
        .await
        .map_err(|e| format!("Impossible de joindre le CDN : {e}"))?;
    let mut s = state.lock().await;
    s.update_status = freshness_against_cache(&game_dir, &fresh);
    s.manifest = Some(fresh.clone());
    Ok(fresh)
}

// ---------------------------------------------------------------------------
// Install / verify
// ---------------------------------------------------------------------------

fn summary_dto(s: &DownloadSummary) -> InstallSummaryDto {
    InstallSummaryDto {
        total_files: s.total_files,
        downloaded_files: s.downloaded_files,
        skipped_files: s.skipped_files,
        total_bytes: s.total_bytes,
    }
}

/// CDN plan for the active mode. In experimental mode the upstream LWJGL3
/// profile owns the libraries (le seau CDN `libraries-*`, cross-platform,
/// shadow e.g. gson-2.10.1 with gson-2.8.6) and replaces Paladium's Forge
/// with the official 1614 universal the profile was tested against — so
/// both are excluded and only mods/configs/assets flow from the CDN, plus
/// a pinned allowlist of CDN libraries the upstream profile omits but pack
/// mods require at runtime (slf4j, MongoDB, molang, mclib, guava-17.0 —
/// the CDN manifest ships them with SHAs).
pub fn cdn_plan_for_mode(
    manifest: &launcher_core::Manifest,
    game_dir: &std::path::Path,
    experimental: bool,
) -> Vec<launcher_core::PlannedFile> {
    let planned = launcher_core::plan_downloads_for_current_os(manifest, game_dir);
    if !experimental {
        return planned;
    }
    planned
        .into_iter()
        .filter(|p| {
            (!launcher_core::is_libraries_model(&p.file.model)
                || launcher_core::modern_cdn_lib_wanted(&p.file.name))
                && !p.file.name.starts_with("palaforge")
                && !p.file.name.starts_with("forge-")
        })
        .collect()
}

/// Garde bytecode au moment du plan : les jars du classpath résolu et
/// présents sur disque qui dépassent le plafond du runtime (v52 classique,
/// v65 expérimental) sont signalés en français, jars nommés — AVERTISSEMENT
/// seulement, jamais d'échec. Les absents sont ignorés (pas encore
/// téléchargés). FML lui-même ignore les jars illisibles (`it will be
/// ignored` — le boot continue), donc bloquer l'install serait strictement
/// pire. Même scanner que l'historique
/// ([`launcher_core::max_class_version_in_jar`]) — seul le verdict a changé.
pub fn assert_classpath_bytecode(
    manifest: &launcher_core::Manifest,
    game_dir: &std::path::Path,
    experimental: bool,
) -> Vec<String> {
    use launcher_core::{class_version_cap_for, Os};
    let cap = class_version_cap_for(experimental);
    let current = Os::current();
    if experimental {
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        if let Some(profile) = launcher_core::load_cached_modern_profile(game_dir) {
            for entry in launcher_core::modern_classpath_entries(
                &profile,
                game_dir,
                &manifest.mc_version,
                current,
                std::env::consts::ARCH,
            ) {
                paths.push(std::path::PathBuf::from(entry));
            }
        }
        for entry in launcher_core::modern_cdn_classpath_entries(manifest, game_dir, current) {
            paths.push(std::path::PathBuf::from(entry));
        }
        launcher_core::check_jar_class_versions(&paths, cap)
    } else {
        let cp = launcher_core::build_classpath(manifest, game_dir, current);
        let sep = launcher_core::classpath_separator(current);
        let paths: Vec<std::path::PathBuf> = cp
            .split(sep)
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
        launcher_core::check_jar_class_versions(&paths, cap)
    }
}

/// Garde fraîcheur bibliothèques : `true` quand des fichiers du plan CDN
/// (mode actif pris en compte) manquent sur disque — attrape les
/// `libraries/` vidés dont la version `cdn_last.json` est pourtant « à
/// jour » (wipe 16:41). Ne couvre que `libraries-*`/`versions` (jamais les
/// mods, où un `.disabled` volontaire n'est pas du stale). Le JOUER suivant
/// re-télécharge automatiquement, zéro étape manuelle.
pub fn cdn_plan_libs_missing(
    manifest: &launcher_core::Manifest,
    game_dir: &std::path::Path,
    experimental: bool,
) -> bool {
    use launcher_core::Os;
    let current = Os::current();
    if experimental {
        return cdn_plan_for_mode(manifest, game_dir, true)
            .iter()
            .filter(|p| {
                launcher_core::is_libraries_model(&p.file.model)
                    || manifest
                        .model_by_name(&p.file.model)
                        .is_some_and(|m| m.dest == "versions" || m.name == "versions")
            })
            .any(|p| !p.dest.is_file());
    }
    launcher_core::cdn_libraries_stale(manifest, game_dir, current)
}

/// Jumeau `.disabled` d'une destination planifiée (`<dest>.disabled`,
/// sémantique [`DISABLED_SUFFIX`]) : compte comme PRÉSENT, jamais
/// re-téléchargé (mod volontairement désactivé).
fn disabled_twin(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", dest.to_string_lossy(), DISABLED_SUFFIX))
}

/// Fichiers de contenu manquants du plan CDN (mode actif pris en compte) :
/// les modèles NON-bibliothèques (mods, configs, resourcepacks, … — les
/// `libraries-*`/`versions` restent couverts par [`cdn_plan_libs_missing`]
/// pour éviter le double travail). Règles : présence seule (`is_file`,
/// SANS hachage — rapide sur ~500 entrées) ; un jumeau `.disabled` compte
/// comme présent ; `loki.jar` est exclu (JOUER ne porte aucun opt-in —
/// jamais de téléchargement forcé d'un optionnel décliné, voir
/// `manifest_lists_loki`/`start_install`) ; le plan mode-actif borne déjà
/// la saveur (la réserve `modstack-experimental/` ne satisfait RIEN — les
/// destinations testées sont sous `mods/` et ailleurs, jamais la réserve).
pub fn missing_content_files(
    manifest: &launcher_core::Manifest,
    game_dir: &std::path::Path,
    experimental: bool,
) -> Vec<launcher_core::PlannedFile> {
    cdn_plan_for_mode(manifest, game_dir, experimental)
        .into_iter()
        .filter(|p| {
            // Partition exacte avec `cdn_plan_libs_missing` : bibliothèques
            // + `versions` ailleurs, tout le reste ici.
            let libs_or_versions = launcher_core::is_libraries_model(&p.file.model)
                || manifest
                    .model_by_name(&p.file.model)
                    .is_some_and(|m| m.dest == "versions" || m.name == "versions");
            !libs_or_versions && !p.file.name.eq_ignore_ascii_case(LOKI_FILE_NAME)
        })
        .filter(|p| !p.dest.is_file() && !disabled_twin(&p.dest).is_file())
        .collect()
}

/// Garde fraîcheur contenu : `true` quand au moins un fichier du plan CDN
/// non-bibliothèque manque sur disque (JOUER avec une version « à jour »
/// mais des mods effacés allait droit au spawn sans jamais se réparer —
/// ex. `PalaForgeUtils-6.6.2.jar`, listé au manifeste et requis par 8 mods,
/// jamais re-téléchargé). Présence seule, sans réseau (voir
/// [`missing_content_files`]).
pub fn cdn_content_stale(
    manifest: &launcher_core::Manifest,
    game_dir: &std::path::Path,
    experimental: bool,
) -> bool {
    !missing_content_files(manifest, game_dir, experimental).is_empty()
}

// ---------------------------------------------------------------------------
// Progression unifiée : FICHIERS et ACTIONS, un seul modèle honnête.
//
// `fraction` = octets réglés du run / octets totaux du run, partout — aucune
// bande de phase. Le total est SOMMÉ AVANT tout téléchargement : les
// métadonnées légères (manifeste CDN déjà en main, version + index Mojang,
// profil LWJGL3, pré-vol HEAD de l'archive Java) sont récupérées d'abord,
// chaque phase contribue alors ses tailles connues au moment du plan :
//   - CDN : somme des `size` du plan (`cdn_plan_for_mode`) ;
//   - Mojang : client vanilla + universel Forge 1614 + bibliothèques +
//     assets manquants (planifiés après les métadonnées) ;
//   - Java : archive Temurin 8 / GraalVM 21 (pré-vol HEAD `Content-Length` ;
//     repli : le total grandit à la première callback) ;
//   - expérimental : bibliothèques amont à tailles connues (+ coremods et
//     opt-mods : fichiers comptés, octets inconnus — `done` et total
//     grandissent ensemble à la fin de chaque fichier, sans rechute) ;
//   - Loki optionnel : idem (1 fichier, taille inconnue).
// Les vérifications (fichiers déjà frais) créditent leurs octets fichier par
// fichier avec une ligne `Vérification i/n — nom`, ÉMISE sur le canal dès
// que le contrôle d'un fichier se termine (jamais comptée en silence : la
// boucle de vérification diffuse en continu, un événement par fichier).
// Les actions sans octets (extraction des natifs, prune de saveur,
// durcissement, finalisation) émettent des lignes de statut
// (`current_file == ""`, compteurs inchangés).
// `current_file` porte toujours le VRAI nom du fichier écrit, ou `""` —
// jamais un mot de phase.
//
// Total en DEUX TEMPS (zéro silence) : le plan CDN, purement local
// (somme des `size` du manifeste — instantané), est enregistré ET émis en
// premier, et sa vérification démarre aussitôt contre ce total provisoire.
// Les pré-vols lents et indépendants (métadonnées Mojang, HEAD de l'archive
// Java, profil LWJGL3) tournent EN PARALLÈLE de cette vérification et
// AGRANDISSENT le total à leur arrivée (`add_work`) — la fraction peut donc
// reculer légèrement quand un gros total tardif atterrit, c'est documenté
// et voulu : jamais de blocage des premières émissions sur des totaux tardifs.

/// Journal horodaté d'une frontière de phase d'installation (une poignée de
/// lignes par run, jamais par fichier) : tout « coincé à 0 % » se localise
/// dans le journal du lanceur.
fn phase_log(detail: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    crate::log_verbose!("[launcher] install phase {detail} t={ms}ms");
}

/// Ligne de durée permanente d'une commande lourde du démarrage (une seule
/// ligne par appel, jamais par fichier/ligne — coût : un `Instant` + un
/// `eprintln!`). Inconditionnelle : une poignée de lignes par démarrage,
/// pas du bruit.
fn boot_timing_log(what: &str, started: std::time::Instant) {
    eprintln!(
        "[launcher] démarrage {what} en {} ms t={}ms",
        started.elapsed().as_millis(),
        crate::state::boot_wall_ms()
    );
}

/// Compteurs partagés d'UN run d'installation (toutes phases, un seul total).
pub struct InstallRun {
    emit: Option<Arc<ProgressEmitter>>,
    completed_files: AtomicUsize,
    total_files: AtomicUsize,
    completed_bytes: AtomicU64,
    total_bytes: AtomicU64,
}

impl InstallRun {
    /// Nouveau run (aucun travail enregistré — `add_work` au moment du plan).
    pub fn new(emit: Option<Arc<ProgressEmitter>>) -> Self {
        Self {
            emit,
            completed_files: AtomicUsize::new(0),
            total_files: AtomicUsize::new(0),
            completed_bytes: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
        }
    }

    /// Enregistre du travail planifié (AVANT tout téléchargement).
    pub fn add_work(&self, files: usize, bytes: u64) {
        self.total_files.fetch_add(files, Ordering::Relaxed);
        self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Ajuste le total d'octets (écart pré-vol/réel, repli taille inconnue).
    /// Fraction inchangée pour les tailles inconnues créditées ensemble.
    pub fn correct_bytes(&self, delta: i64) {
        if delta >= 0 {
            self.total_bytes.fetch_add(delta as u64, Ordering::Relaxed);
        } else {
            self.total_bytes.fetch_sub(
                (delta.unsigned_abs()).min(self.total_bytes.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
        }
    }

    /// Base d'une phase (octets et fichiers réglés avant elle) : les phases
    /// se suivent, donc `base + compteurs locaux` = compteurs du run.
    pub fn base(&self) -> (u64, usize) {
        (
            self.completed_bytes.load(Ordering::Relaxed),
            self.completed_files.load(Ordering::Relaxed),
        )
    }

    /// Fraction unifiée du run (octets réglés / total, repli fichiers).
    pub fn fraction(&self) -> f32 {
        let done = self.completed_bytes.load(Ordering::Relaxed);
        let total = self.total_bytes.load(Ordering::Relaxed);
        if total > 0 {
            (done.min(total) as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            let files = self.completed_files.load(Ordering::Relaxed);
            let all = self.total_files.load(Ordering::Relaxed);
            if all > 0 {
                (files as f32 / all as f32).clamp(0.0, 1.0)
            } else {
                0.0
            }
        }
    }

    fn send(&self, current_file: String, detail: String, done_files: usize, done_bytes: u64) {
        if let Some(emit) = &self.emit {
            let total_files = self.total_files.load(Ordering::Relaxed);
            let total_bytes = self.total_bytes.load(Ordering::Relaxed);
            let fraction = if total_bytes > 0 {
                (done_bytes.min(total_bytes) as f32 / total_bytes as f32).clamp(0.0, 1.0)
            } else if total_files > 0 {
                (done_files as f32 / total_files as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            emit(ProgressPayload {
                completed_files: done_files,
                total_files,
                completed_bytes: done_bytes,
                total_bytes,
                current_file,
                fraction,
                detail,
            });
        }
    }

    /// Ligne fichier à compteurs explicites (pont Java, tailles inconnues).
    pub fn emit_file_at(&self, done_bytes: u64, done_files: usize, file: &str) {
        let total_files = self.total_files.load(Ordering::Relaxed);
        let total_bytes = self.total_bytes.load(Ordering::Relaxed);
        let fraction = if total_bytes > 0 {
            (done_bytes.min(total_bytes) as f32 / total_bytes as f32).clamp(0.0, 1.0)
        } else if total_files > 0 {
            (done_files as f32 / total_files as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.send(
            file.to_string(),
            crate::state::format_progress_detail(
                done_files,
                total_files,
                file,
                done_bytes,
                total_bytes,
                fraction,
            ),
            done_files,
            done_bytes,
        );
    }

    /// Traduit une mise à jour locale d'un lot (`download_files`) en ligne
    /// unifiée : `base + compteurs du lot`, détail fichier ou vérification.
    pub fn on_phase_update(&self, base: (u64, usize), u: &launcher_core::ProgressUpdate) {
        let done = base.0.saturating_add(u.completed_bytes);
        let files = base.1.saturating_add(u.completed_files);
        let total_files = self.total_files.load(Ordering::Relaxed);
        let total_bytes = self.total_bytes.load(Ordering::Relaxed);
        let fraction = if total_bytes > 0 {
            (done.min(total_bytes) as f32 / total_bytes as f32).clamp(0.0, 1.0)
        } else if total_files > 0 {
            (files as f32 / total_files as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let detail = if u.skipped {
            crate::state::format_verify_detail(files, total_files, &u.current_file)
        } else {
            crate::state::format_progress_detail(
                files,
                total_files,
                &u.current_file,
                done,
                total_bytes,
                fraction,
            )
        };
        self.send(u.current_file.clone(), detail, files, done);
    }

    /// Fin d'un lot : crédite ses totaux planifiés (téléchargés + vérifiés).
    pub fn end_phase(&self, bytes: u64, files: usize) {
        self.completed_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.completed_files.fetch_add(files, Ordering::Relaxed);
    }

    /// Ligne de statut d'une action sans fichier (`current_file == ""`,
    /// compteurs inchangés — extraction, prune, durcissement…).
    pub fn say(&self, detail: &str) {
        self.send(
            String::new(),
            detail.to_string(),
            self.completed_files.load(Ordering::Relaxed),
            self.completed_bytes.load(Ordering::Relaxed),
        );
    }

    /// Fichier à taille inconnue terminé (coremods, opt-mods, Loki) : `done`
    /// et total grandissent ensemble (la barre ne retombe jamais), ligne fichier.
    pub fn credit_unknown(&self, file: &str, len: u64) {
        self.completed_bytes.fetch_add(len, Ordering::Relaxed);
        self.total_bytes.fetch_add(len, Ordering::Relaxed);
        self.completed_files.fetch_add(1, Ordering::Relaxed);
        let done_files = self.completed_files.load(Ordering::Relaxed);
        let done_bytes = self.completed_bytes.load(Ordering::Relaxed);
        self.emit_file_at(done_bytes, done_files, file);
    }

    /// Fichier à taille inconnue déjà frais (présence seule) : compteur de
    /// fichiers + ligne de vérification, sans octets.
    pub fn credit_skip(&self, file: &str) {
        self.completed_files.fetch_add(1, Ordering::Relaxed);
        let done_files = self.completed_files.load(Ordering::Relaxed);
        let total_files = self.total_files.load(Ordering::Relaxed);
        self.send(
            file.to_string(),
            crate::state::format_verify_detail(done_files, total_files, file),
            done_files,
            self.completed_bytes.load(Ordering::Relaxed),
        );
    }

    /// Clôture du pipeline : 1.0 exact + `"Jeu installé et vérifié."`
    /// (même ligne que le toast), sans nom de fichier.
    pub fn finish(&self) {
        if let Some(emit) = &self.emit {
            emit(ProgressPayload {
                completed_files: self.total_files.load(Ordering::Relaxed),
                total_files: self.total_files.load(Ordering::Relaxed),
                completed_bytes: self.total_bytes.load(Ordering::Relaxed),
                total_bytes: self.total_bytes.load(Ordering::Relaxed),
                current_file: String::new(),
                fraction: 1.0,
                detail: "Jeu installé et vérifié.".to_string(),
            });
        }
    }
}

/// Somme des `size` d'un plan (contribution au total du run).
fn plan_bytes(planned: &[launcher_core::PlannedFile]) -> u64 {
    planned.iter().map(|p| p.file.size).sum()
}

/// Run the downloader over an explicit plan, forwarding unified progress.
/// `cancel` aborts the run (frontend `Annuler` button). The batch totals
/// must be registered (`add_work`) BEFORE this runs, so the run total stays
/// fixed while the batch streams.
async fn run_planned(
    planned: Vec<launcher_core::PlannedFile>,
    run: &std::sync::Arc<InstallRun>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<DownloadSummary, String> {
    let batch_bytes = plan_bytes(&planned);
    let batch_files = planned.len();
    phase_log(&format!(
        "groupe-cdn debut fichiers={batch_files} octets={batch_bytes}"
    ));
    // La vérification (présents re-hachés) diffuse un événement par fichier
    // via le canal — jamais comptée en silence (voir `download_files`).
    phase_log(&format!("verification debut fichiers={batch_files}"));
    let base = run.base();
    let (tx, mut rx) = mpsc::channel(128);
    let forward_run = std::sync::Arc::clone(run);
    let forward = tokio::spawn(async move {
        while let Some(u) = rx.recv().await {
            forward_run.on_phase_update(base, &u);
        }
    });
    let out = download_files(
        &http_client()?,
        &planned,
        DEFAULT_MAX_CONCURRENCY,
        DEFAULT_MAX_RETRIES,
        Some(tx),
        cancel,
    )
    .await;
    let _ = forward.await;
    // A user cancel keeps its own clean sentence (no "Échec" prefix).
    let summary = out.map_err(|e| match &e {
        launcher_core::CoreError::Cancelled(_) => e.to_string(),
        _ => format!("Échec de l'installation : {e}"),
    })?;
    run.end_phase(batch_bytes, batch_files);
    phase_log(&format!(
        "verification fin fichiers={batch_files} telecharges={} verifies={}",
        summary.downloaded_files, summary.skipped_files
    ));
    phase_log(&format!(
        "groupe-cdn fin fichiers={batch_files} octets={batch_bytes}"
    ));
    Ok(summary)
}

/// Run the downloader for `manifest` into `game_dir` over `run`
/// (standalone batch: the plan totals are registered here — the pipeline
/// entry points prefetch wider totals instead, see `start_install_pipeline`).
/// `cancel` aborts the run (frontend `Annuler` button).
#[cfg(test)]
async fn run_install(
    manifest: &Manifest,
    game_dir: &Path,
    run: &std::sync::Arc<InstallRun>,
    cancel: Option<Arc<AtomicBool>>,
    experimental: bool,
) -> Result<DownloadSummary, String> {
    let planned = cdn_plan_for_mode(manifest, game_dir, experimental);
    run.add_work(planned.len(), plan_bytes(&planned));
    run_planned(planned, run, cancel).await
}

/// Request cancellation of the running install, if any. The downloader
/// checks the flag between files and aborts in-flight attempts; the run
/// ends with `"…annulée."`. Always succeeds (no-op when idle).
pub async fn cancel_install(state: &SharedState) -> Result<(), String> {
    state
        .lock()
        .await
        .install_cancel
        .store(true, Ordering::Relaxed);
    Ok(())
}

/// Request cancellation of the running Java 8 download, if any.
pub async fn cancel_java_download(state: &SharedState) -> Result<(), String> {
    state
        .lock()
        .await
        .java_cancel
        .store(true, Ordering::Relaxed);
    Ok(())
}

// ---------------------------------------------------------------------------
// Mojang fallback runtime (vanilla jar, per-OS natives, assets, hardening)
// ---------------------------------------------------------------------------

/// What the Mojang step provisioned (Console log lines).
#[derive(Debug, Clone, Default)]
pub struct MojangReport {
    /// Vanilla client jar (re)downloaded.
    pub vanilla_downloaded: bool,
    /// Official Forge 1614 universal (re)downloaded (Forge Maven).
    pub forge_downloaded: bool,
    /// Mojang library jars (re)downloaded.
    pub libs_downloaded: usize,
    /// Native files extracted into `natives/`.
    pub natives_extracted: usize,
    /// Asset objects (re)downloaded.
    pub assets_downloaded: usize,
    /// log4j-core jars rewritten (JndiLookup stripped).
    pub log4j_patched: usize,
}

/// Run the shared downloader for `files` with unified progress forwarding
/// onto `run` (same `install-progress` channel as the main install). Small
/// helper for the Mojang-step single-file downloads (vanilla jar, Forge
/// universal) so primary/fallback attempts share the plumbing. Batch totals
/// must be registered (`add_work`) BEFORE this runs.
async fn download_many(
    client: &reqwest::Client,
    files: &[launcher_core::PlannedFile],
    run: &std::sync::Arc<InstallRun>,
    cancel: Option<Arc<AtomicBool>>,
    context: &str,
) -> Result<launcher_core::DownloadSummary, String> {
    use launcher_core::{download_files, DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_RETRIES};
    let batch_bytes: u64 = files.iter().map(|p| p.file.size).sum();
    let batch_files = files.len();
    phase_log(&format!(
        "groupe debut {context} fichiers={batch_files} octets={batch_bytes}"
    ));
    let base = run.base();
    let (tx, mut rx) = mpsc::channel(128);
    let forward_run = std::sync::Arc::clone(run);
    let forward = tokio::spawn(async move {
        while let Some(u) = rx.recv().await {
            forward_run.on_phase_update(base, &u);
        }
    });
    let out = download_files(
        client,
        files,
        DEFAULT_MAX_CONCURRENCY,
        DEFAULT_MAX_RETRIES,
        Some(tx),
        cancel,
    )
    .await;
    let _ = forward.await;
    let summary = out.map_err(|e| match &e {
        launcher_core::CoreError::Cancelled(_) => e.to_string(),
        _ => format!("{context} : {e}"),
    })?;
    run.end_phase(batch_bytes, batch_files);
    phase_log(&format!(
        "groupe fin {context} fichiers={batch_files} octets={batch_bytes}"
    ));
    Ok(summary)
}

/// Plan Mojang (aucun octet) : manifeste de version + index des assets
/// récupérés d'abord, puis vanilla + Forge + bibliothèques + assets manquants
/// planifiés et ENREGISTRÉS dans le total du run (`add_work`) avant tout
/// téléchargement — la barre ne peut donc pas retomber quand la phase
/// démarre (voir le commentaire du modèle unifié au-dessus d'`InstallRun`).
struct MojangPlan {
    /// Client vanilla à télécharger (`None` = frais).
    vanilla: Option<launcher_core::PlannedFile>,
    /// Tentatives Forge 1614 (vide = frais ; même taille/SHA1 épinglés).
    forge_attempts: Vec<launcher_core::PlannedFile>,
    /// Bibliothèques Mojang à télécharger (natifs inclus : téléchargés ici).
    regular: Vec<launcher_core::PlannedFile>,
    /// Jars natifs à extraire (chemins, déjà comptés dans `regular`).
    native_jars: Vec<PathBuf>,
    /// Objets d'assets manquants (pré-filtre taille, sans hachage).
    assets_missing: Vec<launcher_core::PlannedFile>,
}

/// Construit le [`MojangPlan`] (métadonnées réseau + planification locale) et
/// enregistre ses totaux dans `run`. Aucun octet ne circule ici.
async fn plan_mojang_phase(
    run: &InstallRun,
    client: &reqwest::Client,
    manifest: &launcher_core::Manifest,
    game_dir: &std::path::Path,
    experimental: bool,
) -> Result<MojangPlan, String> {
    use launcher_core::{
        artifact_base, load_or_fetch_asset_index, load_or_fetch_mojang_version, needs_update,
        plan_forge_universal, plan_mojang_assets, plan_mojang_libraries, plan_vanilla_jar, Os,
    };
    use std::collections::HashSet;

    phase_log("prevol-mojang debut (version + index)");
    let current = Os::current();
    // CDN coverage: skip Mojang files the manifest already provides (exact
    // name) or shadows (same artifact base, e.g. CDN guava-17.0 beats
    // Mojang guava-15.0 on the classpath).
    let cdn_planned = launcher_core::plan_downloads(manifest, game_dir, current);
    let mut skip_names: HashSet<String> = HashSet::with_capacity(cdn_planned.len());
    skip_names.extend(cdn_planned.iter().map(|p| p.file.name.clone()));
    let mut skip_bases: HashSet<String> = HashSet::with_capacity(cdn_planned.len());
    skip_bases.extend(cdn_planned.iter().map(|p| artifact_base(&p.file.name)));

    run.say("Mojang : manifeste 1.7.10…");
    let version = load_or_fetch_mojang_version(client, game_dir)
        .await
        .map_err(|e| format!("Runtime Mojang : {e}"))?;

    // Vanilla client jar (missing on every OS).
    let vanilla = plan_vanilla_jar(&version, game_dir);
    let vanilla = if needs_update(&vanilla.dest, vanilla.file.size, &vanilla.file.sha1) {
        run.add_work(1, vanilla.file.size);
        Some(vanilla)
    } else {
        None
    };

    // Official Forge 1614 universal (pinned size + SHA1 — primary and
    // fallback attempts carry the same bytes, counted once).
    let forge = plan_forge_universal(game_dir);
    let forge_attempts = if needs_update(&forge.dest, forge.file.size, &forge.file.sha1) {
        run.add_work(1, forge.file.size);
        launcher_core::forge_universal_attempts(game_dir)
    } else {
        Vec::new()
    };

    // Libraries: native jars everywhere, regular fill on Linux/macOS only
    // (Windows already works off the CDN set).
    let (regular, native_jars) = plan_mojang_libraries(
        &version,
        game_dir,
        current,
        &skip_names,
        &skip_bases,
        current != Os::Windows && !experimental,
    );
    run.add_work(regular.len(), plan_bytes(&regular));

    // Assets (686 objects, ~107 MiB first run, incremental after).
    run.say("Mojang : index des assets…");
    let index = load_or_fetch_asset_index(client, game_dir, &version)
        .await
        .map_err(|e| format!("Runtime Mojang : {e}"))?;
    let index_dest = launcher_core::assets_dir(game_dir)
        .join("indexes")
        .join(format!("{}.json", version.asset_index.id));
    if needs_update(
        &index_dest,
        version.asset_index.size,
        &version.asset_index.sha1,
    ) {
        if let Some(parent) = index_dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(
            &index_dest,
            serde_json::to_string(&index).unwrap_or_default(),
        )
        .map_err(|e| format!("Runtime Mojang : index assets : {e}"))?;
    }
    let assets = plan_mojang_assets(&index, game_dir);
    // Cheap size pre-filter so the shared downloader only hashes candidates.
    let assets_missing: Vec<_> = assets
        .into_iter()
        .filter(|p| {
            std::fs::metadata(&p.dest)
                .map(|m| m.len() != p.file.size)
                .unwrap_or(true)
        })
        .collect();
    run.add_work(assets_missing.len(), plan_bytes(&assets_missing));
    phase_log(&format!(
        "prevol-mojang fin vanilla={} forge={} libs={} assets={}",
        if vanilla.is_some() { 1 } else { 0 },
        if forge_attempts.is_empty() { 0 } else { 1 },
        regular.len(),
        assets_missing.len()
    ));

    Ok(MojangPlan {
        vanilla,
        forge_attempts,
        regular,
        native_jars,
        assets_missing,
    })
}

/// Provision the Mojang-side runtime the CDN does not ship (see
/// `launcher_core::mojang`): vanilla client jar, per-OS native jars +
/// extraction, missing regular libraries (Linux/macOS only), asset objects,
/// and Log4Shell hardening. `plan` comes from [`plan_mojang_phase`] (totals
/// already registered); `cancel` aborts between phases with `"…annulée."`.
async fn exec_mojang_phase(
    run: &std::sync::Arc<InstallRun>,
    client: &reqwest::Client,
    game_dir: &std::path::Path,
    plan: MojangPlan,
    cancel: Option<Arc<AtomicBool>>,
    experimental: bool,
) -> Result<MojangReport, String> {
    use launcher_core::{extract_all_natives, harden_log4j, is_cancelled};

    let cancelled = || is_cancelled(&cancel);
    let mut report = MojangReport::default();

    if let Some(vanilla) = plan.vanilla {
        if cancelled() {
            return Err("Installation annulée.".to_string());
        }
        download_many(
            client,
            std::slice::from_ref(&vanilla),
            run,
            cancel.clone(),
            "Runtime Mojang",
        )
        .await?;
        report.vanilla_downloaded = true;
    }

    // Official Forge 1614 universal (vanilla — replaces PalaForge on both
    // flavors; the CDN's 17 Forge libraries already cover everything else —
    // launchwrapper, asm-all 5.0.3, scala stack, akka, guava 17.0, … — so
    // this single jar is the whole dependency diff). Pinned URL + size +
    // SHA1, primary Forge Maven then fallback, non-empty-zip checked after.
    if !plan.forge_attempts.is_empty() {
        if cancelled() {
            return Err("Installation annulée.".to_string());
        }
        let mut done = false;
        let mut last_err = String::new();
        for attempt in &plan.forge_attempts {
            match download_many(
                client,
                std::slice::from_ref(attempt),
                run,
                cancel.clone(),
                "Forge 1614",
            )
            .await
            {
                Ok(_) => {
                    done = true;
                    break;
                }
                Err(e) => {
                    if e == "Installation annulée." {
                        return Err(e);
                    }
                    last_err = e;
                }
            }
        }
        // Tentatives repliées : seule la gagnante crédite (`download_many`
        // termine par `end_phase`, les perdantes ont retiré leurs octets).
        if !done {
            return Err(format!("Forge 1614 indisponible : {last_err}"));
        }
        if !launcher_core::forge_zip_ok(&launcher_core::forge_universal_dest(game_dir)) {
            return Err("Forge 1614 indisponible : universel invalide (zip vide).".to_string());
        }
        report.forge_downloaded = true;
    }

    if !plan.regular.is_empty() {
        if cancelled() {
            return Err("Installation annulée.".to_string());
        }
        let summary =
            download_many(client, &plan.regular, run, cancel.clone(), "Runtime Mojang").await?;
        report.libs_downloaded = summary.downloaded_files;
    }
    if cancelled() {
        return Err("Installation annulée.".to_string());
    }

    // Natives extraction (Mojang classifiers + any CDN *-natives-*.jar).
    // Skipped in experimental mode: LWJGL3 natives stay classpath jars.
    // Transformation locale de jars déjà crédités : ligne de statut, les
    // compteurs ne bougent pas (aucun octet neuf).
    if !experimental {
        run.say("Mojang : extraction des natifs…");
        report.natives_extracted = extract_all_natives(game_dir, &plan.native_jars)
            .map_err(|e| format!("Runtime Mojang : {e}"))?;
        if report.natives_extracted > 0 {
            run.say(&format!(
                "Mojang : {} natif(s) extrait(s).",
                report.natives_extracted
            ));
        }
    }
    if cancelled() {
        return Err("Installation annulée.".to_string());
    }

    if !plan.assets_missing.is_empty() {
        if cancelled() {
            return Err("Installation annulée.".to_string());
        }
        let summary = download_many(
            client,
            &plan.assets_missing,
            run,
            cancel.clone(),
            "Runtime Mojang",
        )
        .await?;
        report.assets_downloaded = summary.downloaded_files;
    }
    if cancelled() {
        return Err("Installation annulée.".to_string());
    }

    // Log4Shell hardening (idempotent, sidecar-recorded).
    run.say("Sécurité : durcissement Log4j…");
    let hardened = harden_log4j(game_dir).map_err(|e| format!("Runtime Mojang : {e}"))?;
    report.log4j_patched = hardened.patched;
    run.say(&format!(
        "Mojang : prêt (client {}, Forge 1614 {}, {} lib(s), {} natif(s), {} asset(s), Log4j : {} corrigé(s)).",
        if report.vanilla_downloaded {
            "téléchargé"
        } else {
            "à jour"
        },
        if report.forge_downloaded {
            "téléchargé"
        } else {
            "à jour"
        },
        report.libs_downloaded,
        report.natives_extracted,
        report.assets_downloaded,
        report.log4j_patched,
    ));
    Ok(report)
}

// ---------------------------------------------------------------------------
// Experimental runtime (LWJGL3 + Java 21, Phase 1, default OFF)
// ---------------------------------------------------------------------------

/// What the experimental step provisioned (Console log lines).
#[derive(Debug, Clone, Default)]
pub struct ModernReport {
    /// Upstream libraries (re)downloaded.
    pub libs_downloaded: usize,
    /// Coremods (re)downloaded (`lwjgl3ify`, UniMixins).
    pub coremods_downloaded: usize,
    /// Optimization-stack mods (re)downloaded (opt-in only).
    pub opt_mods_downloaded: usize,
    /// Java 21 (re)provisioned.
    pub java21_provisioned: bool,
}

/// Size-checked single-file download (GitHub releases publish no hashes):
/// skips non-empty files (presence credit + `Vérification` line), atomic
/// tmp+rename write otherwise. The file slot was registered (`add_work`, 0
/// byte) at plan time ; completion grows `done` and total together via
/// `credit_unknown` (no dip). Returns `true` when bytes were
/// actually fetched. `file` is the REAL `mods/<name>` display name.
async fn download_size_checked(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    file: &str,
    run: &InstallRun,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<bool, String> {
    let fresh = std::fs::metadata(dest)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if fresh {
        run.credit_skip(file);
        return Ok(false);
    }
    if launcher_core::is_cancelled(&cancel) {
        return Err("Installation annulée.".to_string());
    }
    // Ligne de départ (compteurs inchangés : les octets suivent à la fin).
    run.emit_file_at(
        run.completed_bytes.load(Ordering::Relaxed),
        run.completed_files.load(Ordering::Relaxed),
        file,
    );
    let bytes = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Runtime expérimental : {e}"))?
        .error_for_status()
        .map_err(|e| format!("Runtime expérimental : {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Runtime expérimental : {e}"))?;
    if bytes.is_empty() {
        return Err(format!("Runtime expérimental : {file} vide reçu."));
    }
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = dest.with_extension("jar.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("Runtime expérimental : écriture : {e}"))?;
    std::fs::rename(&tmp, dest).map_err(|e| format!("Runtime expérimental : écriture : {e}"))?;
    run.credit_unknown(file, bytes.len() as u64);
    Ok(true)
}

/// Provision GraalVM 21 into `<gameDir>/java21` (verified major 21),
/// shared by the experimental install and the standalone JDK-pane
/// download. `progress`/`cancel` shape matches `provision_temurin_from_url`.
async fn provision_graalvm21(
    game_dir: &Path,
    progress: Option<Arc<launcher_core::JavaProgressCallback>>,
    cancel: Option<Arc<AtomicBool>>,
    say_detail: &str,
) -> Result<(PathBuf, bool), String> {
    use launcher_core::{
        check_java, graalvm_url_for, is_cancelled, java_exe_name, provision_temurin_from_url, Os,
        MODERN_JAVA_DIR, MODERN_JAVA_MAJOR,
    };
    let cancelled = || is_cancelled(&cancel);
    let java21_bin = game_dir
        .join(MODERN_JAVA_DIR)
        .join("bin")
        .join(java_exe_name(Os::current()));
    if check_java(&java21_bin).major == Some(MODERN_JAVA_MAJOR) {
        return Ok((java21_bin, false));
    }
    if cancelled() {
        return Err("Installation annulée.".to_string());
    }
    let graalvm_url = graalvm_url_for(std::env::consts::OS, std::env::consts::ARCH);
    provision_temurin_from_url(
        game_dir,
        MODERN_JAVA_MAJOR,
        MODERN_JAVA_DIR,
        &graalvm_url,
        progress,
        cancel.clone(),
    )
    .await
    .map_err(|e| match &e {
        launcher_core::CoreError::Cancelled(_) => e.to_string(),
        _ => format!("{say_detail}: {e}"),
    })?;
    Ok((java21_bin, true))
}

/// Download GraalVM 21 (~230 Mo) into `<gameDir>/java21` (verified
/// Java 21). Streams `java-progress` events like the Temurin 8 flow;
/// `cancel_java_download` aborts the run.
pub async fn download_graalvm21(
    state: &SharedState,
    emit: Option<Arc<JavaProgressEmitter>>,
) -> Result<JavaInfoDto, String> {
    let (game_dir, cancel) = {
        let s = state.lock().await;
        s.java_cancel.store(false, Ordering::Relaxed);
        (s.game_dir(), s.java_cancel.clone())
    };
    // Sondage modale Java : magasin vidé au démarrage, chaque émission
    // suivante enregistrée (événements inchangés).
    let emit = prepare_java_emit(state, emit).await;
    let progress: Option<Arc<launcher_core::JavaProgressCallback>> = emit.map(|emit| {
        Arc::new(move |downloaded: u64, total: u64| {
            emit(JavaProgressPayload::from_bytes(downloaded, total));
        }) as Arc<launcher_core::JavaProgressCallback>
    });
    let (java_bin, _) =
        provision_graalvm21(&game_dir, progress, Some(cancel), "Runtime expérimental")
            .await
            .map_err(|e| match e.as_str() {
                s if s == "Installation annulée." => s.to_string(),
                _ => format!("Installation de GraalVM 21 : {e}"),
            })?;
    let check = check_java(&java_bin);
    Ok(JavaInfoDto {
        path: java_bin.to_string_lossy().into_owned(),
        major: check.major,
        is_java8: check.is_java8,
        warning: java_warning_for(&check),
    })
}

/// Plan Java (métadonnées seules) : fraîcheur locale puis pré-vol HEAD
/// (`Content-Length` + nom réel de l'archive) enregistré dans le total du run
/// AVANT tout octet. `needed == false` = runtime frais, rien à enregistrer.
/// Le pré-vol peut échouer (proxy sans HEAD…) : `size` vaut alors `None` et
/// le total grandit à la première callback (repli documenté, un seul saut).
struct JavaPlan {
    /// `false` = runtime frais, rien à télécharger ni à enregistrer.
    needed: bool,
    /// `true` = GraalVM 21 (`java21/`), `false` = Temurin 8 (`java/`).
    graalvm: bool,
    /// VRAI nom de l'archive (en-tête `Content-Disposition`, sinon dernier
    /// segment de l'URL, sinon repli construit) — affiché tel quel.
    archive_name: String,
    /// Taille pré-volée (`None` = inconnue, enregistrée en direct).
    size: Option<u64>,
}

/// Pré-vol d'une archive Java : `(Content-Length, nom réel)`. Le nom vient de
/// `Content-Disposition` quand le serveur le donne (Adoptium), sinon du
/// dernier segment de l'URL (GraalVM GitHub), sinon d'un repli construit —
/// jamais un mot de phase.
async fn java_archive_meta(
    client: &reqwest::Client,
    url: &str,
    fallback_name: &str,
) -> (Option<u64>, String) {
    let Ok(res) = client.head(url).send().await else {
        return (None, fallback_name.to_string());
    };
    if !res.status().is_success() {
        return (None, fallback_name.to_string());
    }
    let len = res.content_length();
    let name = res
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|cd| {
            cd.rsplit("filename=")
                .next()
                .map(|s| s.trim().trim_matches(['"', '\'', ' ']).to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            url.split('?')
                .next()
                .and_then(|u| u.rsplit('/').next())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| fallback_name.to_string());
    (len.filter(|n| *n > 0), name)
}

async fn plan_flavor_java(
    run: &InstallRun,
    client: &reqwest::Client,
    game_dir: &Path,
    experimental: bool,
) -> Result<JavaPlan, String> {
    phase_log("prevol-java debut (HEAD archive)");
    let idle = JavaPlan {
        needed: false,
        graalvm: experimental,
        archive_name: String::new(),
        size: None,
    };
    if experimental {
        if launcher_core::resolve_modern_java(game_dir).is_some() {
            phase_log("prevol-java fin frais");
            return Ok(idle);
        }
        let url = launcher_core::graalvm_url_for(std::env::consts::OS, std::env::consts::ARCH);
        let fallback = url
            .rsplit('/')
            .next()
            .unwrap_or("graalvm-21.tar.gz")
            .to_string();
        let (size, archive_name) = java_archive_meta(client, &url, &fallback).await;
        run.add_work(1, size.unwrap_or(0));
        phase_log(&format!(
            "prevol-java fin archive={archive_name} octets={}",
            size.unwrap_or(0)
        ));
        Ok(JavaPlan {
            needed: true,
            graalvm: true,
            archive_name,
            size,
        })
    } else {
        if launcher_core::java8_status(game_dir).is_java8 {
            phase_log("prevol-java fin frais");
            return Ok(idle);
        }
        let url = launcher_core::temurin_url_current();
        let ext = launcher_core::temurin_archive_kind_for(std::env::consts::OS);
        let (size, archive_name) =
            java_archive_meta(client, &url, &format!("temurin-8.{ext}")).await;
        run.add_work(1, size.unwrap_or(0));
        phase_log(&format!(
            "prevol-java fin archive={archive_name} octets={}",
            size.unwrap_or(0)
        ));
        Ok(JavaPlan {
            needed: true,
            graalvm: false,
            archive_name,
            size,
        })
    }
}

/// Pont `(downloaded, total)` → ligne fichier unifiée pour l'archive Java :
/// `base + téléchargé`, vrai nom d'archive. Quand le pré-vol a échoué, le
/// premier total annoncé agrandit le total du run (une fois).
fn java_run_bridge(
    run: std::sync::Arc<InstallRun>,
    base: (u64, usize),
    archive_name: String,
    size_known: std::sync::Arc<AtomicBool>,
    last: std::sync::Arc<(AtomicU64, AtomicU64)>,
) -> std::sync::Arc<launcher_core::JavaProgressCallback> {
    std::sync::Arc::new(move |downloaded: u64, total: u64| {
        last.0.store(downloaded, Ordering::Relaxed);
        last.1.store(total, Ordering::Relaxed);
        if total > 0 && !size_known.swap(true, Ordering::Relaxed) {
            run.correct_bytes(total as i64);
        }
        run.emit_file_at(base.0.saturating_add(downloaded), base.1, &archive_name);
    }) as std::sync::Arc<launcher_core::JavaProgressCallback>
}

/// Provision the current flavor's runtime when missing/invalid, bridging
/// the download onto the install-progress channel. Experimental →
/// GraalVM 21 (`java21/`, via [`provision_graalvm21`]); default → Temurin 8
/// (`java/`, via [`provision_java8`]). Returns `true` when bytes were
/// actually fetched. A custom `java_path` override never triggers a
/// download (launch already prefers it verbatim). `plan` comes from
/// [`plan_flavor_java`] (total déjà enregistré).
async fn exec_flavor_java(
    game_dir: &Path,
    plan: &JavaPlan,
    run: &std::sync::Arc<InstallRun>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<bool, String> {
    use launcher_core::is_cancelled;
    if !plan.needed {
        return Ok(false);
    }
    if is_cancelled(&cancel) {
        return Err("Installation annulée.".to_string());
    }
    // Pont des compteurs d'octets sur install-progress (vrai nom d'archive).
    let base = run.base();
    let size_known = std::sync::Arc::new(AtomicBool::new(plan.size.is_some()));
    let last = std::sync::Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
    let bridge = java_run_bridge(
        std::sync::Arc::clone(run),
        base,
        plan.archive_name.clone(),
        std::sync::Arc::clone(&size_known),
        std::sync::Arc::clone(&last),
    );
    if plan.graalvm {
        provision_graalvm21(
            game_dir,
            Some(bridge),
            cancel.clone(),
            "Runtime expérimental",
        )
        .await?;
    } else {
        if is_cancelled(&cancel) {
            return Err("Installation annulée.".to_string());
        }
        launcher_core::provision_java8(game_dir, Some(bridge), cancel.clone())
            .await
            .map_err(|e| match &e {
                launcher_core::CoreError::Cancelled(_) => e.to_string(),
                _ => format!("Runtime Java 8 : {e}"),
            })?;
    }
    // Réconciliation pré-vol/réel + crédit du fichier (les callbacks
    // n'émettent que des lignes, les compteurs bougent ici, une fois).
    let (downloaded, total) = (
        last.0.load(Ordering::Relaxed),
        last.1.load(Ordering::Relaxed),
    );
    let actual = if total > 0 { total } else { downloaded };
    let registered = plan.size.unwrap_or(0);
    if actual > registered {
        run.correct_bytes((actual - registered) as i64);
    } else if registered > actual {
        run.correct_bytes(-((registered - actual) as i64));
    }
    run.end_phase(actual, 1);
    // The provisioned binary is verified Java 8/21 by construction.
    Ok(true)
}

/// Both JDK statuses for the JDK pane (never fails hard — a missing or
/// broken runtime degrades to "not installed" fields, never an error).
pub async fn get_java_runtimes(state: &SharedState) -> JavaRuntimesDto {
    // Deux sondes `java -version` : différé après le premier affichage côté
    // frontend (même audit que `check_java_status`).
    let started = std::time::Instant::now();
    let game_dir = state.lock().await.game_dir();
    let temurin_bin = launcher_core::resolve_java(&game_dir);
    let temurin = check_java(&temurin_bin);
    // `check_java` never fails hard (missing binary → empty probe), so this
    // is safe to call unconditionally — no existence pre-check needed.
    let graalvm_path = launcher_core::resolve_modern_java(&game_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let graalvm_probe_bin = if graalvm_path.is_empty() {
        std::path::PathBuf::from("__missing__")
    } else {
        std::path::PathBuf::from(&graalvm_path)
    };
    let graalvm = check_java(&graalvm_probe_bin);
    let out = JavaRuntimesDto {
        temurin8: JavaInfoDto {
            path: temurin_bin.to_string_lossy().into_owned(),
            major: temurin.major,
            is_java8: temurin.is_java8,
            warning: java_warning_for(&temurin),
        },
        graalvm21: JavaInfoDto {
            path: graalvm_path,
            major: graalvm.major,
            is_java8: false,
            warning: if graalvm.major == Some(launcher_core::MODERN_JAVA_MAJOR) {
                None
            } else {
                Some("GraalVM 21 non installé ou invalide — cliquez Télécharger.".to_string())
            },
        },
    };
    boot_timing_log("runtimes java", started);
    out
}

/// Plan expérimental (métadonnées seules) : profil LWJGL3 récupéré, puis
/// bibliothèques amont planifiées et ENREGISTRÉES (tailles connues) ;
/// coremods et opt-mods comptés en fichiers (0 octet — tailles inconnues,
/// créditées ensemble à la fin de chaque fichier). Aucun octet ici.
struct ModernPlan {
    /// Bibliothèques amont (ordre du profil — le jeu testé, en bloc).
    libs: Vec<launcher_core::PlannedFile>,
    /// Coremods `(url, dest)` (taille inconnue : GitHub sans hash).
    coremods: Vec<(String, PathBuf)>,
    /// Stack d'optimisation `(url, file)` (opt-in, taille inconnue).
    opt_mods: Vec<(String, String)>,
}

/// Construit le [`ModernPlan`] et enregistre ses totaux dans `run`.
async fn plan_modern_phase(
    run: &InstallRun,
    client: &reqwest::Client,
    game_dir: &std::path::Path,
    optimizations: bool,
) -> Result<ModernPlan, String> {
    use launcher_core::{
        load_or_fetch_modern_profile, plan_modern_coremods, plan_modern_libraries,
        plan_modern_opt_mods, Os,
    };
    use std::collections::HashSet;

    phase_log("prevol-moderne debut (profil LWJGL3)");
    run.say("Expérimental : profil LWJGL3…");
    let profile = load_or_fetch_modern_profile(client, game_dir)
        .await
        .map_err(|e| format!("Runtime expérimental : {e}"))?;
    let current = Os::current();
    let arch = std::env::consts::ARCH;

    // Upstream libraries (profile order — the tested set, wholesale).
    let libs = plan_modern_libraries(&profile, game_dir, current, arch, &HashSet::new());
    run.add_work(libs.len(), plan_bytes(&libs));

    // Coremods (size-checked — GitHub releases publish no hashes).
    let coremods = plan_modern_coremods(game_dir);
    run.add_work(coremods.len(), 0);

    // Optimization stack (separate opt-in — GTNHLib, Hodgepodge,
    // ArchaicFix, FalseTweaks + FalsePatternLib).
    let opt_mods = if optimizations {
        plan_modern_opt_mods()
    } else {
        Vec::new()
    };
    run.add_work(opt_mods.len(), 0);
    phase_log(&format!(
        "prevol-moderne fin libs={} coremods={} opt={}",
        libs.len(),
        coremods.len(),
        opt_mods.len()
    ));

    Ok(ModernPlan {
        libs,
        coremods,
        opt_mods,
    })
}

/// Provision the experimental runtime: upstream LWJGL3 library set, GraalVM
/// 21 (`java21/`, never clobbers `java/`), both coremods in `mods/`, the
/// optimization stack when `optimizations` is on, and seeded default
/// configs. `plan` comes from [`plan_modern_phase`] (totals already
/// registered); `cancel` aborts between phases.
async fn exec_modern_phase(
    run: &std::sync::Arc<InstallRun>,
    client: &reqwest::Client,
    game_dir: &std::path::Path,
    plan: ModernPlan,
    cancel: Option<Arc<AtomicBool>>,
    optimizations: bool,
) -> Result<ModernReport, String> {
    use launcher_core::{
        is_cancelled, seed_modern_configs, Os, MODERN_JAVA_DIR, MODERN_JAVA_MAJOR,
    };

    let cancelled = || is_cancelled(&cancel);
    let mut report = ModernReport::default();
    let current = Os::current();

    // GraalVM 21 (separate dir — the Java 8 game is untouched). Graal JIT
    // for the modded client; Temurin stays the Java 8 path. Normalement déjà
    // provisionné par la phase Java (repli local si besoin : plan + exécution
    // dédiés, jamais de double compte).
    let java21_bin = game_dir
        .join(MODERN_JAVA_DIR)
        .join("bin")
        .join(launcher_core::java_exe_name(current));
    if launcher_core::check_java(&java21_bin).major != Some(MODERN_JAVA_MAJOR) {
        if cancelled() {
            return Err("Installation annulée.".to_string());
        }
        let java_plan = plan_flavor_java(run, client, game_dir, true).await?;
        if exec_flavor_java(game_dir, &java_plan, run, cancel.clone()).await? {
            report.java21_provisioned = true;
        }
    }
    if cancelled() {
        return Err("Installation annulée.".to_string());
    }

    // Upstream libraries (profile order — the tested set, wholesale).
    if !plan.libs.is_empty() {
        let summary = download_many(
            client,
            &plan.libs,
            run,
            cancel.clone(),
            "Runtime expérimental",
        )
        .await?;
        report.libs_downloaded = summary.downloaded_files;
    }
    if cancelled() {
        return Err("Installation annulée.".to_string());
    }

    // Coremods (size-checked — GitHub releases publish no hashes).
    for (url, dest) in &plan.coremods {
        let file = format!(
            "mods/{}",
            dest.file_name().unwrap_or_default().to_string_lossy()
        );
        if download_size_checked(client, url, dest, &file, run, cancel.clone()).await? {
            report.coremods_downloaded += 1;
        }
        if cancelled() {
            return Err("Installation annulée.".to_string());
        }
    }

    // Optimization stack (separate opt-in — GTNHLib, Hodgepodge,
    // ArchaicFix, FalseTweaks + FalsePatternLib).
    if optimizations {
        for (url, file) in &plan.opt_mods {
            let dest = launcher_core::mods_dir(game_dir).join(file);
            let display = format!("mods/{file}");
            if download_size_checked(client, url, &dest, &display, run, cancel.clone()).await? {
                report.opt_mods_downloaded += 1;
            }
            if cancelled() {
                return Err("Installation annulée.".to_string());
            }
        }
    }

    // Load-bearing default configs (written only when absent — user edits
    // win). Without these the stack crashes before the menu.
    for line in seed_modern_configs(game_dir) {
        run.say(&line);
    }

    run.say(&format!(
        "Expérimental : prêt ({} lib(s), {} coremod(s){}, Java 21 {}).",
        report.libs_downloaded,
        report.coremods_downloaded,
        if optimizations {
            format!(", {} optimisation(s)", report.opt_mods_downloaded)
        } else {
            String::new()
        },
        if report.java21_provisioned {
            "installé"
        } else {
            "à jour"
        },
    ));
    Ok(report)
}

/// Modern verify items from the offline cache (empty without one): upstream
/// libraries (sha-verified) + coremod presence + Java 21 binary presence.
/// With `optimizations`, the opt-stack presence joins in too.
fn modern_verify_items(
    game_dir: &std::path::Path,
    optimizations: bool,
) -> Vec<(PathBuf, u64, String)> {
    use launcher_core::{
        load_cached_modern_profile, plan_modern_libraries, plan_modern_opt_mods, Os,
    };
    use std::collections::HashSet;

    let mut items = Vec::new();
    let Some(profile) = load_cached_modern_profile(game_dir) else {
        return items;
    };
    for planned in plan_modern_libraries(
        &profile,
        game_dir,
        Os::current(),
        std::env::consts::ARCH,
        &HashSet::new(),
    ) {
        items.push((planned.dest, planned.file.size, planned.file.sha1));
    }
    // Presence-only entries: reuse the file's own size so `needs_update`
    // reports stale if and only if the file is missing.
    for (_, dest) in launcher_core::plan_modern_coremods(game_dir) {
        if let Ok(meta) = std::fs::metadata(&dest) {
            items.push((dest, meta.len(), String::new()));
        } else {
            items.push((dest, 1, String::new()));
        }
    }
    if optimizations {
        let mods = launcher_core::mods_dir(game_dir);
        for (_, file) in plan_modern_opt_mods() {
            let dest = mods.join(&file);
            if let Ok(meta) = std::fs::metadata(&dest) {
                items.push((dest, meta.len(), String::new()));
            } else {
                items.push((dest, 1, String::new()));
            }
        }
    }
    let java21_bin = game_dir
        .join(launcher_core::MODERN_JAVA_DIR)
        .join("bin")
        .join(launcher_core::java_exe_name(Os::current()));
    if let Ok(meta) = std::fs::metadata(&java21_bin) {
        items.push((java21_bin, meta.len(), String::new()));
    } else {
        items.push((java21_bin, 1, String::new()));
    }
    items
}

/// Optional cheat client downloaded on explicit user opt-in only.
/// Served from the CDN; no SHA is published, so freshness is size-based
/// (`size > 0`) rather than hash-verified. Never installed by default.
pub const LOKI_URL: &str = "https://cdn.46anarchy.fr/files/loki.jar";
/// Destination file name inside `<gameDir>/mods` (direct `.jar` : vanilla
/// Forge découvre les `.jar`, plus de mapping `.pala`).
pub const LOKI_FILE_NAME: &str = "loki.jar";

// ---------------------------------------------------------------------------
// Server status (live Server List Ping) / patch notes
// ---------------------------------------------------------------------------

/// Default Minecraft server host (status ping target).
/// TODO: make host/port configurable in settings; hardcoded for now.
pub const MC_SERVER_HOST: &str = "mc.46anarchy.fr";
/// Default Minecraft server port (status ping target).
/// TODO: make host/port configurable in settings; hardcoded for now.
pub const MC_SERVER_PORT: u16 = 25566;

/// `true` when the CDN manifest already ships a Loki entry. In that case the
/// normal downloader already handles it and the explicit opt-in download
/// must be skipped to avoid a duplicate fetch.
pub fn manifest_lists_loki(manifest: &Manifest) -> bool {
    manifest
        .files
        .iter()
        .any(|f| f.name.eq_ignore_ascii_case(LOKI_FILE_NAME))
}

/// Fetch [`LOKI_URL`] into `<gameDir>/mods/loki.jar` (opt-in only).
///
/// No known SHA exists, so verification is limited to `size > 0` plus a
/// successful HTTP status; the reason is surfaced in the progress detail so
/// the Console log stays explicit. Streams start/done lines through `emit`
/// (same `install-progress` channel as the main install). Returns the bytes
/// written.
async fn download_loki_mod(
    game_dir: &Path,
    run: &InstallRun,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<u64, String> {
    if launcher_core::is_cancelled(&cancel) {
        return Err("Installation annulée.".to_string());
    }
    let dir = launcher_core::mods_dir(game_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Dossier mods inaccessible : {e}"))?;
    let dest = dir.join(LOKI_FILE_NAME);
    let file = format!("mods/{LOKI_FILE_NAME}");
    // Fichier enregistré (1 fichier, 0 octet) au moment du plan : ligne de
    // départ à compteurs inchangés, crédit groupé à la fin.
    run.emit_file_at(
        run.completed_bytes.load(Ordering::Relaxed),
        run.completed_files.load(Ordering::Relaxed),
        &file,
    );
    let client = http_client()?;
    let res = client
        .get(LOKI_URL)
        .send()
        .await
        .map_err(|e| format!("Téléchargement de Loki impossible : {e}"))?;
    let status = res.status();
    if !status.is_success() {
        return Err(format!("Téléchargement de Loki impossible : HTTP {status}"));
    }
    let bytes = res
        .bytes()
        .await
        .map_err(|e| format!("Téléchargement de Loki impossible : {e}"))?;
    if bytes.is_empty() {
        return Err("Téléchargement de Loki impossible : fichier vide reçu.".to_string());
    }
    let tmp = dir.join(format!("{LOKI_FILE_NAME}.tmp"));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("Écriture de Loki impossible : {e}"))?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("Écriture de Loki impossible : {e}"))?;
    let len = bytes.len() as u64;
    run.credit_unknown(&file, len);
    Ok(len)
}

/// Start a full install / update. Enforces the mandatory login gate
/// (`LOGIN_REQUIRED` when logged out). Streams `install-progress` events.
///
/// `include_loki` opts into the Loki cheat client *after* the normal
/// manifest downloads: when `true` (and the manifest does not already list
/// Loki), `https://cdn.46anarchy.fr/files/loki.jar` is fetched into
/// `<gameDir>/mods/loki.jar` (size-checked only — no published SHA).
/// `false` (default) keeps the historical behavior unchanged.
pub async fn start_install(
    state: &SharedState,
    emit: Option<Arc<ProgressEmitter>>,
    include_loki: bool,
) -> Result<InstallSummaryDto, String> {
    // The slot is claimed only AFTER the manifest is in hand: the CDN fetch
    // below can fail (offline, DNS, timeout) and must never wedge the
    // launcher — a stuck `install_running` rejects every later install AND
    // every play with zero progress events until the app restarts.
    {
        let s = state.lock().await;
        s.ensure_logged_in()?;
        if s.install_running {
            return Err("Installation déjà en cours…".to_string());
        }
    }
    let manifest = ensure_manifest(state).await?;
    let cancel = {
        let mut s = state.lock().await;
        if s.install_running {
            return Err("Installation déjà en cours…".to_string());
        }
        s.install_running = true;
        // Fresh flag per run: a stale cancel never kills a new install.
        s.install_cancel.store(false, Ordering::Relaxed);
        s.install_cancel.clone()
    };
    let (game_dir, experimental) = {
        let s = state.lock().await;
        (s.game_dir(), s.settings.experimental_runtime)
    };
    // Le drapeau est toujours libéré (succès comme échec) : un seul point de
    // sortie, plus de `install_running` coincé après une phase tardive.
    let out = start_install_pipeline(
        state,
        emit,
        &manifest,
        &game_dir,
        experimental,
        include_loki,
        cancel,
    )
    .await;
    state.lock().await.install_running = false;
    out
}

/// Pipeline d'installation sur un [`InstallRun`] partagé : total CDN
/// provisoire D'ABORD (émissions immédiates), pré-vols en parallèle,
/// clôture à 1.0.
///
/// Ordonnancement (changement voulu, seul changement de comportement :
/// le calendrier) : le plan CDN purement local est enregistré et annoncé
/// dès la première milliseconde, puis sa vérification/téléchargement tourne
/// EN PARALLÈLE des pré-vols lents et indépendants (métadonnées Mojang,
/// HEAD Java, profil LWJGL3). Ensembles de fichiers disjoints, seul `run`
/// (compteurs atomiques) est partagé — aucune autre dépendance, donc le
/// parallélisme ne change que le calendrier. Les totaux Mojang/Java
/// agrandissent le total à leur arrivée (deux temps documenté au-dessus
/// d'`InstallRun`) : les premières émissions ne sont jamais bloquées sur
/// des totaux tardifs.
async fn start_install_pipeline(
    state: &SharedState,
    emit: Option<Arc<ProgressEmitter>>,
    manifest: &Manifest,
    game_dir: &Path,
    experimental: bool,
    include_loki: bool,
    cancel: Arc<AtomicBool>,
) -> Result<InstallSummaryDto, String> {
    // Sondage barres : le magasin est vidé au démarrage du run, chaque
    // émission suivante y est enregistrée (événements inchangés).
    let emit = prepare_install_emit(state, emit).await;
    let run = std::sync::Arc::new(InstallRun::new(emit));
    phase_log("plan debut");
    // Repair pass first: stray `.pala` files (PalaForge-era build) migrate
    // back to `.jar` (no re-download — bytes are correct) so the freshness
    // check below sees the final destinations and the prune sees direct names.
    let migrated = launcher_core::migrate_mods_to_jar(game_dir);
    if !migrated.is_empty() {
        run.say(&format!("Mods renommés en .jar : {}.", migrated.join(", ")));
    }
    // Branch swap (never merge, never delete): stash the modern stack on
    // classic-activate, restore it on experimental-activate, before
    // downloading — a flavor switch can't leave both stacks active (Forge
    // loads every jar in mods/). Each move is logged in French.
    let pruned = launcher_core::prune_other_flavor(game_dir, experimental);
    for line in flavor_swap_lines(&pruned, experimental) {
        run.say(&line);
    }
    let client = http_client()?;
    // Cale palaforge supprimée : le modid nu `palaforge` n'est requis par rien (`palaforge-utils` est la vraie dépendance, couverte par le manifeste).
    // PLAN CDN (local pur, instantané) : total provisoire + annonce
    // immédiate — la vérification démarre contre lui sans attendre les
    // pré-vols réseau.
    let cdn = cdn_plan_for_mode(manifest, game_dir, experimental);
    let cdn_bytes = plan_bytes(&cdn);
    run.add_work(cdn.len(), cdn_bytes);
    phase_log(&format!(
        "plan-cdn fichiers={} octets={cdn_bytes}",
        cdn.len()
    ));
    run.say(&format!(
        "Préparation : {} fichier(s) — vérification…",
        cdn.len()
    ));
    let loki_listed = manifest_lists_loki(manifest);
    let want_loki = include_loki && !loki_listed;
    if want_loki {
        // 1 fichier, taille inconnue (crédit groupé à la fin).
        run.add_work(1, 0);
    }
    let optimizations = state.lock().await.settings.experimental_optimizations;
    // EXÉCUTION CDN + PRÉ-VOLS en parallèle (disjoints, voir doc ci-dessus).
    let (cdn_out, mojang, java, modern) = tokio::join!(
        run_planned(cdn, &run, Some(cancel.clone())),
        plan_mojang_phase(&run, &client, manifest, game_dir, experimental),
        plan_flavor_java(&run, &client, game_dir, experimental),
        async {
            if experimental {
                Some(plan_modern_phase(&run, &client, game_dir, optimizations).await)
            } else {
                None
            }
        },
    );
    let summary = cdn_out?;
    let mojang = mojang?;
    let java = java?;
    let modern = match modern {
        Some(Ok(plan)) => Some(plan),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    phase_log("plan fin (totaux complets)");
    if include_loki {
        if loki_listed {
            // Already covered by the manifest downloads — no duplicate
            // fetch; still surface a clear line on the progress channel.
            run.say("Loki déjà fourni par le manifeste — téléchargement optionnel ignoré.");
        } else {
            download_loki_mod(game_dir, &run, Some(cancel.clone())).await?;
        }
    }
    // Mojang fallback: vanilla jar, per-OS natives, missing
    // libraries, assets, Log4Shell hardening.
    phase_log("execution mojang debut");
    exec_mojang_phase(
        &run,
        &client,
        game_dir,
        mojang,
        Some(cancel.clone()),
        experimental,
    )
    .await?;
    phase_log("execution mojang fin");
    // Flavor runtime (auto-managed, no manual download): Temurin 8
    // on default, GraalVM 21 on experimental. A missing runtime is
    // provisioned here so launch never meets an empty `java/`.
    phase_log("execution java debut");
    exec_flavor_java(game_dir, &java, &run, Some(cancel.clone())).await?;
    phase_log("execution java fin");
    // Experimental LWJGL3 runtime (flag-gated, default off; its
    // internal GraalVM check is a no-op when provisioned above).
    if let Some(modern_plan) = modern {
        let optimizations = state.lock().await.settings.experimental_optimizations;
        phase_log("execution moderne debut");
        exec_modern_phase(
            &run,
            &client,
            game_dir,
            modern_plan,
            Some(cancel.clone()),
            optimizations,
        )
        .await?;
        phase_log("execution moderne fin");
    }
    // Garde bytecode plan-time : avertissements français, jars nommés
    // (warn-only — FML ignore les jars illisibles et continue, donc
    // jamais d'échec). Le balayage ouvre chaque jar du classpath (secondes
    // sur un gros `libraries/`) : annoncé avant, jamais en silence.
    phase_log("finalisation bytecode debut");
    run.say("Vérification des versions de classes…");
    for warning in assert_classpath_bytecode(manifest, game_dir, experimental) {
        run.say(&warning);
    }
    phase_log("finalisation bytecode fin");
    // Le pipeline possède toute la barre : il la ferme à 1.0 pile.
    phase_log("finalisation debut");
    run.finish();
    phase_log("finalisation fin");
    // Persistance disque hors mutex (même ordre disque-puis-mémoire qu'avant,
    // sans tenir le verrou pendant l'écriture).
    let version = manifest.version.clone();
    let snapshot = manifest.clone();
    let _ = save_manifest(&launcher_core::cdn_last_path(game_dir), &snapshot);
    launcher_core::write_installed_flavor(game_dir, experimental);
    {
        let mut s = state.lock().await;
        s.update_status = UpdateStatus::Ok { version };
        s.manifest = Some(snapshot);
    }
    Ok(summary_dto(&summary))
}

/// Mise à niveau avant lancement (chemin JOUER) : mêmes phases que
/// `start_install_pipeline`, jouées seulement quand leur garde fraîcheur
/// l'exige — total CDN provisoire d'abord, pré-vols en parallèle (deux
/// temps documenté au-dessus d'`InstallRun`), pas de clôture à 1.0 (le
/// jeu démarre juste après, le frontend masque la barre).
#[allow(clippy::too_many_arguments)]
async fn play_repair_pipeline(
    state: &SharedState,
    emit: Option<Arc<ProgressEmitter>>,
    manifest: &Manifest,
    game_dir: &Path,
    experimental: bool,
    full_cdn: bool,
    gap: Vec<launcher_core::PlannedFile>,
    want_mojang: bool,
    want_java: bool,
    want_modern: bool,
    cancel: Arc<AtomicBool>,
) -> Result<(), String> {
    // Sondage barres : même contrat que le pipeline d'installation.
    let emit = prepare_install_emit(state, emit).await;
    let run = std::sync::Arc::new(InstallRun::new(emit));
    phase_log("plan debut (jouer)");
    let client = http_client()?;
    // PLAN CDN (local pur, instantané) : total provisoire + annonce immédiate.
    let cdn = if full_cdn {
        Some(cdn_plan_for_mode(manifest, game_dir, experimental))
    } else {
        None
    };
    if let Some(ref cdn) = cdn {
        run.add_work(cdn.len(), plan_bytes(cdn));
        phase_log(&format!("plan-cdn fichiers={} (jouer)", cdn.len()));
        run.say(&format!(
            "Préparation : {} fichier(s) — vérification…",
            cdn.len()
        ));
    } else if !gap.is_empty() {
        run.add_work(gap.len(), plan_bytes(&gap));
        phase_log(&format!(
            "plan-cdn fichiers={} (jouer, comblement)",
            gap.len()
        ));
        run.say(&format!(
            "Préparation : {} fichier(s) — vérification…",
            gap.len()
        ));
    }
    let optimizations = state.lock().await.settings.experimental_optimizations;
    // EXÉCUTION CDN + PRÉ-VOLS en parallèle (disjoints — même contrat que
    // `start_install_pipeline` : seul `run` est partagé).
    let want_modern_inner = want_modern;
    let (cdn_out, mojang, java, modern) = tokio::join!(
        async {
            // Persistance disque hors mutex (même ordre disque-puis-mémoire).
            if let Some(cdn) = cdn {
                let r = run_planned(cdn, &run, Some(cancel.clone())).await;
                let snapshot = manifest.clone();
                let version = snapshot.version.clone();
                let _ = save_manifest(&launcher_core::cdn_last_path(game_dir), &snapshot);
                {
                    let mut s = state.lock().await;
                    s.update_status = UpdateStatus::Ok { version };
                    s.manifest = Some(snapshot);
                }
                Some(r)
            } else if !gap.is_empty() {
                // Version inchangée, seuls des fichiers de contenu manquent :
                // comblement ciblé (aucun re-hachage du présent — `download_files`
                // re-hacherait ~500 fichiers à chaque JOUER, la corruption
                // silencieuse reste du ressort de l'installation / Vérifier).
                let r = run_planned(gap, &run, Some(cancel.clone())).await;
                let snapshot = manifest.clone();
                let version = snapshot.version.clone();
                let _ = save_manifest(&launcher_core::cdn_last_path(game_dir), &snapshot);
                {
                    let mut s = state.lock().await;
                    s.update_status = UpdateStatus::Ok { version };
                    s.manifest = Some(snapshot);
                }
                Some(r)
            } else {
                None
            }
        },
        async {
            if want_mojang {
                Some(plan_mojang_phase(&run, &client, manifest, game_dir, experimental).await)
            } else {
                None
            }
        },
        async {
            if want_java {
                Some(plan_flavor_java(&run, &client, game_dir, experimental).await)
            } else {
                None
            }
        },
        async {
            if want_modern_inner {
                Some((
                    plan_modern_phase(&run, &client, game_dir, optimizations).await,
                    optimizations,
                ))
            } else {
                None
            }
        },
    );
    if let Some(r) = cdn_out {
        r?;
    }
    let mojang = match mojang {
        Some(Ok(plan)) => Some(plan),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    let java = match java {
        Some(Ok(plan)) => Some(plan),
        Some(Err(e)) => return Err(e),
        None => None,
    };
    let modern = match modern {
        Some((Ok(plan), opt)) => Some((plan, opt)),
        Some((Err(e), _)) => return Err(e),
        None => None,
    };
    phase_log("plan fin (jouer, totaux complets)");
    if let Some(plan) = mojang {
        exec_mojang_phase(
            &run,
            &client,
            game_dir,
            plan,
            Some(cancel.clone()),
            experimental,
        )
        .await?;
    }
    if let Some(plan) = java {
        exec_flavor_java(game_dir, &plan, &run, Some(cancel.clone())).await?;
    }
    if let Some((plan, optimizations)) = modern {
        exec_modern_phase(
            &run,
            &client,
            game_dir,
            plan,
            Some(cancel.clone()),
            optimizations,
        )
        .await?;
    }
    Ok(())
}

/// Uninstall the game payload (mods, libraries, versions, assets, … —
/// settings, runtimes and logs are kept). Resets the manifest status so
/// the main button flips back to INSTALLER. Refuses to run mid-install.
pub async fn uninstall_game(state: &SharedState) -> Result<UninstallReportDto, String> {
    let game_dir = {
        let s = state.lock().await;
        if s.install_running {
            return Err("Installation en cours…".to_string());
        }
        s.game_dir()
    };
    let report = launcher_core::uninstall_instance(&game_dir)
        .map_err(|e| format!("Désinstallation : {e}"))?;
    {
        let mut s = state.lock().await;
        s.manifest = None;
        s.update_status = launcher_core::UpdateStatus::Init;
    }
    Ok(UninstallReportDto {
        removed_files: report.removed_files,
        removed_dirs: report.removed_dirs,
    })
}

/// Check file freshness without downloading.
pub async fn verify_files(state: &SharedState) -> Result<VerifyReportDto, String> {
    let manifest = ensure_manifest(state).await?;
    // Instantané unique sous un seul verrou court (évite deux prises).
    let (game_dir, experimental, optimizations) = {
        let s = state.lock().await;
        (
            s.game_dir(),
            s.settings.experimental_runtime,
            s.settings.experimental_optimizations,
        )
    };
    let planned = plan_downloads_for_current_os(&manifest, &game_dir);
    let mut items: Vec<(PathBuf, u64, String)> = Vec::with_capacity(planned.len() + 32);
    items.extend(
        planned
            .iter()
            .map(|p| (p.dest.clone(), p.file.size, p.file.sha1.clone())),
    );
    // Mojang-side files from the offline cache (vanilla jar, per-OS
    // libraries, asset objects): no network, same freshness rules
    // (Log4j sidecar included — a patched jar never reports stale).
    items.extend(mojang_verify_items(&game_dir, &manifest));
    // Experimental set from its offline cache (upstream libs with real
    // hashes + presence checks for coremods and the Java 21 binary).
    if experimental {
        items.extend(modern_verify_items(&game_dir, optimizations));
    }
    let report = tokio::task::spawn_blocking(move || {
        let mut stale = 0usize;
        let mut stale_bytes = 0u64;
        for (dest, size, sha1) in &items {
            if needs_update(dest, *size, sha1) {
                stale += 1;
                stale_bytes += *size;
            }
        }
        VerifyReportDto {
            total: items.len(),
            stale,
            stale_bytes,
        }
    })
    .await
    .map_err(|e| format!("Vérification interrompue : {e}"))?;
    Ok(report)
}

/// Mojang-side verify items from the offline cache (empty without one):
/// vanilla jar + per-OS libraries (same CDN-skip rules as the install) +
/// cached asset objects. Natives are derived (extracted), not verified.
///
/// Plus the pinned Forge 1614 universal plan (cache-independent): the file
/// is provisioned by the install on both flavors, so verify covers it with
/// the same freshness rules and no network.
fn mojang_verify_items(
    game_dir: &std::path::Path,
    manifest: &launcher_core::Manifest,
) -> Vec<(PathBuf, u64, String)> {
    use launcher_core::{
        artifact_base, load_cached_asset_index, load_cached_mojang_version, plan_mojang_assets,
        plan_mojang_libraries, plan_vanilla_jar, Os,
    };
    use std::collections::HashSet;

    let mut items = Vec::new();
    let forge = launcher_core::plan_forge_universal(game_dir);
    items.push((forge.dest, forge.file.size, forge.file.sha1));
    let Some(version) = load_cached_mojang_version(game_dir) else {
        return items;
    };
    let current = Os::current();
    let cdn_planned = launcher_core::plan_downloads(manifest, game_dir, current);
    let mut skip_names: HashSet<String> = HashSet::with_capacity(cdn_planned.len());
    skip_names.extend(cdn_planned.iter().map(|p| p.file.name.clone()));
    let mut skip_bases: HashSet<String> = HashSet::with_capacity(cdn_planned.len());
    skip_bases.extend(cdn_planned.iter().map(|p| artifact_base(&p.file.name)));
    let vanilla = plan_vanilla_jar(&version, game_dir);
    items.push((vanilla.dest, vanilla.file.size, vanilla.file.sha1));
    let (regular, _) = plan_mojang_libraries(
        &version,
        game_dir,
        current,
        &skip_names,
        &skip_bases,
        current != Os::Windows,
    );
    items.extend(
        regular
            .into_iter()
            .map(|p| (p.dest, p.file.size, p.file.sha1)),
    );
    if let Some(index) = load_cached_asset_index(game_dir) {
        items.extend(
            plan_mojang_assets(&index, game_dir)
                .into_iter()
                .map(|p| (p.dest, p.file.size, p.file.sha1)),
        );
    }
    items
}

// ---------------------------------------------------------------------------
// Server status / patch notes (live home-screen data, no fake numbers)
// ---------------------------------------------------------------------------

/// Live Server List Ping against [`MC_SERVER_HOST`]:[`MC_SERVER_PORT`].
///
/// Handshake (`0x00`) + status request (`0x00`) + ping/pong (`0x01`) over
/// `tokio` `TcpStream` (5s timeout). On failure returns
/// `{online: false, error}` — errors propagate, never fabricated.
/// `tps` is always `None` (TPS non exposé par le ping).
pub async fn get_server_status(_state: &SharedState) -> ServerStatusDto {
    // Ping TCP avec timeout : gardé au premier affichage (la carte serveur
    // de l'accueil en dépend), durée journalisée en une ligne.
    let started = std::time::Instant::now();
    let address = format!("{MC_SERVER_HOST}:{MC_SERVER_PORT}");
    let out = match crate::server_status::ping_server(
        MC_SERVER_HOST,
        MC_SERVER_PORT,
        crate::server_status::STATUS_TIMEOUT,
    )
    .await
    {
        Ok((info, ping_ms)) => ServerStatusDto {
            online: true,
            address,
            players_online: Some(info.players_online),
            players_max: Some(info.players_max),
            version: Some(info.version),
            motd: Some(info.motd),
            ping_ms: Some(ping_ms),
            tps: None,
            error: None,
        },
        Err(e) => ServerStatusDto::offline(address, e),
    };
    boot_timing_log("statut serveur", started);
    out
}

/// Patch-notes feed (newest first, max 5, never errors) + reachability
/// verdict: local `CHANGELOG.md` + `"Version installée"` manifest summary +
/// best-effort GitHub commits across 6 repos (failures degrade to
/// local-only entries; total failure sets `unreachable` so the UI shows
/// the muted offline hint instead of a silent empty state).
pub async fn get_patch_notes(state: &SharedState) -> PatchNotesFeedDto {
    // Lecture locale instantanée + fetch GitHub (6 dépôts en parallèle,
    // 10 s + un réessai sur timeout — voir `remote_release_entries`) :
    // gardé au premier affichage (la colonne notes de l'accueil en dépend).
    let started = std::time::Instant::now();
    let (game_dir, manifest) = {
        let s = state.lock().await;
        (s.game_dir(), s.manifest.clone())
    };
    let local = crate::patch_notes::local_changelog_entries(&game_dir);
    let installed = crate::patch_notes::installed_version_entry(manifest.as_ref());
    let manifest_version = manifest
        .as_ref()
        .map(|m| m.version.clone())
        .unwrap_or_default();
    let (remote, unreachable) = crate::patch_notes::remote_release_entries().await;
    let notes = crate::patch_notes::merge_patch_notes(remote, local, installed, &manifest_version);
    boot_timing_log(
        &format!("notes de patch ({} entrée(s))", notes.len()),
        started,
    );
    PatchNotesFeedDto { notes, unreachable }
}

// ---------------------------------------------------------------------------
// Launcher version (bottom status bar, never the modpack version)
// ---------------------------------------------------------------------------

/// Default GitHub repo (`owner/name`) for the launcher release lookup,
/// used when the compile-time env `LAUNCHER_GITHUB_REPO` is absent. The
/// repo may have no releases yet (API 404 → hash fallback, correct
/// behavior) ; the tag lights up on its own once releases land.
/// Configurez avec `LAUNCHER_GITHUB_REPO=owner/name` au moment de la
/// compilation pour remplacer ce défaut.
pub const DEFAULT_GITHUB_REPO: &str = "46Anarchy/46Launcher";
/// Raw compile-time override (`None` = absent at build time).
pub const LAUNCHER_GITHUB_REPO: Option<&str> = option_env!("LAUNCHER_GITHUB_REPO");

/// Effective repo for the release lookup: the compile-time env wins when
/// set (an explicitly empty value still means "skip GitHub"), else the
/// [`DEFAULT_GITHUB_REPO`] above. Pure (unit-tested).
pub fn effective_github_repo() -> Option<&'static str> {
    match LAUNCHER_GITHUB_REPO {
        None => Some(DEFAULT_GITHUB_REPO),
        Some(repo) if repo.trim().is_empty() => None,
        Some(repo) => Some(repo),
    }
}
/// Timeout of the GitHub `releases/latest` lookup.
const LAUNCHER_RELEASE_TIMEOUT_SECS: u64 = 8;

/// Short git hash baked by `crates/ui/build.rs` (`"dev"` when unknown).
fn launcher_git_hash() -> Option<String> {
    option_env!("LAUNCHER_GIT_HASH")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "dev")
}

/// Pure resolution (unit-tested): a fetched `tag` wins (displayed as-is),
/// else the baked git hash, else `"dev"`. Never fails.
pub fn resolve_launcher_version(tag: Option<&str>, git_hash: Option<&str>) -> VersionInfo {
    let tag = tag.map(str::trim).filter(|s| !s.is_empty());
    if let Some(tag) = tag {
        return VersionInfo {
            display: tag.to_string(),
            detail: format!("Launcher 46Anarchy — release GitHub {tag}"),
        };
    }
    let hash = git_hash.map(str::trim).filter(|s| !s.is_empty());
    if let Some(hash) = hash {
        return VersionInfo {
            display: hash.to_string(),
            detail: format!("Launcher 46Anarchy — commit {hash}"),
        };
    }
    VersionInfo {
        display: "dev".to_string(),
        detail: "Launcher 46Anarchy — version de développement".to_string(),
    }
}

/// Fetch the latest GitHub release tag for `repo` (`owner/name`).
/// `None` on ANY failure (offline, 404/no releases, parse error).
async fn fetch_latest_release_tag(client: &reqwest::Client, repo: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let res = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "46anarchy-launcher")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(
            LAUNCHER_RELEASE_TIMEOUT_SECS,
        ))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    let value: serde_json::Value = res.json().await.ok()?;
    value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Launcher version for the bottom bar. NEVER errors (worst case
/// `display == "dev"`). One GitHub fetch per boot at most (process-once
/// cache); every failure degrades to the baked hash / `"dev"`.
pub async fn get_launcher_version() -> VersionInfo {
    static CACHE: std::sync::OnceLock<VersionInfo> = std::sync::OnceLock::new();
    if let Some(cached) = CACHE.get() {
        return cached.clone();
    }
    let fresh = resolve_launcher_version_for_repo(effective_github_repo()).await;
    let _ = CACHE.set(fresh.clone());
    fresh
}

/// Uncached lookup for `repo` (one GitHub fetch at most): fetched tag wins,
/// else the baked hash, else `"dev"`. `None` = GitHub skipped (offline-first).
async fn resolve_launcher_version_for_repo(repo: Option<&str>) -> VersionInfo {
    let tag = match repo {
        Some(repo) => match http_client() {
            Ok(client) => fetch_latest_release_tag(&client, repo).await,
            Err(_) => None,
        },
        None => None,
    };
    let hash = launcher_git_hash();
    resolve_launcher_version(tag.as_deref(), hash.as_deref())
}

// ---------------------------------------------------------------------------
// Tampon d'identité du build
// ---------------------------------------------------------------------------

/// FNV-1a 32 bits sur les octets (8 hex minuscules). Miroir exact de
/// `build_identity_hash` dans `build.rs` (compilation) et du snippet
/// Python du rapport : même constante, même ordre des fichiers.
pub fn build_identity_hash(bytes: &[u8]) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// Haché de compilation, `"?"` quand absent ou vide (ne casse jamais rien).
fn baked_build_hash(env: Option<&str>) -> String {
    env.map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("?")
        .to_string()
}

/// Tampon d'identité du build (jamais d'erreur) : dit EXACTEMENT quel code
/// la fenêtre exécute (interface + moteur), journalisé au démarrage.
pub async fn get_build_info() -> BuildInfoDto {
    BuildInfoDto {
        frontend_hash: baked_build_hash(option_env!("LAUNCHER_FRONTEND_HASH")),
        backend_hash: baked_build_hash(option_env!("LAUNCHER_BACKEND_HASH")),
    }
}

// ---------------------------------------------------------------------------
// Mods manager
// ---------------------------------------------------------------------------

/// Suffix marking a locally disabled optional mod.
pub const DISABLED_SUFFIX: &str = ".disabled";

/// List every mod for the Mods screen: CDN manifest entries with
/// `model == "mods"` (or a model whose `dest` is `"mods"`) are `required`
/// and locked; extra `.jar` / `.jar.disabled` files found in the on-disk
/// `mods` folder are optional local mods with a working toggle.
///
/// Manifest membership compares plain file names (direct-name semantics :
/// vanilla Forge découvre les `.jar`, le disque et le manifeste parlent le
/// même langage depuis la fin de l'épisode PalaForge `.pala`).
///
/// TODO: the CDN manifest carries no per-file `required` flag, so "required"
/// currently means "listed in the manifest". If the manifest ever gains an
/// explicit flag, use it here instead of manifest membership.
pub async fn get_mods_list(state: &SharedState) -> Result<Vec<ModInfo>, String> {
    // Balayage disque + lecture `mcmod.info` de chaque jar (~70 zips) :
    // gardé au premier affichage (la ligne « N mods » de l'accueil vient du
    // nombre renvoyé), durée journalisée en une ligne. Les lectures zip sont
    // locales et bornées — aucun réseau, rien à paralléliser au-delà du
    // fan-out frontend déjà concurrent.
    let started = std::time::Instant::now();
    let (game_dir, manifest) = {
        let s = state.lock().await;
        (s.game_dir(), s.manifest.clone())
    };
    let out = list_mods(&game_dir, manifest.as_ref());
    boot_timing_log(&format!("liste mods ({} mod(s))", out.len()), started);
    Ok(out)
}

/// Enable/disable an optional mod by renaming `<id>` ↔ `<id>.disabled`
/// (direct `.jar` names — vanilla Forge). Refuses manifest-listed
/// (required) mods.
pub async fn toggle_mod(state: &SharedState, id: String, enabled: bool) -> Result<ModInfo, String> {
    let (game_dir, manifest) = {
        let s = state.lock().await;
        (s.game_dir(), s.manifest.clone())
    };
    set_mod_enabled(&game_dir, manifest.as_ref(), &id, enabled)
}

/// Reveal the `mods` folder in the OS file manager (creates it first).
pub async fn open_mods_folder(state: &SharedState) -> Result<String, String> {
    let game_dir = state.lock().await.game_dir();
    open_folder(&launcher_core::mods_dir(&game_dir))
}

/// Reveal the game root folder in the OS file manager (used by Console).
pub async fn open_game_folder(state: &SharedState) -> Result<String, String> {
    let game_dir = state.lock().await.game_dir();
    open_folder(&game_dir)
}

/// Read `mcmod.info` from a local mod file (a zip under `.jar`,
/// best effort): array form or `{modlist: [...]}` — `name` / `version` /
/// `description` of the first entry win. Returns `None` when absent,
/// unreadable, or unusable (so the caller falls back to the
/// filename-derived display name).
fn read_mcmod_info(jar_path: &Path) -> Option<(String, String, String)> {
    let file = std::fs::File::open(jar_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    // Canonical `mcmod.info` at the archive root (case-insensitive scan
    // as a fallback — some jars capitalize it).
    let index = (0..archive.len()).find(|i| {
        archive
            .name_for_index(*i)
            .is_some_and(|n| n.eq_ignore_ascii_case("mcmod.info"))
    })?;
    let mut entry = archive.by_index(index).ok()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut entry, &mut text).ok()?;
    drop(entry);
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let entries = if let Some(list) = value.as_array() {
        list.clone()
    } else {
        value.get("modlist")?.as_array()?.clone()
    };
    let first = entries.first()?;
    let pick = |key: &str| {
        first
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let info = (pick("name"), pick("version"), pick("description"));
    if info.0.is_empty() && info.1.is_empty() && info.2.is_empty() {
        return None;
    }
    Some(info)
}

/// Stem of a mod file name: the trailing `.jar` stripped when present
/// (display + manifest-membership comparisons).
/// Disabled handling stays with the caller (`.disabled` stripped first).
fn mod_stem(name: &str) -> &str {
    name.strip_suffix(".jar").unwrap_or(name)
}

/// Pure (network-free) mod listing over `game_dir` + an optional manifest.
///
/// The local disk is the truth: only `.jar` files physically in `mods/`
/// (plus `.jar.disabled`) are listed — a manifest mod missing from disk
/// simply doesn't appear. Manifest membership only decides `required`
/// (locked toggle, direct-name comparison); `mcmod.info` inside each local
/// file provides the real display name/version (filename-derived fallback,
/// `.jar` stripped).
fn list_mods(game_dir: &Path, manifest: Option<&Manifest>) -> Vec<ModInfo> {
    use std::collections::{BTreeMap, BTreeSet};

    // Manifest-declared mod names: model key "mods" or any model with
    // dest "mods" (plus their model version/description as fallback).
    let mut declared: BTreeMap<String, (String, String)> = BTreeMap::new();
    if let Some(m) = manifest {
        let mods_models: Vec<&str> = m
            .models
            .iter()
            .filter(|model| model.name == "mods" || model.dest == "mods")
            .map(|model| model.name.as_str())
            .collect();
        for model_name in &mods_models {
            let model = m.model_by_name(model_name);
            let (version, description) = model
                .map(|x| {
                    (
                        x.version.clone().unwrap_or_default(),
                        x.description.clone().unwrap_or_default(),
                    )
                })
                .unwrap_or_default();
            for f in m.files.iter().filter(|f| &f.model == model_name) {
                declared
                    .entry(f.name.clone())
                    .or_insert((version.clone(), description.clone()));
            }
        }
    }
    let required_stems: BTreeSet<String> =
        declared.keys().map(|n| mod_stem(n).to_string()).collect();

    // On-disk state: `<id>.jar` (enabled) vs `<id>.jar.disabled` (off);
    // `id` is the on-disk `.jar` name. Only Forge-visible files list.
    let mut on_disk: BTreeMap<String, (String, u64)> = BTreeMap::new();
    let dir = launcher_core::mods_dir(game_dir);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (base, enabled) = match name.strip_suffix(DISABLED_SUFFIX) {
                Some(rest) => (rest.to_string(), false),
                None => (name.clone(), true),
            };
            if !base.ends_with(".jar") {
                continue;
            }
            let size = entry.metadata().map(|md| md.len()).unwrap_or(0);
            // Enabled wins when both `<id>` and `<id>.disabled` exist.
            on_disk
                .entry(base)
                .and_modify(|e| {
                    if enabled {
                        *e = (name.clone(), size);
                    }
                })
                .or_insert((name, size));
        }
    }

    // Index par stem (O(1) par mod disque au lieu du balayage O(D×M)).
    let by_stem: BTreeMap<String, (String, String)> = declared
        .iter()
        .map(|(n, v)| (mod_stem(n).to_string(), v.clone()))
        .collect();
    // Disk-only listing, required first then alphabetical.
    let mut out: Vec<ModInfo> = Vec::with_capacity(on_disk.len());
    out.extend(on_disk.iter().map(|(id, (file_name, disk_size))| {
        let stem = mod_stem(id);
        let required = required_stems.contains(stem);
        let enabled = !file_name.ends_with(DISABLED_SUFFIX);
        let (fallback_version, fallback_description) =
            by_stem.get(stem).cloned().unwrap_or_default();
        let filename_display = mod_stem(id).replace(['_', '-'], " ");
        let (display_name, version, description) = match read_mcmod_info(&dir.join(file_name)) {
            Some((name, ver, desc)) => (
                if name.is_empty() {
                    filename_display.clone()
                } else {
                    name
                },
                if ver.is_empty() {
                    fallback_version.clone()
                } else {
                    ver
                },
                if desc.is_empty() {
                    fallback_description.clone()
                } else {
                    desc
                },
            ),
            None => (filename_display, fallback_version, fallback_description),
        };
        ModInfo {
            id: id.clone(),
            file_name: file_name.clone(),
            display_name,
            description,
            version,
            size: *disk_size,
            required,
            enabled,
        }
    }));
    out.sort_by(|a, b| b.required.cmp(&a.required).then(a.id.cmp(&b.id)));
    out
}

/// Rename-based toggle for one optional mod (headless, unit-tested).
///
/// `id` is the on-disk `.jar` name (direct-name semantics — vanilla Forge).
/// Required-ness compares manifest names directly (both sides are `.jar`).
fn set_mod_enabled(
    game_dir: &Path,
    manifest: Option<&Manifest>,
    id: &str,
    enabled: bool,
) -> Result<ModInfo, String> {
    if id.contains(['/', '\\']) || id.contains("..") {
        return Err("Nom de mod invalide.".to_string());
    }
    let canonical = id.to_string();
    if !canonical.ends_with(".jar") {
        return Err("Nom de mod invalide.".to_string());
    }
    if let Some(m) = manifest {
        let mods_models: Vec<&str> = m
            .models
            .iter()
            .filter(|model| model.name == "mods" || model.dest == "mods")
            .map(|model| model.name.as_str())
            .collect();
        let listed = m
            .files
            .iter()
            .any(|f| mods_models.contains(&f.model.as_str()) && f.name == canonical);
        if listed {
            return Err("Ce mod est obligatoire et ne peut pas être désactivé.".to_string());
        }
    }
    let dir = launcher_core::mods_dir(game_dir);
    let enabled_path = dir.join(&canonical);
    let disabled_path = dir.join(format!("{canonical}{DISABLED_SUFFIX}"));
    let (from, to) = if enabled {
        (&disabled_path, &enabled_path)
    } else {
        (&enabled_path, &disabled_path)
    };
    if to.exists() && !from.exists() {
        // Already in the requested state.
    } else if from.exists() {
        std::fs::rename(from, to).map_err(|e| format!("Activation du mod impossible : {e}"))?;
    } else {
        return Err("Mod introuvable dans le dossier mods.".to_string());
    }
    Ok(list_mods(game_dir, manifest)
        .into_iter()
        .find(|x| x.id == canonical)
        .unwrap_or(ModInfo {
            id: canonical.clone(),
            file_name: to
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            display_name: mod_stem(&canonical).to_string(),
            description: String::new(),
            version: String::new(),
            size: 0,
            required: false,
            enabled,
        }))
}

/// Create `path` if needed, then reveal it in the OS file manager
/// (detached `explorer` / `open` / `xdg-open`).
fn open_folder(path: &Path) -> Result<String, String> {
    std::fs::create_dir_all(path).map_err(|e| format!("Dossier inaccessible : {e}"))?;
    spawn_detached(open_folder_command(path))
        .map_err(|e| format!("Ouverture du dossier impossible : {e}"))?;
    Ok(path.to_string_lossy().into_owned())
}

/// OS file-manager command for `path` (detached, stdio nulled by caller).
fn open_folder_command(path: &Path) -> std::process::Command {
    #[cfg(target_os = "windows")]
    let cmd = {
        let mut c = std::process::Command::new("explorer");
        c.arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    cmd
}

/// Spawn a detached OS command with nulled stdio.
fn spawn_detached(mut cmd: std::process::Command) -> std::io::Result<()> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Hosts the launcher may open in the OS default browser (explicit
/// allowlist — `open_external` refuses anything else, so a compromised
/// renderer can never turn `invoke` into an open-redirect).
const ALLOWED_EXTERNAL_HOSTS: &[&str] = &[
    "microsoft.com",
    "login.microsoftonline.com",
    "login.live.com",
    "46anarchy.fr",
    "cdn.46anarchy.fr",
    "discord.46anarchy.fr",
    "discord.gg",
    "discord.com",
    "adoptium.net",
    "github.com",
];

/// Validate an external-browser URL against the allowlist (pure, unit-tested).
fn validate_external_url(url: &str) -> Result<String, String> {
    let url = url.trim().to_string();
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .ok_or_else(|| "Lien refusé (https uniquement).".to_string())?;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('.');
    let allowed = ALLOWED_EXTERNAL_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")));
    if !allowed {
        return Err("Lien refusé (domaine non autorisé).".to_string());
    }
    Ok(url)
}

// ---------------------------------------------------------------------------
// Avatars (proxied through the backend with disk cache)
// ---------------------------------------------------------------------------

/// Primary avatar provider (64px helm render for an undashed UUID —
/// minotar sert le skin courant, mc-heads un Steve périmé).
const AVATAR_PRIMARY_URL: &str = "https://minotar.net/helm/";
/// Fallback avatar provider (64px head render for an undashed UUID).
const AVATAR_FALLBACK_URL: &str = "https://mc-heads.net/avatar/";
/// Per-provider HTTP timeout (the WebView never fetches avatars itself).
const AVATAR_TIMEOUT_SECS: u64 = 10;
/// Disk-cache freshness: files older than this are refetched (a failed
/// refetch keeps serving the stale file instead of erroring).
const AVATAR_TTL_SECS: u64 = 24 * 3600;
/// Smallest plausible avatar PNG (anything smaller is an error page).
const AVATAR_MIN_BYTES: usize = 100;
/// Largest plausible avatar PNG (64px heads are a few KiB).
const AVATAR_MAX_BYTES: usize = 1024 * 1024;
/// PNG file magic (`89 50 4E 47`).
const PNG_MAGIC: [u8; 4] = [0x89, 0x50, 0x4E, 0x47];

/// Undashed lowercase UUID when `uuid` holds 32 hex chars, else `None`.
/// Pure (unit-tested): the single validation gate for [`get_avatar`].
pub fn avatar_cache_key(uuid: &str) -> Option<String> {
    let undashed = uuid.replace('-', "").to_lowercase();
    if undashed.len() == 32 && undashed.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(undashed)
    } else {
        None
    }
}

/// Disk cache path for an undashed UUID:
/// `<gameDir>/cache/avatars/<undashed>.png`.
pub fn avatar_cache_path(game_dir: &Path, undashed: &str) -> PathBuf {
    game_dir
        .join("cache")
        .join("avatars")
        .join(format!("{undashed}.png"))
}

/// `true` when `bytes` look like a real avatar PNG (size + magic).
fn is_valid_avatar_png(bytes: &[u8]) -> bool {
    (AVATAR_MIN_BYTES..=AVATAR_MAX_BYTES).contains(&bytes.len())
        && bytes.len() >= PNG_MAGIC.len()
        && bytes[..PNG_MAGIC.len()] == PNG_MAGIC
}

fn avatar_data_url(png: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// `true` when the cache file exists and is younger than
/// [`AVATAR_TTL_SECS`] (mtime-based). Missing/unreadable mtimes count as stale.
fn avatar_cache_fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|mtime| {
            mtime
                .elapsed()
                .map(|age| age.as_secs() < AVATAR_TTL_SECS)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Fetch an avatar PNG: primary provider first, fallback second. Returns
/// the bytes plus the serving provider name (`"minotar"` / `"mc-heads"`),
/// or `None` when neither yields a valid PNG (HTTP failure, wrong size, or
/// wrong magic — the frontend falls back to its glyph, never an error).
async fn fetch_avatar_bytes(
    client: &reqwest::Client,
    undashed: &str,
) -> Option<(Vec<u8>, &'static str)> {
    let targets = [
        (format!("{AVATAR_PRIMARY_URL}{undashed}/64.png"), "minotar"),
        (format!("{AVATAR_FALLBACK_URL}{undashed}/64"), "mc-heads"),
    ];
    for (url, provider) in &targets {
        let res = match client
            .get(url)
            .timeout(std::time::Duration::from_secs(AVATAR_TIMEOUT_SECS))
            .send()
            .await
        {
            Ok(res) => res,
            Err(_) => continue,
        };
        if !res.status().is_success() {
            continue;
        }
        let bytes = match res.bytes().await {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        if is_valid_avatar_png(&bytes) {
            return Some((bytes.to_vec(), *provider));
        }
    }
    None
}

/// Avatar PNG for `uuid` as a `data:` URL, served through the backend with
/// a `<gameDir>/cache/avatars/` disk cache (the WebView never fetches
/// remote avatar URLs itself). A fresh cache hit never touches the network;
/// a stale (> 24h) or missing entry refetches minotar (fallback: mc-heads)
/// and caches the PNG — a failed refetch keeps serving the stale file
/// instead of erroring. Errors are plain strings; the frontend falls back
/// to its glyph (no error toasts).
pub async fn get_avatar(state: &SharedState, uuid: String) -> Result<String, String> {
    let undashed = avatar_cache_key(&uuid).ok_or_else(|| "UUID invalide.".to_string())?;
    crate::log_verbose!("[launcher] avatar uuid={undashed} requête");
    let path = {
        let s = state.lock().await;
        avatar_cache_path(&s.game_dir(), &undashed)
    };
    let cached = std::fs::read(&path).ok().filter(|b| is_valid_avatar_png(b));
    if let Some(ref bytes) = cached {
        if avatar_cache_fresh(&path) {
            crate::log_verbose!(
                "[launcher] avatar uuid={undashed} cache=frais octets={} fournisseur=cache",
                bytes.len()
            );
            return Ok(avatar_data_url(bytes));
        }
    }
    match fetch_avatar_bytes(&http_client()?, &undashed).await {
        Some((bytes, provider)) => {
            crate::log_verbose!(
                "[launcher] avatar uuid={undashed} cache=renouvelé octets={} fournisseur={provider}",
                bytes.len()
            );
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, &bytes);
            Ok(avatar_data_url(&bytes))
        }
        None => {
            if let Some(bytes) = cached {
                crate::log_verbose!(
                    "[launcher] avatar uuid={undashed} cache=périmé-servi octets={} fournisseur=cache",
                    bytes.len()
                );
                return Ok(avatar_data_url(&bytes));
            }
            crate::log_verbose!(
                "[launcher] avatar uuid={undashed} cache=absent octets=0 fournisseur=aucun"
            );
            Err("Avatar indisponible.".to_string())
        }
    }
}

/// Open an `https://` URL in the OS default browser (always external,
/// never in the WebView): `xdg-open` / `open` / `cmd /C start`.
/// Only allowlisted hosts are accepted; everything else (including
/// `http:`, `file:` and `javascript:`) is rejected.
pub async fn open_external(url: String) -> Result<String, String> {
    let url = validate_external_url(&url)?;
    #[cfg(target_os = "windows")]
    let cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", &url]);
        c
    };
    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(&url);
        c
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(&url);
        c
    };
    spawn_detached(cmd).map_err(|e| format!("Ouverture impossible : {e}"))?;
    Ok(url)
}

// ---------------------------------------------------------------------------
// Settings / Java
// ---------------------------------------------------------------------------

/// Current settings (no disk access).
pub async fn get_settings(state: &SharedState) -> SettingsDto {
    state.lock().await.settings.clone()
}

/// Drain the one-shot boot notices queued by [`AppState::boot`] (currently
/// the experimental-gate migration line, if any). The frontend toasts each
/// line once; later calls return an empty vec.
pub async fn get_boot_notices(state: &SharedState) -> Vec<String> {
    std::mem::take(&mut state.lock().await.boot_notices)
}

/// Persist settings to `<gameDir>/launcher-settings.json`.
pub async fn save_settings(
    state: &SharedState,
    settings: SettingsDto,
) -> Result<SettingsDto, String> {
    let (game_dir, merged) = {
        let mut s = state.lock().await;
        // Le formulaire Réglages ne transporte pas le registre des comptes
        // (`collectSettings` ne connaît ni `accounts` ni `active_account`) :
        // reprendre les valeurs en mémoire, sinon chaque enregistrement
        // écrase le fichier avec un registre vide et déconnecte tout.
        let mut merged = settings.clone();
        merged.accounts = s.settings.accounts.clone();
        merged.active_account = s.settings.active_account.clone();
        s.settings = merged.clone();
        (s.game_dir(), merged)
    };
    save_settings_file(&settings_path(&game_dir), &merged)
        .map_err(|e| format!("Réglages : {e}"))?;
    Ok(merged)
}

/// French Console line per flavor-swap move (existing style: `Classique` /
/// `Expérimental` prefixes like the install log). Same order as `moved`.
pub fn flavor_swap_lines(moved: &[String], experimental: bool) -> Vec<String> {
    let stash = launcher_core::MODERN_STASH_DIR_NAME;
    moved
        .iter()
        .map(|n| {
            if experimental {
                format!("Expérimental : {n} restauré depuis {stash}.")
            } else {
                format!("Classique : {n} mis de côté dans {stash}.")
            }
        })
        .collect()
}

/// Flavor switch backend path (Accueil dropdown `setPackFlavor`): persist
/// both experimental flags, then stash or restore the modern stack on disk
/// immediately so `mods/` is boot-clean without waiting for the next
/// install (classic moves the stack to `modstack-experimental/`,
/// experimental moves it back — only still-missing files download later).
/// Idempotent: a repeat call moves nothing and returns empty lines.
///
/// While [`EXPERIMENTAL_GATED`](crate::state::EXPERIMENTAL_GATED) holds, a
/// request for the experimental branch is refused with a French error (the
/// default branch still switches freely).
pub async fn set_pack_flavor(
    state: &SharedState,
    experimental: bool,
) -> Result<PackFlavorSwapDto, String> {
    set_pack_flavor_gated(state, experimental, crate::state::EXPERIMENTAL_GATED).await
}

/// [`set_pack_flavor`] with an explicit gate position (the live path always
/// passes [`EXPERIMENTAL_GATED`](crate::state::EXPERIMENTAL_GATED); tests
/// cover both positions without touching the constant).
async fn set_pack_flavor_gated(
    state: &SharedState,
    experimental: bool,
    gated: bool,
) -> Result<PackFlavorSwapDto, String> {
    if let Some(refusal) = crate::state::experimental_gate_rejects(experimental, gated) {
        return Err(refusal);
    }
    // Écriture disque hors mutex : le verrou ne couvre que la mutation mémoire.
    let (game_dir, merged) = {
        let mut s = state.lock().await;
        // Same roster-preserving merge as `save_settings` (the flavor
        // payload carries no accounts).
        s.settings.experimental_runtime = experimental;
        s.settings.experimental_optimizations = experimental;
        (s.game_dir(), s.settings.clone())
    };
    save_settings_file(&settings_path(&game_dir), &merged)
        .map_err(|e| format!("Réglages : {e}"))?;
    let moved = launcher_core::prune_other_flavor(&game_dir, experimental);
    let lines = flavor_swap_lines(&moved, experimental);
    Ok(PackFlavorSwapDto {
        experimental,
        moved,
        lines,
    })
}

/// Probe the effective Java binary (`java -version`, never fails hard).
///
/// Kept for back-compat; prefers the provisioned `<gameDir>/java` runtime
/// (see [`resolve_java`]) and now delegates to [`check_java_status`].
pub async fn resolve_java_info(state: &SharedState) -> JavaInfoDto {
    check_java_status(state).await
}

/// Total physical RAM in MiB (memory slider ceiling; never fails hard —
/// falls back to the historical 12 GiB cap when unreadable).
pub async fn get_system_memory(_state: &SharedState) -> u64 {
    std::panic::catch_unwind(launcher_core::system_memory_mb).unwrap_or(12 * 1024)
}

/// Effective runtime readiness (`java -version`, never fails hard): the
/// custom `java_path` override wins when set, else the flavor's runtime —
/// GraalVM 21 on experimental, Temurin 8 on default (auto-managed).
pub async fn check_java_status(state: &SharedState) -> JavaInfoDto {
    // Sonde(s) `java -version` : le coût lent du démarrage (voir audit
    // `refreshAll` — le frontend diffère cet appel après le premier
    // affichage, l'onglet Java étant masqué au boot).
    // Verrou court : on ne garde que les entrées (chemins + saveur), les
    // sondes disque/processus (`resolve_modern_java`, `java8_status`,
    // `check_java` → spawn `java -version`) tournent hors mutex.
    let started = std::time::Instant::now();
    enum Probe {
        Override(PathBuf),
        Modern(PathBuf),
        Default(PathBuf),
    }
    let probe = {
        let s = state.lock().await;
        let trimmed = s.settings.java_path.trim().to_string();
        if !trimmed.is_empty() {
            Probe::Override(PathBuf::from(trimmed))
        } else if s.settings.experimental_runtime {
            Probe::Modern(s.game_dir())
        } else {
            Probe::Default(s.game_dir())
        }
    };
    let (path, check, warning) = match probe {
        Probe::Override(path) => {
            let check = check_java(&path);
            let warning = java_warning_for(&check);
            (path, check, warning)
        }
        Probe::Modern(game_dir) => match launcher_core::resolve_modern_java(&game_dir) {
            Some(bin) => {
                let check = check_java(&bin);
                let warning = if check.major == Some(launcher_core::MODERN_JAVA_MAJOR) {
                    None
                } else {
                    Some("GraalVM 21 invalide — réinstallez le runtime du modpack.".to_string())
                };
                (bin, check, warning)
            }
            None => {
                let bin = game_dir
                    .join(launcher_core::MODERN_JAVA_DIR)
                    .join("bin")
                    .join(launcher_core::java_exe_name(launcher_core::Os::current()));
                let check = check_java(&bin);
                (
                    bin,
                    check,
                    Some(
                        "GraalVM 21 non installé — cliquez Installer pour le télécharger."
                            .to_string(),
                    ),
                )
            }
        },
        Probe::Default(game_dir) => {
            let status = java8_status(&game_dir);
            let path = status.path;
            let check = check_java(&path);
            let warning = java_warning_for(&check);
            (path, check, warning)
        }
    };
    let out = JavaInfoDto {
        path: path.to_string_lossy().into_owned(),
        major: check.major,
        is_java8: check.is_java8,
        warning,
    };
    boot_timing_log("sonde java", started);
    out
}

/// Download the Temurin 8 JRE (~100MB) and install it into `<gameDir>/java`
/// (verified Java 8). Streams `java-progress` events; returns the fresh probe.
/// `cancel_java_download` aborts the run (checked per download chunk).
pub async fn download_java8(
    state: &SharedState,
    emit: Option<Arc<JavaProgressEmitter>>,
) -> Result<JavaInfoDto, String> {
    let (game_dir, cancel) = {
        let s = state.lock().await;
        s.java_cancel.store(false, Ordering::Relaxed);
        (s.game_dir(), s.java_cancel.clone())
    };
    // Sondage modale Java : même contrat que `download_graalvm21`.
    let emit = prepare_java_emit(state, emit).await;
    let progress: Option<Arc<JavaProgressCallback>> = emit.map(|emit| {
        Arc::new(move |downloaded: u64, total: u64| {
            emit(JavaProgressPayload::from_bytes(downloaded, total));
        }) as Arc<JavaProgressCallback>
    });
    let java_bin = provision_java8(&game_dir, progress, Some(cancel))
        .await
        .map_err(|e| match &e {
            launcher_core::CoreError::Cancelled(_) => e.to_string(),
            _ => format!("Installation de Java 8 : {e}"),
        })?;
    let check = check_java(&java_bin);
    Ok(JavaInfoDto {
        path: java_bin.to_string_lossy().into_owned(),
        major: check.major,
        is_java8: check.is_java8,
        warning: java_warning_for(&check),
    })
}

// ---------------------------------------------------------------------------
// Play
// ---------------------------------------------------------------------------

/// JOUER: mandatory login → ensure install → detached Forge 1.7.10 spawn.
/// Returns `LOGIN_REQUIRED` when logged out, the maintenance message when
/// the CDN enables maintenance.
///
/// `console` receives live game stdout/stderr lines when the `console_live`
/// setting is on (tailed from `<gameDir>/logs/launcher-console.log`, current
/// launch only); the crash-error tail always surfaces there when a sink is
/// wired, even with the setting off. Game output is always appended to the
/// log file. `outcome` (see [`play_with_outcome`]) receives the post-launch
/// verdict (`running` / `exited` / `crashed`) for the Journal card.
pub async fn play(
    state: &SharedState,
    emit: Option<Arc<ProgressEmitter>>,
    console: Option<Arc<ConsoleEmitter>>,
) -> Result<PlayResultDto, String> {
    play_with_outcome(state, emit, console, None).await
}

/// `play` avec verdict post-lancement : même lancement détaché, mais un
/// moniteur en tâche de fond suit le journal console du lancement courant
/// et pousse exactement un verdict `running` / `crashed` précoce puis,
/// à la sortie du jeu, `exited` (ou `crashed` tardif) sur `outcome`.
///
/// Le détaché est inchangé (`kill_on_drop(false)`, le jeu survit au
/// lanceur) ; seule l'observation est ajoutée (voir [`spawn_detached_game`]).
pub async fn play_with_outcome(
    state: &SharedState,
    emit: Option<Arc<ProgressEmitter>>,
    console: Option<Arc<ConsoleEmitter>>,
    outcome: Option<Arc<OutcomeEmitter>>,
) -> Result<PlayResultDto, String> {
    {
        let s = state.lock().await;
        s.ensure_logged_in()?;
        if let Some(msg) = s.maintenance_message() {
            return Err(msg);
        }
        if s.install_running {
            return Err("Installation en cours…".to_string());
        }
    }
    let manifest = ensure_manifest(state).await?;
    if manifest.maintenance.enabled {
        let msg = manifest.maintenance.message.trim();
        return Err(if msg.is_empty() {
            "Maintenance en cours. Le jeu est temporairement indisponible.".to_string()
        } else {
            msg.to_string()
        });
    }
    // Sondage console + verdict : anneaux vidés au démarrage du lancement,
    // chaque ligne/verdict suivant y est enregistré AVANT transmission
    // (événements inchangés, premier signal gagne). Enregistrement même sans
    // émetteur : le sondage frontend reste utile sans événements.
    let console = prepare_console_emit(state, console).await;
    let outcome = prepare_outcome_emit(state, outcome).await;
    // Ensure install when the cached snapshot differs from the CDN, when
    // the Mojang-side runtime is missing (vanilla jar, natives, Forge —
    // cheap disk checks, no network), or when planned CDN libraries are
    // missing on disk (wipe 16:41 : version « à jour » mais `libraries/`
    // vidé — le JOUER suivant re-télécharge, zéro étape manuelle).
    let game_dir = state.lock().await.game_dir();
    // Repair pass (unconditional, idempotent): stray PalaForge-era `.pala`
    // files migrate back to `.jar` without any re-download, so even a
    // "fresh" launch never boots invisible mods.
    let migrated = launcher_core::migrate_mods_to_jar(&game_dir);
    let experimental = state.lock().await.settings.experimental_runtime;
    let optimizations = state.lock().await.settings.experimental_optimizations;
    // Flavor swap (unconditional, idempotent self-heal): a stale mixed
    // `mods/` heals on JOUER even without an explicit switch — classic
    // stashes the modern stack out of the Java 8 Mixin/ASM chain,
    // experimental restores it from the stash (restored bytes read as fresh
    // by the checks below, so nothing re-downloads). Each move is logged
    // in French.
    let swapped = launcher_core::prune_other_flavor(&game_dir, experimental);
    let swap_lines = flavor_swap_lines(&swapped, experimental);
    let cached = load_manifest(&launcher_core::cdn_last_path(&game_dir))
        .ok()
        .flatten();
    let fresh = compare_manifests(cached.as_ref(), Some(&manifest));
    let cdn_stale = !matches!(fresh, UpdateStatus::Ok { .. });
    let mojang_stale = launcher_core::mojang_runtime_stale(&game_dir);
    // Garde-fou bibliothèques : un `libraries/` vidé (wipe 16:41) avec une
    // version « à jour » doit quand même re-télécharger les 304 libs au
    // JOUER suivant — présence seule, sans réseau.
    let cdn_libs_stale = cdn_plan_libs_missing(&manifest, &game_dir, experimental);
    // Garde-fou contenu (mods/configs/…) : présence seule, sans hachage —
    // un monde « version OK » aux fichiers de mods effacés doit se réparer
    // au JOUER suivant au lieu d'aller droit au spawn (PalaForgeUtils).
    let missing_content = missing_content_files(&manifest, &game_dir, experimental);
    let cdn_content_stale = !missing_content.is_empty();
    let modern_stale = experimental && launcher_core::modern_runtime_stale(&game_dir);
    let opt_stale =
        experimental && optimizations && launcher_core::modern_opt_mods_stale(&game_dir);
    // Flavor runtime (auto-managed): GraalVM 21 on experimental, Temurin 8
    // on default — provisioned below when missing (cheap disk probes).
    let java_stale = if experimental {
        launcher_core::resolve_modern_java(&game_dir).is_none()
    } else {
        !launcher_core::java8_status(&game_dir).is_java8
    };
    // Flavor switch with everything "fresh": the other branch's mods are
    // still on disk (Forge would load both stacks). Force the install
    // flow, which prunes + replaces them entirely.
    let flavor_stale = launcher_core::installed_flavor(&game_dir).as_deref()
        != Some(if experimental {
            "experimental"
        } else {
            "default"
        });
    if !migrated.is_empty() {
        if let Some(c) = &console {
            c(format!("Mods renommés en .jar : {}.", migrated.join(", ")));
        }
    }
    for line in &swap_lines {
        if let Some(c) = &console {
            c(line.clone());
        }
    }
    // Cale palaforge supprimée : le modid nu `palaforge` n'est requis par rien (`palaforge-utils` est la vraie dépendance, couverte par le manifeste).
    if cdn_stale
        || mojang_stale
        || cdn_libs_stale
        || cdn_content_stale
        || modern_stale
        || opt_stale
        || flavor_stale
        || java_stale
    {
        let already = state.lock().await.install_running;
        if already {
            return Err("Installation déjà en cours…".to_string());
        }
        let cancel = {
            let mut s = state.lock().await;
            s.install_running = true;
            s.install_cancel.store(false, Ordering::Relaxed);
            s.install_cancel.clone()
        };
        // Même modèle unifié que `start_install` : les totaux de TOUTES les
        // phases jouées sont sommés avant le premier octet, la barre JOUER
        // monte en continu pendant la mise à niveau avant lancement.
        // `cdn_libs_stale` seul suffit : le plan CDN re-télécharge les
        // bibliothèques manquantes même à version inchangée (wipe 16:41).
        // Le drapeau est libéré sur tous les chemins (bloc unique).
        let repair = play_repair_pipeline(
            state,
            emit.clone(),
            &manifest,
            &game_dir,
            experimental,
            cdn_stale || cdn_libs_stale,
            if cdn_stale || cdn_libs_stale {
                Vec::new()
            } else {
                missing_content
            },
            mojang_stale,
            java_stale,
            experimental && (modern_stale || opt_stale),
            cancel,
        )
        .await;
        state.lock().await.install_running = false;
        repair?;
        launcher_core::write_installed_flavor(&game_dir, experimental);
    } else {
        state.lock().await.manifest = Some(manifest.clone());
        // Tout est frais : aucun run ne démarre, le payload terminal du run
        // précédent est périmé — l'oublier pour que le sondage du lancement
        // reste silencieux (`None`, jamais de barre ressuscitée).
        clear_install_progress(state).await;
    }

    // Garde bytecode plan-time avant tout spawn (avertissements français,
    // jars nommés — warn-only, jamais d'échec : FML ignore les jars
    // illisibles et continue). Couvre aussi le cas « tout frais » sans
    // réinstall ; chaque avertissement part vers la console + les warnings.
    let bytecode_warnings = assert_classpath_bytecode(&manifest, &game_dir, experimental);
    if let Some(c) = &console {
        for w in &bytecode_warnings {
            c(w.clone());
        }
    }

    // Construction du lancement hors mutex : l'instantané est cloné sous
    // un verrou court, tout le travail lourd (classpath + balayage
    // bytecode des jars) tourne verrou relâché.
    let (argv, dir, mut warnings) = {
        // Prism-style freshness: relaunch the refresh chain when the
        // ACTIVE in-memory session is older than the Minecraft token
        // lifetime instead of launching dead. Dead refresh → re-login
        // prompt; anything else surfaces as a plain error (token kept).
        let stale = state
            .lock()
            .await
            .active_session()
            .map(session_is_stale)
            .unwrap_or(false);
        if stale {
            refresh_session(state).await?;
        }
        let (manifest, session, game_dir, settings) = state.lock().await.launch_snapshot()?;
        crate::state::AppState::launch_argv_from(&manifest, &session, &game_dir, &settings)?
    };
    warnings.extend(bytecode_warnings);
    let console_live = state.lock().await.settings.console_live;
    // Driver-level environment for the game child (Mesa threading on
    // Linux experimental; empty elsewhere — never touches our own env).
    let modern_env = launcher_core::modern_env_vars(
        launcher_core::Os::current(),
        state.lock().await.settings.experimental_runtime,
    );
    let log_path = dir.join("logs").join("launcher-console.log");
    let pid = spawn_detached_game(
        &argv,
        &dir,
        &log_path,
        console,
        console_live,
        outcome,
        &modern_env,
    )?;
    // PID enregistré pour le sondage `get_game_status` (secours quand les
    // événements `game-outcome` n'atteignent pas la WebView — le chemin
    // événement reste inchangé, premier signal gagne).
    state.lock().await.game_pid = Some(pid);
    Ok(PlayResultDto {
        command: launcher_core::quote_command(&argv),
        warnings,
    })
}

/// Lignes de console conservées par lancement (garde-fou mémoire : un mod
/// bavard ne fait jamais gonfler l'onglet Console sans limite).
pub const GAME_TAIL_MAX_LINES: usize = 2_000;
/// Largeur max d'une ligne console (`…` au-delà, comme avant).
pub const GAME_TAIL_LINE_CHARS: usize = 500;
/// Scrutation du journal console + du processus jeu.
pub const GAME_TAIL_POLL_MS: u64 = 500;
/// Délai de grâce : un jeu vivant passé ce délai est déclaré « en cours »
/// (le plantage FML 1.7.10 au boot survient en ~1 s).
pub const RUNNING_GRACE_SECS: u64 = 5;
/// Fenêtre du plantage précoce : une sortie DANS ce délai avec des marqueurs
/// dans `latest.log` vaut `crashed` ; au-delà, simple `exited` (le suivi en
/// direct a déjà tout montré, et un mod peut journaliser des
/// `Exception` sans mourir).
pub const EARLY_CRASH_WINDOW_SECS: u64 = 120;
/// Lignes d'erreur réelle renvoyées vers l'onglet Console en cas de plantage.
pub const CRASH_TAIL_LINES: usize = 30;
/// Fenêtre de lecture du delta `latest.log` (les logs FML font ~50 Kio).
pub const LATEST_SCAN_MAX_BYTES: usize = 512 * 1024;
/// Fragment console sans `\n` au-delà duquel on force l'affichage (un flux
/// sans retour à la ligne ne doit ni gonfler sans borne ni rester muet).
pub const CONSOLE_PENDING_MAX_BYTES: usize = 64 * 1024;
/// Longueur max de la cause dans le toast (le détail garde la ligne entière).
pub const CAUSE_TOAST_CHARS: usize = 160;

/// Verdict du moniteur post-lancement, poussé sur `outcome` (événement
/// `game-outcome` côté Tauri) : la carte Journal ne s'arrête jamais à la
/// ligne de commande de lancement.
pub type OutcomeEmitter = dyn Fn(crate::state::GameOutcomeDto) + Send + Sync;

/// Lancement détaché du jeu : stdout/stderr ajoutées au fichier
/// `log_path` (jamais de pipe — le suivi se fait par relecture du fichier),
/// puis moniteur en tâche de fond (suivi console du lancement courant +
/// détection de plantage précoce + verdict de sortie).
///
/// Le détaché est inchangé : `kill_on_drop(false)`, le jeu survit au
/// lanceur. L'observation repose sur le handle `Child` conservé par le
/// moniteur : le PID est connu dès le spawn même en détaché
/// (`Child::id`), et le sondage se fait par `try_wait` non bloquant — on ne
/// signale jamais le jeu, on ne fait que constater sa sortie (sans handle,
/// un sondage `/proc/<pid>` resterait possible, non nécessaire ici).
/// Une erreur de spawn remonte en `Err` (le frontend journalise « Échec du
/// lancement », qui vaut ligne de conclusion).
/// Detached-spawn configuration (pure, no process): argv + process working
/// directory + console log path.
///
/// `cwd` is ALWAYS the instance root: FS-Patches resolves `./`-relative
/// paths (its updater's `performSha1` uses `new File("./", …)`) against the
/// game process working directory — and the instance root is generally the
/// correct CWD for the game anyway. Build it with [`spawn_config_for`] so
/// the invariant is asserted in one place (unit-tested below).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnConfig {
    /// Executable argv (`[java, jvm…, -cp, cp, main, game…]`).
    pub argv: Vec<String>,
    /// Process working directory (instance root).
    pub cwd: PathBuf,
    /// Console log file (`logs/launcher-console.log`).
    pub log_path: PathBuf,
}

/// Build the detached-spawn configuration for `game_dir`: the CWD is the
/// instance root itself (never inherited, never `/`).
pub fn spawn_config_for(argv: &[String], game_dir: &Path, log_path: &Path) -> SpawnConfig {
    SpawnConfig {
        argv: argv.to_vec(),
        cwd: game_dir.to_path_buf(),
        log_path: log_path.to_path_buf(),
    }
}

pub fn spawn_detached_game(
    argv: &[String],
    dir: &Path,
    log_path: &Path,
    console: Option<Arc<ConsoleEmitter>>,
    live_tail: bool,
    outcome: Option<Arc<OutcomeEmitter>>,
    extra_env: &[(String, String)],
) -> Result<u32, String> {
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Taille du journal AVANT le spawn : seul ce qui suit appartient au
    // lancement courant (les lancements précédents restent dans le fichier
    // mais ne sont jamais rejoués dans l'onglet).
    let log_offset = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
    let latest_path = dir.join("logs").join("latest.log");
    let latest_offset = std::fs::metadata(&latest_path)
        .map(|m| m.len())
        .unwrap_or(0);
    // Configuration pure : le CWD du jeu est la racine d'instance (voir
    // [`SpawnConfig`] — FS-Patches résout `./` contre lui).
    // Diagnostic collecteur post-assemblage : jetons GC seuls, jamais la
    // commande complète — toute récidive « Multiple garbage collectors
    // selected » est attribuable d'un coup d'œil.
    {
        let tokens = crate::state::gc_spawn_tokens(argv);
        crate::log_verbose!(
            "[launcher] collecteurs GC au lancement : {}",
            if tokens.is_empty() {
                "aucun sélecteur".to_string()
            } else {
                tokens.join(" ")
            }
        );
    }
    let cfg = spawn_config_for(argv, dir, log_path);
    let mut cmd = tokio::process::Command::new(&cfg.argv[0]);
    cmd.args(&cfg.argv[1..])
        .envs(extra_env.iter().map(|(k, v)| (k, v)))
        .current_dir(&cfg.cwd)
        .stdin(Stdio::null())
        .kill_on_drop(false);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(out) => {
            cmd.stdout(out);
        }
        Err(_) => {
            cmd.stdout(Stdio::null());
        }
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(err) => {
            cmd.stderr(err);
        }
        Err(_) => {
            cmd.stderr(Stdio::null());
        }
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("Échec du lancement : {e}"))?;
    // PID connu dès le spawn même en détaché : il sert aux lignes de journal.
    let pid = child.id().unwrap_or(0);
    if console.is_none() && outcome.is_none() {
        // Sans puits : `child` est détruit sans attendre (`kill_on_drop(false)`
        // : le jeu survit) — comportement historique du chemin sans console.
        return Ok(pid);
    }
    tokio::spawn(watch_game(
        child,
        GameWatch {
            pid,
            log_path: log_path.to_path_buf(),
            log_offset,
            latest_path,
            latest_offset,
            live_tail: live_tail && console.is_some(),
            console,
            outcome,
            started: tokio::time::Instant::now(),
        },
    ));
    Ok(pid)
}

/// État du moniteur post-lancement (tâche détachée de [`spawn_detached_game`]).
struct GameWatch {
    /// PID connu au spawn (lignes de journal).
    pid: u32,
    /// Journal console suivi (`launcher-console.log`).
    log_path: PathBuf,
    /// Taille du journal au spawn (lancement courant uniquement).
    log_offset: u64,
    /// Journal FML scruté pour les marqueurs (`latest.log`).
    latest_path: PathBuf,
    /// Taille du `latest.log` au spawn (delta post-lancement uniquement).
    latest_offset: u64,
    /// Suivi en direct vers l'onglet Console (réglage `console_live`).
    live_tail: bool,
    /// Puits console (`console-line`) : suivi en direct + file d'erreur.
    console: Option<Arc<ConsoleEmitter>>,
    /// Puits verdict (`game-outcome`) : carte Journal + toast de plantage.
    outcome: Option<Arc<OutcomeEmitter>>,
    /// Instant du spawn (grâce `running`, fenêtre plantage précoce).
    started: tokio::time::Instant,
}

/// Moniteur post-lancement : relaie les nouvelles lignes console du lancement
/// courant, déclare `running` passé la grâce, puis à la sortie du jeu pousse
/// le verdict `crashed` (file d'erreur + cause) ou `exited` (code).
async fn watch_game(mut child: tokio::process::Child, mut watch: GameWatch) {
    let mut forwarded: usize = 0;
    let mut running_reported = false;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(GAME_TAIL_POLL_MS)).await;
        if watch.live_tail {
            let (lines, consumed) = read_new_console_lines(&watch.log_path, watch.log_offset).await;
            watch.log_offset += consumed;
            if let Some(console) = watch.console.as_ref() {
                for line in lines {
                    if forwarded >= GAME_TAIL_MAX_LINES {
                        break;
                    }
                    forwarded += 1;
                    console(line);
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // Drain final : rien du plantage ne doit manquer à l'onglet.
                if watch.live_tail {
                    let (lines, consumed) =
                        read_new_console_lines(&watch.log_path, watch.log_offset).await;
                    watch.log_offset += consumed;
                    if let Some(console) = watch.console.as_ref() {
                        for line in lines {
                            if forwarded >= GAME_TAIL_MAX_LINES {
                                break;
                            }
                            forwarded += 1;
                            console(line);
                        }
                    }
                }
                finish_game_exit(&watch, status.code()).await;
                break;
            }
            Ok(None) => {
                if !running_reported && watch.started.elapsed().as_secs() >= RUNNING_GRACE_SECS {
                    running_reported = true;
                    if let Some(outcome) = watch.outcome.as_ref() {
                        crate::log_verbose!(
                            "[launcher] game-outcome running émis (PID {}).",
                            watch.pid
                        );
                        outcome(decide_game_outcome(false, None, None, watch.pid));
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// Verdict de sortie : dans la fenêtre précoce, un marqueur dans le delta
/// `latest.log` vaut `crashed` (file d'erreur vers la Console + cause pour
/// le toast et le Journal) ; sinon `exited` avec le code. Le Journal reçoit
/// toujours sa ligne de conclusion.
async fn finish_game_exit(watch: &GameWatch, code: Option<i32>) {
    let elapsed = watch.started.elapsed().as_secs();
    let mut delta = String::new();
    let mut cause = None;
    if elapsed <= EARLY_CRASH_WINDOW_SECS {
        delta = read_log_delta(
            &watch.latest_path,
            watch.latest_offset,
            LATEST_SCAN_MAX_BYTES,
        )
        .await;
        cause = find_crash_cause(&delta);
    }
    if let Some(ref c) = cause {
        if let Some(console) = watch.console.as_ref() {
            for line in crash_tail_lines(&delta, CRASH_TAIL_LINES) {
                console(format_console_line(&line));
            }
        }
        if let Some(outcome) = watch.outcome.as_ref() {
            eprintln!(
                "[launcher] game-outcome crashed émis (PID {}, cause : {}).",
                watch.pid,
                short_cause(c)
            );
            outcome(decide_game_outcome(true, Some(c.clone()), code, watch.pid));
        }
        return;
    }
    if let Some(outcome) = watch.outcome.as_ref() {
        let code_s = code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());
        // Sortie nominale (`code 0`) : routine, derrière le gate. Toute
        // autre fin (code non nul ou inconnu) est anormale : toujours
        // imprimée, comme les plantages.
        if matches!(code, Some(0)) {
            crate::log_verbose!(
                "[launcher] game-outcome exited émis (PID {}, code {code_s}).",
                watch.pid
            );
        } else {
            eprintln!(
                "[launcher] game-outcome exited émis (PID {}, code {code_s}).",
                watch.pid
            );
        }
        outcome(decide_game_outcome(true, None, code, watch.pid));
    }
}

/// Lit le delta lisible du fichier `path` après `offset`, borné aux
/// `max_bytes` derniers octets (fichier remplacé entre-temps ⇒ reprise à 0).
async fn read_log_delta(path: &Path, offset: u64, max_bytes: usize) -> String {
    let Ok(content) = tokio::fs::read(path).await else {
        return String::new();
    };
    log_delta_after(&bytes_to_text(&content), offset, max_bytes).to_string()
}

/// Lit les octets ajoutés au journal console depuis `offset` et renvoie les
/// lignes complètes affichables plus les octets consommés (le fragment final
/// sans `\n` n'est pas consommé : relu au prochain passage).
async fn read_new_console_lines(path: &Path, offset: u64) -> (Vec<String>, u64) {
    let Ok(content) = tokio::fs::read(path).await else {
        return (Vec::new(), 0);
    };
    let len = content.len() as u64;
    // Fichier remplacé/tronqué depuis le spawn : on repart du début plutôt
    // que de rester muet (un `offset` au-delà de la fin ne consommerait plus
    // jamais rien).
    let start = if offset > len { 0 } else { offset } as usize;
    let (lines, consumed) = split_complete_lines(&content[start..]);
    (lines, consumed as u64)
}

/// Octets bruts → texte (UTF-8 tolérant : le jeu écrit parfois des fragments).
fn bytes_to_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// Delta lisible de `content` après `offset` (pur, testé) : reprise à 0 si le
/// fichier a été remplacé (`offset` au-delà de la fin), borné aux `max_bytes`
/// derniers octets, frontières UTF-8 respectées des deux côtés.
pub fn log_delta_after(content: &str, offset: u64, max_bytes: usize) -> &str {
    let bytes = content.as_bytes();
    let mut start = if (offset as usize) > bytes.len() {
        0
    } else {
        offset as usize
    };
    while start < bytes.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    let mut slice = &bytes[start..];
    if slice.len() > max_bytes {
        slice = &slice[slice.len() - max_bytes..];
    }
    let mut cut = 0;
    while cut < slice.len() && std::str::from_utf8(&slice[cut..]).is_err() {
        cut += 1;
    }
    std::str::from_utf8(&slice[cut..]).unwrap_or("")
}

/// Sépare `chunk` en lignes complètes affichables + octets consommés (pur,
/// testé). Seules les lignes terminées par `\n` sont renvoyées ; le fragment
/// final est conservé (non consommé). Un fragment sans `\n` au-delà de
/// [`CONSOLE_PENDING_MAX_BYTES`] est affiché de force (tronqué) pour ne ni
/// gonfler sans borne ni rester muet.
pub fn split_complete_lines(chunk: &[u8]) -> (Vec<String>, usize) {
    match chunk.iter().rposition(|b| *b == b'\n') {
        Some(pos) => {
            let complete = &chunk[..=pos];
            let lines = bytes_to_text(complete)
                .split('\n')
                .filter(|l| !l.is_empty())
                .map(format_console_line)
                .collect();
            (lines, complete.len())
        }
        None => {
            if chunk.len() > CONSOLE_PENDING_MAX_BYTES {
                (
                    vec![format_console_line(&bytes_to_text(chunk))],
                    chunk.len(),
                )
            } else {
                (Vec::new(), 0)
            }
        }
    }
}

/// Mise en forme d'une ligne console (pure, testée) : `\r` retirés, 500
/// caractères max (`…` au-delà, comptés en caractères pour l'UTF-8).
pub fn format_console_line(raw: &str) -> String {
    let mut line = raw.replace('\r', "");
    if line.chars().count() > GAME_TAIL_LINE_CHARS {
        line = line.chars().take(GAME_TAIL_LINE_CHARS).collect();
        line.push('…');
    }
    line
}

/// Marqueurs de liaison fatale : quand plusieurs lignes `Caused by:` se
/// succèdent, celles-ci (la vraie chaîne du plantage) gagnent sur un
/// `IllegalArgumentException` générique — souvent la simple enveloppe
/// bénigne de découverte ASM qui masquait la cause réelle dans le toast.
const LINKAGE_CAUSE_MARKERS: &[&str] = &[
    "NoClassDefFoundError",
    "ClassNotFoundException",
    "UnsatisfiedLinkError",
    "NoSuchMethodError",
    "VerifyError",
];

/// Cause du plantage dans un texte de log (pur, testé) : une ligne `Caused
/// by:` de liaison (voir [`LINKAGE_CAUSE_MARKERS`]) gagne sur la première
/// ligne `Caused by:` générique, qui gagne sur `Unable to launch`, qui gagne
/// sur toute autre ligne d'exception. `None` quand le texte est sain.
pub fn find_crash_cause(text: &str) -> Option<String> {
    let mut caused: Vec<String> = Vec::new();
    let mut unable = None;
    let mut exception = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("Caused by:") {
            caused.push(trimmed.to_string());
            continue;
        }
        if unable.is_none() && trimmed.contains("Unable to launch") {
            unable = Some(trimmed.to_string());
        }
        if exception.is_none()
            && (trimmed.contains("Exception") || trimmed.contains("Error:"))
            && !trimmed.contains("ExitTrappedException")
        {
            exception = Some(trimmed.to_string());
        }
    }
    if !caused.is_empty() {
        if let Some(link) = caused
            .iter()
            .find(|l| LINKAGE_CAUSE_MARKERS.iter().any(|m| l.contains(m)))
        {
            return Some(link.clone());
        }
        return Some(caused.remove(0));
    }
    unable.or(exception)
}

/// Dernières lignes non vides d'un log (file d'erreur réelle, pure, testée).
pub fn crash_tail_lines(text: &str, max_lines: usize) -> Vec<String> {
    text.lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(max_lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|s| s.to_string())
        .collect()
}

/// Cause courte pour le toast (pure, testée) : 160 caractères max.
pub fn short_cause(cause: &str) -> String {
    format_console_line_short(cause, CAUSE_TOAST_CHARS)
}

fn format_console_line_short(raw: &str, max_chars: usize) -> String {
    let line = raw.replace('\r', "");
    if line.chars().count() > max_chars {
        let mut short: String = line.chars().take(max_chars).collect();
        short.push('…');
        short
    } else {
        line
    }
}

/// Verdict post-lancement (pur, testé) : `gone` = processus déjà terminé
/// (vrai PID de spawn ou faux PID en test), `cause` = marqueur trouvé dans
/// le delta `latest.log` (fenêtre précoce, `None` hors fenêtre), `code` =
/// code de sortie. Détails en français pour la carte Journal.
///
/// Un code de sortie `0` (fermeture par la croix) vaut toujours `exited` :
/// un jeu vraiment planté ne sort quasiment jamais en 0, et le delta
/// contiendrait sinon des lignes `Exception` bénignes qui accuseraient un
/// plantage à tort (la file d'erreur reste visible dans la Console).
pub fn decide_game_outcome(
    gone: bool,
    cause: Option<String>,
    code: Option<i32>,
    pid: u32,
) -> crate::state::GameOutcomeDto {
    use crate::state::GameOutcomeDto;
    if !gone {
        return GameOutcomeDto {
            status: "running".to_string(),
            code: None,
            cause: None,
            detail: format!("Jeu en cours d'exécution (PID {pid})."),
        };
    }
    if code == Some(0) {
        return GameOutcomeDto {
            status: "exited".to_string(),
            code,
            cause: None,
            detail: "Jeu fermé (code 0).".to_string(),
        };
    }
    if let Some(c) = cause {
        return GameOutcomeDto {
            status: "crashed".to_string(),
            code,
            cause: Some(c.clone()),
            detail: format!("Plantage détecté (PID {pid}) : {c} — voir la Console du jeu."),
        };
    }
    let code_s = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    GameOutcomeDto {
        status: "exited".to_string(),
        code,
        cause: None,
        detail: format!("Jeu fermé (code {code_s})."),
    }
}

// ---------------------------------------------------------------------------
// État du jeu par sondage (secours `En jeu`, sans toucher aux événements)
// ---------------------------------------------------------------------------

/// `true` quand la ligne de commande `cmd` porte un marqueur du jeu (pur,
/// testé) : la classe principale `launchwrapper` OU un chemin sous
/// `game_dir` (classpath du lancement). Comparaison insensible à la casse
/// et aux séparateurs (`/` vs `\`) pour Windows. C'est la garde anti
/// recyclage de PID : un PID réutilisé par un processus étranger ne porte
/// jamais ces marqueurs.
pub fn cmdline_is_game(cmd: &[String], game_dir: &Path) -> bool {
    let dir = game_dir.to_string_lossy().replace('\\', "/").to_lowercase();
    cmd.iter().any(|arg| {
        let a = arg.replace('\\', "/").to_lowercase();
        a.contains("launchwrapper") || (!dir.is_empty() && a.contains(dir.as_str()))
    })
}

/// `true` quand le PID enregistré est vivant ET porte un marqueur du jeu
/// (voir [`cmdline_is_game`]). Jamais d'erreur : PID absent/nul, processus
/// inconnu ou ligne sans marqueur valent `false` (état inconnu → jeu
/// considéré fermé, le chemin événement reste premier signal).
///
/// Rafraîchissement singulier d'abord (ciblé, sans balayage complet à chaque
/// sondage 3s), repli au balayage complet quand le singulier ne voit rien.
/// Les deux rafraîchissements demandent explicitement la ligne de commande :
/// les sucres `refresh_process`/`refresh_processes` de sysinfo 0.30 ne la
/// chargent jamais (`cmd = Never` → `cmd()` vide → marqueur toujours refusé,
/// le sondage retombait à `false` ~0,5 s après le lancement). La garde
/// marqueur s'applique dans tous les cas (anti recyclage de PID).
pub fn game_process_running(pid: Option<u32>, game_dir: &Path) -> bool {
    let pid = match pid {
        Some(p) if p != 0 => p,
        _ => return false,
    };
    let sys_pid = sysinfo::Pid::from_u32(pid);
    // Mêmes champs que les sucres par défaut, plus la ligne de commande.
    let genre = sysinfo::ProcessRefreshKind::new()
        .with_memory()
        .with_cpu()
        .with_disk_usage()
        .with_exe(sysinfo::UpdateKind::OnlyIfNotSet)
        .with_cmd(sysinfo::UpdateKind::Always);
    let mut sys = sysinfo::System::new();
    if !sys.refresh_process_specifics(sys_pid, genre) || sys.process(sys_pid).is_none() {
        sys.refresh_processes_specifics(genre);
    }
    match sys.process(sys_pid) {
        Some(proc_) => cmdline_is_game(proc_.cmd(), game_dir),
        None => false,
    }
}

/// État du jeu pour le sondage frontend (`get_game_status`, toutes les 3s) :
/// le PID enregistré au spawn est-il vivant avec sa ligne de commande jeu ?
/// Ne fait jamais échouer l'appel (inconnu → `{ running: false }`,
/// synchronisation d'état silencieuse, aucun toast côté frontend).
///
/// Journalisation à la transition uniquement (permanente, minuscule) : le
/// journal du lanceur prouve `false→true`/`true→false` + PID, silencieux
/// sinon (les sondages stables à 3s ne doivent jamais spammer le journal).
pub async fn get_game_status(state: &SharedState) -> crate::state::GameStatusDto {
    let (pid, game_dir) = {
        let s = state.lock().await;
        (s.game_pid, s.game_dir())
    };
    let running = game_process_running(pid, &game_dir);
    {
        let mut s = state.lock().await;
        if running != s.last_game_running {
            s.last_game_running = running;
            let pid_s = pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "inconnu".to_string());
            if running {
                eprintln!("[launcher] état jeu : En jeu (PID {pid_s}).");
            } else {
                eprintln!("[launcher] état jeu : jeu fermé (PID {pid_s}).");
            }
        }
    }
    crate::state::GameStatusDto { running }
}

// ---------------------------------------------------------------------------
// Progression par sondage (secours barres, sans toucher aux événements)
// ---------------------------------------------------------------------------

/// Prépare l'émission d'un run d'installation : vide le magasin sondable
/// (`get_install_progress` → `None` tant que le run n'a rien émis) puis
/// renvoie un émetteur qui enregistre chaque payload dans l'état partagé
/// AVANT de le transmettre (le chemin événement est inchangé). Le magasin
/// est alimenté même quand l'émetteur enveloppé est `None` (certains chemins
/// de réparation JOUER n'ont pas d'émetteur — le sondage 500 ms doit quand
/// même voir la progression) : `None` donne un enregistreur seul, sans
/// transmission.
pub async fn prepare_install_emit(
    state: &SharedState,
    emit: Option<Arc<ProgressEmitter>>,
) -> Option<Arc<ProgressEmitter>> {
    let store = state.lock().await.last_install_progress.clone();
    store
        .lock()
        .expect("magasin install-progress empoisonné")
        .take();
    Some(Arc::new(move |p: ProgressPayload| {
        *store.lock().expect("magasin install-progress empoisonné") = Some(p.clone());
        if let Some(inner) = &emit {
            inner(p);
        }
    }) as Arc<ProgressEmitter>)
}

/// Même contrat que [`prepare_install_emit`] pour les téléchargements Java
/// (`java-progress` → `get_java_progress`). La plomberie diffère (type
/// d'émetteur dédié, callbacks `(downloaded, total)` converties au vol),
/// mais l'enveloppe est identique : enregistrement même sans émetteur.
pub async fn prepare_java_emit(
    state: &SharedState,
    emit: Option<Arc<JavaProgressEmitter>>,
) -> Option<Arc<JavaProgressEmitter>> {
    let store = state.lock().await.last_java_progress.clone();
    store
        .lock()
        .expect("magasin java-progress empoisonné")
        .take();
    Some(Arc::new(move |p: JavaProgressPayload| {
        *store.lock().expect("magasin java-progress empoisonné") = Some(p.clone());
        if let Some(inner) = &emit {
            inner(p);
        }
    }) as Arc<JavaProgressEmitter>)
}

/// Oublie le dernier `install-progress` (JOUER sans mise à niveau : le
/// payload terminal du run précédent est périmé et ne doit pas ressusciter
/// dans la barre pendant le lancement).
pub async fn clear_install_progress(state: &SharedState) {
    state
        .lock()
        .await
        .last_install_progress
        .lock()
        .expect("magasin install-progress empoisonné")
        .take();
}

/// Dernier `install-progress` émis (`None` quand aucun run n'a encore émis).
/// Ne fait jamais échouer l'appel (sondage silencieux du frontend).
pub async fn get_install_progress(state: &SharedState) -> Option<ProgressPayload> {
    state
        .lock()
        .await
        .last_install_progress
        .lock()
        .expect("magasin install-progress empoisonné")
        .clone()
}

/// Dernier `java-progress` émis (`None` quand aucun téléchargement n'a
/// encore émis). Ne fait jamais échouer l'appel.
pub async fn get_java_progress(state: &SharedState) -> Option<JavaProgressPayload> {
    state
        .lock()
        .await
        .last_java_progress
        .lock()
        .expect("magasin java-progress empoisonné")
        .clone()
}

// ---------------------------------------------------------------------------
// Console et verdict par sondage (secours sans événements)
// ---------------------------------------------------------------------------

/// Lignes console conservées pour le sondage (anneau borné).
pub const CONSOLE_TAIL_CAP: usize = 500;

/// Prépare l'émission console d'un lancement : vide l'anneau (le run
/// précédent ne doit pas resservir — le curseur monotone `console_seq`
/// reste valide) puis renvoie un émetteur qui enregistre chaque ligne
/// AVANT de la transmettre (chemin événement inchangé). Enregistrement même
/// sans émetteur (sondage 1 s de l'onglet Console).
pub async fn prepare_console_emit(
    state: &SharedState,
    console: Option<Arc<ConsoleEmitter>>,
) -> Option<Arc<ConsoleEmitter>> {
    let (lines, seq) = {
        let s = state.lock().await;
        (s.console_lines.clone(), s.console_seq.clone())
    };
    lines.lock().expect("anneau console empoisonné").clear();
    // `seq` est volontairement monotone (jamais remis à zéro) : un curseur
    // frontend périmé reste valide après un nouveau lancement.
    let _ = seq;
    Some(Arc::new(move |line: String| {
        {
            let mut guard = lines.lock().expect("anneau console empoisonné");
            guard.push_back(line.clone());
            while guard.len() > CONSOLE_TAIL_CAP {
                guard.pop_front();
            }
        }
        seq.fetch_add(1, Ordering::Relaxed);
        if let Some(inner) = &console {
            inner(line);
        }
    }) as Arc<ConsoleEmitter>)
}

/// File console sondable (`get_console_tail`) : lignes après `since`
/// (curseur vu), `next` = nouveau curseur. Ne fait jamais échouer l'appel.
/// Un `since` périmé (au-delà du retenu, anneau borné) est recalé au plus
/// ancien retenu ; un `since` futur (vidage entre deux sondages) ressert
/// tout le retenu au lieu de rester muet.
pub async fn get_console_tail(state: &SharedState, since: usize) -> ConsoleTailDto {
    let (lines, seq) = {
        let s = state.lock().await;
        (s.console_lines.clone(), s.console_seq.clone())
    };
    let total = seq.load(Ordering::Relaxed);
    let guard = lines.lock().expect("anneau console empoisonné");
    let stored = guard.len();
    let retained_start = total.saturating_sub(stored);
    if since == total {
        return ConsoleTailDto {
            lines: Vec::new(),
            next: total,
        };
    }
    // `since` futur (curseur périmé d'avant un vidage) : tout le retenu
    // ressert au lieu de rester muet.
    let offset = if since > total {
        0
    } else {
        since.max(retained_start).saturating_sub(retained_start)
    };
    ConsoleTailDto {
        lines: guard.iter().skip(offset).cloned().collect(),
        next: total,
    }
}

/// Prépare l'émission du verdict d'un lancement : oublie le verdict du run
/// précédent puis renvoie un émetteur qui enregistre chaque verdict AVANT de
/// le transmettre (chemin événement inchangé). Enregistrement même sans
/// émetteur (sondage 3 s du statut jeu).
pub async fn prepare_outcome_emit(
    state: &SharedState,
    outcome: Option<Arc<OutcomeEmitter>>,
) -> Option<Arc<OutcomeEmitter>> {
    let store = state.lock().await.last_outcome.clone();
    store.lock().expect("magasin verdict empoisonné").take();
    Some(Arc::new(move |o: GameOutcomeDto| {
        *store.lock().expect("magasin verdict empoisonné") = Some(o.clone());
        if let Some(inner) = &outcome {
            inner(o);
        }
    }) as Arc<OutcomeEmitter>)
}

/// Dernier verdict `game-outcome` (`None` quand aucun lancement n'a encore
/// produit de verdict). Ne fait jamais échouer l'appel.
pub async fn get_last_outcome(state: &SharedState) -> Option<GameOutcomeDto> {
    state
        .lock()
        .await
        .last_outcome
        .lock()
        .expect("magasin verdict empoisonné")
        .clone()
}

// ---------------------------------------------------------------------------
// Tests (headless backend, no WebView, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::login_required_error;
    use launcher_auth::MinecraftSession;
    use launcher_core::parse_manifest;

    const JSON: &str = include_str!("../../core/tests/fixtures/minimal_manifest.json");

    fn manifest() -> Manifest {
        parse_manifest(JSON).unwrap()
    }

    fn state_with_tmp() -> (SharedState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut inner = AppState::default();
        inner.settings.game_dir = dir.path().to_string_lossy().into_owned();
        (Arc::new(Mutex::new(inner)), dir)
    }

    fn session() -> MinecraftSession {
        MinecraftSession {
            uuid: "123e4567-e89b-12d3-a456-426614174000".to_string(),
            name: "Steve".to_string(),
            mc_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            obtained: std::time::Instant::now(),
        }
    }

    #[test]
    fn session_staleness_threshold() {
        let fresh = session();
        assert!(!session_is_stale(&fresh));
        let mut old = session();
        old.obtained = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(
                crate::state::SESSION_MAX_AGE_SECS + 60,
            ))
            .unwrap();
        assert!(session_is_stale(&old));
    }

    #[test]
    fn restore_candidate_prefers_active_roster_entry() {
        use crate::state::AccountRef;
        let roster = vec![
            AccountRef {
                uuid: "uuid-a".to_string(),
                name: "Alice".to_string(),
            },
            AccountRef {
                uuid: "uuid-b".to_string(),
                name: "Bob".to_string(),
            },
        ];
        assert_eq!(restore_candidate(&[], None), None);
        assert_eq!(
            restore_candidate(&roster, Some("uuid-b")),
            Some("uuid-b".to_string())
        );
        // Active pointer pointing nowhere falls back to the first entry.
        assert_eq!(
            restore_candidate(&roster, Some("uuid-zz")),
            Some("uuid-a".to_string())
        );
        assert_eq!(restore_candidate(&roster, None), Some("uuid-a".to_string()));
    }

    #[tokio::test]
    async fn session_starts_null_and_logout_clears() {
        let (state, _dir) = state_with_tmp();
        assert!(get_session(&state).await.is_none());
        state.lock().await.upsert_session(session());
        assert_eq!(get_session(&state).await.unwrap().name, "Steve");
        logout(&state).await.unwrap();
        assert!(get_session(&state).await.is_none());
    }

    fn session_named(uuid: &str, name: &str) -> MinecraftSession {
        let mut s = session();
        s.uuid = uuid.to_string();
        s.name = name.to_string();
        s
    }

    #[tokio::test]
    async fn multi_account_upsert_switch_remove() {
        let (state, _dir) = state_with_tmp();
        // First login activates itself.
        state
            .lock()
            .await
            .upsert_session(session_named("uuid-a", "Alice"));
        assert_eq!(get_session(&state).await.unwrap().name, "Alice");
        // A second stored session does NOT steal the active pointer
        // (upsert is also the refresh path); activation is explicit.
        state
            .lock()
            .await
            .upsert_session(session_named("uuid-b", "Bob"));
        assert_eq!(get_session(&state).await.unwrap().name, "Alice");
        // Switch explicitly; unknown UUIDs are refused without touching state.
        assert!(state.lock().await.set_active_account("uuid-b"));
        assert_eq!(get_session(&state).await.unwrap().name, "Bob");
        assert!(!state.lock().await.set_active_account("uuid-zz"));
        assert_eq!(get_session(&state).await.unwrap().name, "Bob");
        // Removing the active account falls back to the survivor.
        assert!(remove_account(&state, "uuid-b".to_string()).await);
        assert_eq!(get_session(&state).await.unwrap().name, "Alice");
        // Removing unknown UUIDs is a silent no-op.
        assert!(!remove_account(&state, "uuid-zz".to_string()).await);
        // Removing the last one logs out.
        assert!(remove_account(&state, "uuid-a".to_string()).await);
        assert!(get_session(&state).await.is_none());
    }

    #[tokio::test]
    async fn accounts_list_flags_active() {
        let (state, _dir) = state_with_tmp();
        assert!(get_accounts(&state).await.is_empty());
        state
            .lock()
            .await
            .upsert_session(session_named("uuid-a", "Alice"));
        state
            .lock()
            .await
            .upsert_session(session_named("uuid-b", "Bob"));
        // First account stays active until explicitly switched.
        let list = get_accounts(&state).await;
        assert_eq!(list.len(), 2);
        assert!(list.iter().find(|a| a.uuid == "uuid-a").unwrap().active);
        assert!(!list.iter().find(|a| a.uuid == "uuid-b").unwrap().active);
        assert!(state.lock().await.set_active_account("uuid-b"));
        let list = get_accounts(&state).await;
        assert!(list.iter().find(|a| a.uuid == "uuid-b").unwrap().active);
    }

    #[tokio::test]
    async fn refresh_unknown_account_errors_without_touching_state() {
        // Hermetic: a dummy client id passes the config guard, then the
        // unknown UUID has no keyring entry, so the lookup fails before
        // any network is issued.
        let (state, _dir) = state_with_tmp();
        {
            let mut s = state.lock().await;
            s.settings.client_id = "test-client-id".to_string();
            s.upsert_session(session_named("uuid-a", "Alice"));
        }
        let before = get_accounts(&state).await;
        let err = refresh_account(&state, "uuid-zz".to_string())
            .await
            .unwrap_err();
        assert!(
            err.contains("Compte inconnu") || err.contains("Stockage sécurisé"),
            "unexpected: {err}"
        );
        assert_eq!(get_accounts(&state).await, before);
        assert_eq!(get_session(&state).await.unwrap().name, "Alice");
    }

    #[tokio::test]
    async fn cancel_commands_set_flags() {
        let (state, _dir) = state_with_tmp();
        cancel_install(&state).await.unwrap();
        assert!(state.lock().await.install_cancel.load(Ordering::Relaxed));
        cancel_java_download(&state).await.unwrap();
        assert!(state.lock().await.java_cancel.load(Ordering::Relaxed));
    }

    fn pending_device(code: &str) -> launcher_auth::DeviceCodeResponse {
        launcher_auth::DeviceCodeResponse {
            user_code: "ABCD-EFGH".to_string(),
            device_code: code.to_string(),
            verification_uri: "https://www.microsoft.com/link".to_string(),
            verification_uri_complete: None,
            expires_in: 900,
            interval: 5,
            message: "message".to_string(),
        }
    }

    #[tokio::test]
    async fn cancel_login_clears_pending_without_error() {
        let (state, _dir) = state_with_tmp();
        state.lock().await.pending_login = Some(pending_device("device-1"));
        cancel_login(&state).await.unwrap();
        assert!(state.lock().await.pending_login.is_none());
        // Idempotent sans connexion en cours.
        cancel_login(&state).await.unwrap();
    }

    #[test]
    fn login_cancel_guard_rejects_cleared_and_replaced() {
        // Demande effacée → annulée.
        let err = ensure_login_not_cancelled(None, "device-1").unwrap_err();
        assert!(err.contains("annulée"), "unexpected: {err}");
        // Demande remplacée (nouveau `request_login`) → annulée.
        let other = pending_device("device-2");
        let err = ensure_login_not_cancelled(Some(&other), "device-1").unwrap_err();
        assert!(err.contains("annulée"), "unexpected: {err}");
        // Même demande → pas d'erreur.
        let same = pending_device("device-1");
        ensure_login_not_cancelled(Some(&same), "device-1").unwrap();
    }

    #[tokio::test]
    async fn login_cancel_watcher_resolves_on_clear() {
        // Hermétique : la garde ne touche jamais le réseau.
        let (state, _dir) = state_with_tmp();
        state.lock().await.pending_login = Some(pending_device("device-1"));
        cancel_login(&state).await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_login_cancel(state.clone(), "device-1".to_string()),
        )
        .await
        .expect("la garde doit résoudre après annulation");
    }

    #[tokio::test]
    async fn pre_cancelled_install_aborts_without_network() {
        // Flag set before the run: every task bails (or the drain aborts)
        // before any CDN request is issued.
        let dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let err = {
            let run = std::sync::Arc::new(InstallRun::new(None));
            run_install(&manifest(), dir.path(), &run, Some(cancel), false)
                .await
                .unwrap_err()
        };
        assert!(err.contains("annulée"), "unexpected: {err}");
    }

    #[tokio::test]
    async fn mojang_verify_items_need_cache() {
        let (state, dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        // No Mojang cache → only the pinned Forge 1614 universal (no network
        // needed for its plan) joins the CDN verify surface.
        let bare = mojang_verify_items(dir.path(), &manifest());
        assert_eq!(bare.len(), 1);
        assert!(bare[0]
            .0
            .ends_with("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"));
        // Seed a minimal cached Mojang version: vanilla jar + one lib.
        let cache_dir = launcher_core::caches_dir(dir.path());
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join(launcher_core::MOJANG_CACHE_FILE),
            serde_json::json!({
                "id": "1.7.10",
                "libraries": [
                    {"name": "com.example:onlymojang:1.0",
                     "downloads": {"artifact": {"path": "com/example/onlymojang/1.0/onlymojang-1.0.jar",
                         "sha1": "aa", "size": 3, "url": "https://example.com/o.jar"}}}
                ],
                "downloads": {"client": {"sha1": "bb", "size": 5, "url": "https://example.com/c.jar"}},
                "assetIndex": {"id": "1.7.10", "sha1": "cc", "size": 7, "url": "https://example.com/a.json"}
            })
            .to_string(),
        )
        .unwrap();
        let items = mojang_verify_items(dir.path(), &manifest());
        // Forge 1614 + vanilla jar + the Mojang-only lib (fixture CDN has neither).
        assert_eq!(items.len(), 3);
        assert!(items
            .iter()
            .any(|(p, _, _)| p.ends_with("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar")));
        assert!(items
            .iter()
            .any(|(p, _, _)| p.ends_with("versions/1.7.10/1.7.10.jar")));
        assert!(items
            .iter()
            .any(|(p, _, _)| p.ends_with("onlymojang-1.0.jar")));
    }

    #[tokio::test]
    async fn play_and_install_require_login() {
        let (state, _dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        assert_eq!(
            play(&state, None, None).await.unwrap_err(),
            login_required_error()
        );
        // `include_loki` defaults to `false` for back-compat (explicit here).
        assert_eq!(
            start_install(&state, None, false).await.unwrap_err(),
            login_required_error()
        );
        assert_eq!(
            start_install(&state, None, true).await.unwrap_err(),
            login_required_error()
        );
    }

    #[tokio::test]
    async fn play_blocked_by_maintenance_before_any_install() {
        let (state, _dir) = state_with_tmp();
        let mut m = manifest();
        m.maintenance.enabled = true;
        m.maintenance.message = "MAJ en cours".to_string();
        {
            let mut s = state.lock().await;
            s.upsert_session(session());
            s.manifest = Some(m);
        }
        assert_eq!(play(&state, None, None).await.unwrap_err(), "MAJ en cours");
    }

    #[tokio::test]
    async fn verify_counts_everything_stale_on_empty_dir() {
        let (state, _dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        let report = verify_files(&state).await.unwrap();
        let planned = plan_downloads_for_current_os(&manifest(), &PathBuf::from("/none"));
        // Plan CDN + universel Forge 1614 épinglé (hors cache Mojang ici).
        assert_eq!(report.total, planned.len() + 1);
        assert_eq!(report.stale, report.total);
        assert!(report.stale_bytes > 0);
    }

    #[tokio::test]
    async fn save_settings_persists_to_game_dir() {
        let (state, dir) = state_with_tmp();
        let s = SettingsDto {
            memory_max: "6G".to_string(),
            game_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let back = save_settings(&state, s.clone()).await.unwrap();
        assert_eq!(back, s);
        // Effective dir carries the `paladium` tripwire subdir.
        let game_dir = state.lock().await.game_dir();
        assert!(settings_path(&game_dir).is_file());
        assert_eq!(get_settings(&state).await, s);
    }

    #[tokio::test]
    async fn java_status_matches_legacy_probe() {
        let (state, _dir) = state_with_tmp();
        let status = check_java_status(&state).await;
        let legacy = resolve_java_info(&state).await;
        assert_eq!(status, legacy);
        assert!(!status.path.is_empty());
        assert_eq!(status.is_java8, status.major == Some(8));
    }

    #[test]
    fn java_progress_payload_math() {
        let p = JavaProgressPayload::from_bytes(50, 200);
        assert!((p.fraction - 0.25).abs() < f32::EPSILON);
        assert!(p.detail.contains("25%"), "unexpected: {}", p.detail);
        let unknown = JavaProgressPayload::from_bytes(512, 0);
        assert_eq!(unknown.fraction, 0.0);
        assert!(
            unknown.detail.contains("512 o"),
            "unexpected: {}",
            unknown.detail
        );
    }

    #[tokio::test]
    async fn pollable_install_progress_none_then_some_and_cleared_on_new_run() {
        use crate::state::ProgressPayload;
        let (state, _dir) = state_with_tmp();
        // Aucun run n'a jamais émis : le sondage répond `None`.
        assert!(get_install_progress(&state).await.is_none());
        let (emit, seen) = recording_emit();
        let tracked = prepare_install_emit(&state, emit).await;
        // Démarrage du run : magasin vidé, toujours `None` avant la
        // première émission.
        assert!(get_install_progress(&state).await.is_none());
        let p = ProgressPayload {
            completed_files: 1,
            total_files: 4,
            completed_bytes: 100,
            total_bytes: 400,
            current_file: "mods/a.jar".to_string(),
            fraction: 0.25,
            detail: "Fichier 1/4 — mods/a.jar — 100/400 o (25 %)".to_string(),
        };
        (tracked.expect("enveloppe présente"))(p.clone());
        // Enregistré pour le sondage ET transmis sur le chemin événement.
        assert_eq!(get_install_progress(&state).await, Some(p));
        assert_eq!(seen.lock().unwrap().len(), 1);
        // Nouveau run : le terminal du précédent est oublié.
        let _ = prepare_install_emit(&state, None).await;
        assert!(get_install_progress(&state).await.is_none());
    }

    #[tokio::test]
    async fn pollable_java_progress_none_then_some_and_cleared_on_new_run() {
        let (state, _dir) = state_with_tmp();
        assert!(get_java_progress(&state).await.is_none());
        let seen: Arc<std::sync::Mutex<Vec<JavaProgressPayload>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe = seen.clone();
        let emit: Option<Arc<JavaProgressEmitter>> =
            Some(Arc::new(move |p: JavaProgressPayload| {
                probe.lock().unwrap().push(p);
            }));
        let tracked = prepare_java_emit(&state, emit).await;
        assert!(get_java_progress(&state).await.is_none());
        let p = JavaProgressPayload::from_bytes(50, 200);
        (tracked.expect("enveloppe présente"))(p.clone());
        assert_eq!(get_java_progress(&state).await, Some(p));
        assert_eq!(seen.lock().unwrap().len(), 1);
        let _ = prepare_java_emit(&state, None).await;
        assert!(get_java_progress(&state).await.is_none());
    }

    #[tokio::test]
    async fn pollable_install_progress_forwards_every_emit_without_reordering() {
        use crate::state::ProgressPayload;
        let (state, _dir) = state_with_tmp();
        let (emit, seen) = recording_emit();
        let tracked = prepare_install_emit(&state, emit)
            .await
            .expect("enveloppe présente");
        for i in 1..=3usize {
            tracked(ProgressPayload {
                completed_files: i,
                total_files: 3,
                completed_bytes: (i * 100) as u64,
                total_bytes: 300,
                current_file: format!("mods/f{i}.jar"),
                fraction: i as f32 / 3.0,
                detail: format!("Fichier {i}/3"),
            });
        }
        // Le magasin garde la DERNIÈRE émission (le sondage rejoue le
        // dernier état, jamais l'historique) ; l'événement a tout vu.
        let last = get_install_progress(&state)
            .await
            .expect("payload sondable");
        assert_eq!(last.completed_files, 3);
        assert_eq!(last.current_file, "mods/f3.jar");
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn install_emit_records_without_inner_emitter() {
        // Chemins sans émetteur (réparation JOUER partielle) : le magasin
        // sondable est quand même alimenté à chaque appel.
        use crate::state::ProgressPayload;
        let (state, _dir) = state_with_tmp();
        let tracked = prepare_install_emit(&state, None)
            .await
            .expect("enregistreur seul");
        let p = ProgressPayload {
            completed_files: 2,
            total_files: 4,
            completed_bytes: 200,
            total_bytes: 400,
            current_file: "mods/b.jar".to_string(),
            fraction: 0.5,
            detail: "Fichier 2/4 — mods/b.jar — 200/400 o (50 %)".to_string(),
        };
        tracked(p.clone());
        assert_eq!(get_install_progress(&state).await, Some(p));
    }

    #[tokio::test]
    async fn java_emit_records_without_inner_emitter() {
        // Même contrat côté Java : sans émetteur, le sondage voit quand même.
        let (state, _dir) = state_with_tmp();
        let tracked = prepare_java_emit(&state, None)
            .await
            .expect("enregistreur seul");
        let p = JavaProgressPayload::from_bytes(60, 200);
        tracked(p.clone());
        assert_eq!(get_java_progress(&state).await, Some(p));
    }

    #[tokio::test]
    async fn console_tail_ring_bounded_and_cursor() {
        // Anneau borné à 500 lignes, curseur monotone, sémantique `since`.
        let (state, _dir) = state_with_tmp();
        let tracked = prepare_console_emit(&state, None)
            .await
            .expect("enregistreur seul");
        for i in 0..600usize {
            tracked(format!("ligne {i}"));
        }
        let full = get_console_tail(&state, 0).await;
        assert_eq!(full.lines.len(), CONSOLE_TAIL_CAP);
        assert_eq!(full.lines[0], "ligne 100");
        assert_eq!(full.lines[CONSOLE_TAIL_CAP - 1], "ligne 599");
        assert_eq!(full.next, 600);
        // Curseur à jour : rien de neuf.
        let empty = get_console_tail(&state, 600).await;
        assert!(empty.lines.is_empty());
        assert_eq!(empty.next, 600);
        // Curseur partiel : seules les postérieures resservent.
        let tail = get_console_tail(&state, 595).await;
        assert_eq!(tail.lines.len(), 5);
        assert_eq!(tail.lines[0], "ligne 595");
        assert_eq!(tail.next, 600);
        // Curseur périmé (au-delà du retenu) : recalé au plus ancien.
        let old = get_console_tail(&state, 50).await;
        assert_eq!(old.lines.len(), CONSOLE_TAIL_CAP);
        assert_eq!(old.next, 600);
    }

    #[tokio::test]
    async fn console_tail_new_run_keeps_old_cursor_valid() {
        // Nouveau lancement : anneau vidé mais curseur monotone — un curseur
        // de l'ancien run reste valide et ne ressert que le neuf.
        let (state, _dir) = state_with_tmp();
        let first = prepare_console_emit(&state, None)
            .await
            .expect("enregistreur seul");
        first("ancien".to_string());
        assert_eq!(get_console_tail(&state, 0).await.next, 1);
        let second = prepare_console_emit(&state, None)
            .await
            .expect("enregistreur seul");
        second("neuf".to_string());
        // Curseur 1 (fin de l'ancien run) : seule la ligne neuve ressert.
        let tail = get_console_tail(&state, 1).await;
        assert_eq!(tail.lines, vec!["neuf".to_string()]);
        assert_eq!(tail.next, 2);
    }

    #[tokio::test]
    async fn last_outcome_none_then_some_and_cleared_on_new_run() {
        // Verdict sondable : `None` avant tout lancement, enregistré même
        // sans émetteur, oublié au lancement suivant.
        let (state, _dir) = state_with_tmp();
        assert!(get_last_outcome(&state).await.is_none());
        let tracked = prepare_outcome_emit(&state, None)
            .await
            .expect("enregistreur seul");
        assert!(get_last_outcome(&state).await.is_none());
        let crashed = decide_game_outcome(true, Some("Caused by: boom".to_string()), Some(1), 4242);
        assert_eq!(crashed.status, "crashed");
        tracked(crashed.clone());
        assert_eq!(get_last_outcome(&state).await, Some(crashed));
        // Nouveau lancement : l'ancien verdict ne ressert plus.
        let _ = prepare_outcome_emit(&state, None).await;
        assert!(get_last_outcome(&state).await.is_none());
    }

    #[tokio::test]
    async fn outcome_emit_records_and_forwards() {
        // Avec émetteur : enregistré pour le sondage ET transmis à l'événement.
        let (state, _dir) = state_with_tmp();
        let seen: Arc<std::sync::Mutex<Vec<GameOutcomeDto>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe = seen.clone();
        let emit: Option<Arc<OutcomeEmitter>> = Some(Arc::new(move |o: GameOutcomeDto| {
            probe.lock().unwrap().push(o);
        }));
        let tracked = prepare_outcome_emit(&state, emit)
            .await
            .expect("enveloppe présente");
        let exited = decide_game_outcome(true, None, Some(0), 7);
        tracked(exited.clone());
        assert_eq!(get_last_outcome(&state).await, Some(exited));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn request_login_without_client_id_errors_without_network() {
        // Deterministic without touching the process env: the placeholder id
        // fails `AuthConfig::validate` before any HTTP request is issued.
        let (state, _dir) = state_with_tmp();
        state.lock().await.settings.client_id = launcher_auth::PLACEHOLDER_CLIENT_ID.to_string();
        let err = request_login(&state).await.unwrap_err();
        assert!(err.contains("client-ID"), "unexpected: {err}");
    }

    #[tokio::test]
    async fn manifest_status_init_without_backend_data() {
        // Home screen boot with no manifest: honest empty fields, never fake.
        let (state, _dir) = state_with_tmp();
        let st = get_manifest_status(&state).await;
        assert_eq!(st.update_status, "init");
        assert!(st.version.is_empty());
        assert!(st.mc_version.is_empty());
        assert!(!st.maintenance_enabled);
    }

    #[test]
    fn launcher_version_prefers_tag_then_hash_then_dev() {
        let release = resolve_launcher_version(Some("v0.1.0"), Some("abc1234"));
        assert_eq!(release.display, "v0.1.0");
        assert!(release.detail.contains("release GitHub v0.1.0"));
        let commit = resolve_launcher_version(None, Some("abc1234"));
        assert_eq!(commit.display, "abc1234");
        assert!(commit.detail.contains("commit abc1234"));
        let blank_tag = resolve_launcher_version(Some("  "), Some("abc1234"));
        assert_eq!(blank_tag.display, "abc1234");
        let dev = resolve_launcher_version(None, None);
        assert_eq!(dev.display, "dev");
        assert!(dev.detail.contains("développement"));
        let blank_hash = resolve_launcher_version(None, Some("  "));
        assert_eq!(blank_hash.display, "dev");
    }

    #[test]
    fn github_repo_defaults_to_46launcher_env_still_overrides() {
        // Sans env de compilation : le dépôt par défaut (les releases
        // futures allument le tag sans changement de code). Gardé souple
        // si l'env est posé : l'override compile-time gagne toujours.
        assert_eq!(DEFAULT_GITHUB_REPO, "46Anarchy/46Launcher");
        if LAUNCHER_GITHUB_REPO.is_none() {
            assert_eq!(effective_github_repo(), Some(DEFAULT_GITHUB_REPO));
        } else {
            assert_eq!(effective_github_repo(), LAUNCHER_GITHUB_REPO);
        }
    }

    #[tokio::test]
    async fn launcher_version_fallback_never_errors() {
        // Hermetic: `None` → no network, fallback to the baked hash or
        // `"dev"`, never an error.
        let info = resolve_launcher_version_for_repo(None).await;
        assert!(!info.display.trim().is_empty());
        assert!(!info.detail.trim().is_empty());
        assert!(info.detail.contains("Launcher 46Anarchy"));
    }

    #[test]
    fn build_identity_hash_is_deterministic_and_sensitive() {
        assert_eq!(build_identity_hash(b""), "811c9dc5");
        assert_eq!(build_identity_hash(b"46anarchy-fixture"), "ae6cfe63");
        assert_eq!(
            build_identity_hash(b"46anarchy-fixture"),
            build_identity_hash(b"46anarchy-fixture")
        );
        assert_ne!(
            build_identity_hash(b"46anarchy-fixture"),
            build_identity_hash(b"46anarchy-fixtureX")
        );
    }

    #[tokio::test]
    async fn build_info_returns_non_empty_hashes() {
        let info = get_build_info().await;
        assert!(!info.frontend_hash.trim().is_empty());
        assert!(!info.backend_hash.trim().is_empty());
    }

    #[tokio::test]
    async fn mods_list_empty_dir_returns_empty_not_placeholders() {
        // Mods manager with no manifest and empty mods dir: real empty list.
        let (state, _dir) = state_with_tmp();
        let mods = get_mods_list(&state).await.unwrap();
        assert!(mods.is_empty());
    }

    #[tokio::test]
    async fn mods_list_marks_manifest_entries_required() {
        let (state, _dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        // Disk truth: only physically present `.jar` files are listed. The
        // fixture manifest declares `shared-mod.jar` (mods model) — write it
        // plus an unlisted local file.
        let mods_dir = launcher_core::mods_dir(&state.lock().await.game_dir());
        std::fs::create_dir_all(&mods_dir).unwrap();
        std::fs::write(mods_dir.join("shared-mod.jar"), b"fake").unwrap();
        std::fs::write(mods_dir.join("legacy.jar"), b"fake").unwrap();
        std::fs::write(mods_dir.join("Local.jar"), b"fake").unwrap();
        std::fs::write(mods_dir.join("notes.txt"), b"fake").unwrap();
        let mods = get_mods_list(&state).await.unwrap();
        assert_eq!(mods.len(), 3);
        let shared = mods.iter().find(|m| m.id == "shared-mod.jar").unwrap();
        assert!(shared.required);
        assert!(shared.enabled);
        assert_eq!(shared.size, 4);
        // Model fallback when the file carries no mcmod.info.
        assert_eq!(shared.version, "v1.0.0");
        assert_eq!(shared.display_name, "shared mod");
        let local = mods.iter().find(|m| m.id == "Local.jar").unwrap();
        assert!(!local.required);
        assert!(local.enabled);
    }

    #[tokio::test]
    async fn mods_list_excludes_manifest_phantoms_missing_from_disk() {
        // Manifest declares `shared-mod.jar` but the mods dir is empty:
        // no phantom entry appears.
        let (state, _dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        let mods_dir = launcher_core::mods_dir(&state.lock().await.game_dir());
        std::fs::create_dir_all(&mods_dir).unwrap();
        let mods = get_mods_list(&state).await.unwrap();
        assert!(mods.is_empty());
        // A `.jar.disabled` twin still lists (disabled), never a phantom.
        std::fs::write(mods_dir.join("shared-mod.jar.disabled"), b"fake").unwrap();
        let mods = get_mods_list(&state).await.unwrap();
        assert_eq!(mods.len(), 1);
        let shared = &mods[0];
        assert_eq!(shared.id, "shared-mod.jar");
        assert_eq!(shared.file_name, "shared-mod.jar.disabled");
        assert!(shared.required);
        assert!(!shared.enabled);
    }

    #[tokio::test]
    async fn mods_list_shows_reverse_migrated_mods_enabled_with_required_lock() {
        // Régression « 0 mod chargé » inversée : des `.pala` de l'époque
        // PalaForge adoptés par `migrate_mods_to_jar` apparaissent activés,
        // et le verrou « obligatoire » survit (le manifeste liste `.jar`).
        let (state, _dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        let game_dir = state.lock().await.game_dir();
        let mods_dir = launcher_core::mods_dir(&game_dir);
        std::fs::create_dir_all(&mods_dir).unwrap();
        std::fs::write(mods_dir.join("shared-mod.pala"), b"fake").unwrap();
        std::fs::write(mods_dir.join("OptiFine.pala.disabled"), b"fake").unwrap();
        let renamed = launcher_core::migrate_mods_to_jar(&game_dir);
        assert_eq!(
            renamed,
            vec![
                "OptiFine.jar.disabled".to_string(),
                "shared-mod.jar".to_string()
            ]
        );
        let mods = get_mods_list(&state).await.unwrap();
        assert_eq!(mods.len(), 2);
        let shared = mods.iter().find(|m| m.id == "shared-mod.jar").unwrap();
        assert!(shared.required && shared.enabled);
        let opt = mods.iter().find(|m| m.id == "OptiFine.jar").unwrap();
        assert!(!opt.required && !opt.enabled);
        assert_eq!(opt.file_name, "OptiFine.jar.disabled");
        // Le basculement fonctionne après migration, sur les nouveaux ids.
        let on = toggle_mod(&state, "OptiFine.jar".to_string(), true)
            .await
            .unwrap();
        assert!(on.enabled);
        assert!(mods_dir.join("OptiFine.jar").is_file());
    }

    #[tokio::test]
    async fn verify_checks_manifest_jars_by_direct_name() {
        // `verify_files` : le manifeste `present.jar` est satisfait par le
        // disque `present.jar` (mêmes octets). Un jumeau historique `.pala`
        // (époque PalaForge) ne compte pas.
        let (state, _dir) = state_with_tmp();
        let present_body = vec![0xABu8; 64];
        let absent_body = vec![0xCDu8; 32];
        let present: &[u8] = &present_body;
        let absent: &[u8] = &absent_body;
        let m = stub_manifest(
            "http://example.invalid",
            &[("present.jar", present), ("absent.jar", absent)],
        );
        state.lock().await.manifest = Some(m);
        let mods_dir = launcher_core::mods_dir(&state.lock().await.game_dir());
        std::fs::create_dir_all(&mods_dir).unwrap();
        std::fs::write(mods_dir.join("present.jar"), &present_body).unwrap();
        // Jumeau historique : ne satisfait pas la vérification `.jar`.
        std::fs::write(mods_dir.join("absent.pala"), &absent_body).unwrap();
        let report = verify_files(&state).await.unwrap();
        assert_eq!(report.total, 3);
        assert_eq!(report.stale, 2);
        assert_eq!(
            report.stale_bytes,
            (absent_body.len() + launcher_core::FORGE_UNIVERSAL_SIZE as usize) as u64
        );
    }

    #[test]
    fn bytecode_gate_warns_without_failing_launch() {
        // Warn-only (nashorn 15.4 v53) : un jar v53 planifié au classpath
        // émet un avertissement français nommant le jar — PAS d'erreur, le
        // plan comme le lancement continuent (FML ignore et continue).
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "net.minecraft.launchwrapper.Launch",
            "arguments": {"game": [], "jvm": []},
            "models": [{"name": "libraries-windows", "dest": "libraries",
                         "properties": {"classpath": true}, "os": ["WINDOWS"]}],
            "files": [
                {"name": "nashorn-core-15.4.jar", "model": "libraries-windows",
                 "url": "https://x/nashorn.jar", "size": 10, "sha1": "aa",
                 "path": "org/openjdk/nashorn/nashorn-core-15.4/nashorn-core-15.4.jar", "os": ["WINDOWS"]}
            ]
        }"#,
        )
        .unwrap();
        // Le seau cross-platform planifie nashorn sur Linux sans erreur…
        let planned = launcher_core::plan_downloads(&m, game, launcher_core::Os::Linux);
        assert_eq!(planned.len(), 1, "nashorn planifié sur Linux : {planned:?}");
        // …puis le jar v53 sur disque n'émet qu'un avertissement nommé.
        let dest = &planned[0].dest;
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        {
            let f = std::fs::File::create(dest).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("a/N.class", opts).unwrap();
            w.write_all(&[0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 53, 0x00, 0x10])
                .unwrap();
            w.finish().unwrap();
        }
        let warnings = assert_classpath_bytecode(&m, game, false);
        assert_eq!(warnings.len(), 1, "un avertissement, pas d'échec");
        assert!(
            warnings[0].contains("nashorn-core-15.4.jar"),
            "jar nommé : {:?}",
            warnings
        );
        assert!(
            warnings[0].contains("v53"),
            "version nommée : {:?}",
            warnings
        );
    }

    #[test]
    fn wiped_libraries_flag_stale_then_heal_after_fill() {
        // Non-régression wipe 16:41 : game dir avec le seul remplissage
        // Mojang (33 jars) mais sans les libs CDN planifiées → stale=true ;
        // après remplissage simulé → fresh. Le JOUER suivant re-télécharge
        // les 304 automatiquement, zéro étape manuelle.
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "net.minecraft.launchwrapper.Launch",
            "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "libraries-windows", "dest": "libraries",
                 "properties": {"classpath": true}, "os": ["WINDOWS"]},
                {"name": "versions", "dest": "versions", "os": []}
            ],
            "files": [
                {"name": "launchwrapper-1.12.jar", "model": "libraries-windows",
                 "url": "https://x/lw.jar", "size": 3, "sha1": "a",
                 "path": "net/minecraft/launchwrapper/1.12/launchwrapper-1.12.jar", "os": ["WINDOWS"]},
                {"name": "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "model": "versions",
                 "url": "https://x/forge.jar", "size": 5, "sha1": "c",
                 "path": "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "os": []}
            ]
        }"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        assert!(
            cdn_plan_libs_missing(&m, game, false),
            "libs CDN absentes → stale (même à version inchangée)"
        );
        for p in launcher_core::plan_downloads(&m, game, launcher_core::Os::current()) {
            if let Some(parent) = p.dest.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p.dest, b"x").unwrap();
        }
        assert!(
            !cdn_plan_libs_missing(&m, game, false),
            "après remplissage simulé → fresh"
        );
    }

    /// Craft a jar containing `mcmod.info` `content` (plus a dummy entry so
    /// the zip is never empty).
    fn jar_with_mcmod_info(dir: &std::path::Path, name: &str, content: Option<&str>) {
        let path = dir.join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        if let Some(body) = content {
            writer.start_file("mcmod.info", options).unwrap();
            std::io::Write::write_all(&mut writer, body.as_bytes()).unwrap();
        }
        writer.start_file("dummy.class", options).unwrap();
        std::io::Write::write_all(&mut writer, b"fake").unwrap();
        writer.finish().unwrap();
    }

    #[tokio::test]
    async fn mods_list_prefers_mcmod_info_over_filename() {
        let (state, _dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        let mods_dir = launcher_core::mods_dir(&state.lock().await.game_dir());
        std::fs::create_dir_all(&mods_dir).unwrap();
        // Array form.
        jar_with_mcmod_info(
            &mods_dir,
            "arraymod.jar",
            Some(
                r#"[{"modid":"am","name":"Array Mod","version":"1.2.3","description":"Depuis mcmod"}]"#,
            ),
        );
        // `{modlist: [...]}` form (first entry wins).
        jar_with_mcmod_info(
            &mods_dir,
            "listmod.jar",
            Some(
                r#"{"modlist":[{"modid":"l1","name":"Premier","version":"0.1","description":"Un"},{"modid":"l2","name":"Second","version":"0.2","description":"Deux"}]}"#,
            ),
        );
        // Malformed JSON → filename fallback.
        jar_with_mcmod_info(&mods_dir, "broken_mod.jar", Some("{pas du json"));
        // No mcmod.info at all → filename fallback (`.jar` stripped).
        jar_with_mcmod_info(&mods_dir, "plain-mod.jar", None);
        let mods = get_mods_list(&state).await.unwrap();
        assert_eq!(mods.len(), 4);
        let array = mods.iter().find(|m| m.id == "arraymod.jar").unwrap();
        assert_eq!(array.display_name, "Array Mod");
        assert_eq!(array.version, "1.2.3");
        assert_eq!(array.description, "Depuis mcmod");
        assert!(!array.required);
        let listed = mods.iter().find(|m| m.id == "listmod.jar").unwrap();
        assert_eq!(listed.display_name, "Premier");
        assert_eq!(listed.version, "0.1");
        let broken = mods.iter().find(|m| m.id == "broken_mod.jar").unwrap();
        assert_eq!(broken.display_name, "broken mod");
        assert!(broken.version.is_empty());
        let plain = mods.iter().find(|m| m.id == "plain-mod.jar").unwrap();
        assert_eq!(plain.display_name, "plain mod");
        assert!(plain.version.is_empty());
    }

    #[tokio::test]
    async fn mods_toggle_optional_jar_and_locks_required() {
        let (state, dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        let mods_dir = launcher_core::mods_dir(&state.lock().await.game_dir());
        let _dir = dir;
        std::fs::create_dir_all(&mods_dir).unwrap();
        std::fs::write(mods_dir.join("OptiFine.jar"), b"fake").unwrap();
        // Manifest-listed mod on disk: required + locked.
        std::fs::write(mods_dir.join("shared-mod.jar"), b"fake").unwrap();
        // Optional mod shows up unlocked.
        let mods = get_mods_list(&state).await.unwrap();
        let opt = mods.iter().find(|m| m.id == "OptiFine.jar").unwrap();
        assert!(!opt.required && opt.enabled);
        // Disable → renamed to `.jar.disabled`.
        let off = toggle_mod(&state, "OptiFine.jar".to_string(), false)
            .await
            .unwrap();
        assert!(!off.enabled);
        assert_eq!(off.file_name, "OptiFine.jar.disabled");
        assert!(mods_dir.join("OptiFine.jar.disabled").is_file());
        // Re-enable → renamed back (round-trip on the new ids).
        let on = toggle_mod(&state, "OptiFine.jar".to_string(), true)
            .await
            .unwrap();
        assert!(on.enabled);
        assert!(mods_dir.join("OptiFine.jar").is_file());
        // Required (manifest-listed) mod is locked.
        let err = toggle_mod(&state, "shared-mod.jar".to_string(), false)
            .await
            .unwrap_err();
        assert!(err.contains("obligatoire"), "unexpected: {err}");
        // Unknown mod errors without touching disk.
        let err = toggle_mod(&state, "nope.jar".to_string(), false)
            .await
            .unwrap_err();
        assert!(err.contains("introuvable"), "unexpected: {err}");
        // Path traversal is refused.
        let err = toggle_mod(&state, "../evil.jar".to_string(), false)
            .await
            .unwrap_err();
        assert!(err.contains("invalide"), "unexpected: {err}");
    }

    #[tokio::test]
    async fn save_settings_roundtrips_java_screen_fields() {
        let (state, dir) = state_with_tmp();
        let s = SettingsDto {
            game_dir: dir.path().to_string_lossy().into_owned(),
            memory_max: "8G".to_string(),
            resolution: "2560x1440".to_string(),
            fullscreen: true,
            console_live: false,
            jvm_preset: "lowram".to_string(),
            jvm_args: "-Xmx8G".to_string(),
            experimental_runtime: true,
            ..Default::default()
        };
        let back = save_settings(&state, s.clone()).await.unwrap();
        assert_eq!(back, s);
        assert!(get_settings(&state).await.experimental_runtime);
        assert_eq!(get_settings(&state).await.resolution, "2560x1440");
    }

    #[tokio::test]
    async fn save_settings_preserve_le_registre_des_comptes() {
        // Non-régression : le formulaire Réglages ne transporte ni `accounts`
        // ni `active_account` (comme `collectSettings` côté frontend) — un
        // enregistrement ne doit jamais vider le registre sur disque.
        use crate::state::AccountRef;
        let (state, dir) = state_with_tmp();
        {
            let mut s = state.lock().await;
            s.upsert_session(session_named("uuid-a", "Alice"));
            s.settings.accounts = vec![AccountRef {
                uuid: "uuid-a".to_string(),
                name: "Alice".to_string(),
            }];
            s.settings.active_account = Some("uuid-a".to_string());
        }
        // Charge utile « formulaire » : réglages seuls, registre vide.
        let form = SettingsDto {
            game_dir: dir.path().to_string_lossy().into_owned(),
            memory_max: "8G".to_string(),
            ..Default::default()
        };
        assert!(form.accounts.is_empty());
        let back = save_settings(&state, form).await.unwrap();
        // Mémoire : registre intact, réglage appliqué.
        assert_eq!(back.memory_max, "8G");
        assert_eq!(
            back.accounts,
            vec![AccountRef {
                uuid: "uuid-a".to_string(),
                name: "Alice".to_string(),
            }]
        );
        assert_eq!(back.active_account.as_deref(), Some("uuid-a"));
        // Disque : relecture du fichier, registre intact.
        let game_dir = state.lock().await.game_dir();
        let on_disk = crate::state::load_settings_file(&crate::state::settings_path(&game_dir));
        assert_eq!(on_disk.accounts.len(), 1);
        assert_eq!(on_disk.accounts[0].name, "Alice");
        assert_eq!(on_disk.active_account.as_deref(), Some("uuid-a"));
    }

    #[tokio::test]
    async fn remove_account_supprime_aussi_l_entree_sans_session() {
        // Un compte en quarantaine (registre seul, sans session) reste
        // supprimable explicitement par « Retirer ».
        use crate::state::AccountRef;
        let (state, _dir) = state_with_tmp();
        {
            let mut s = state.lock().await;
            s.settings.accounts = vec![AccountRef {
                uuid: "uuid-q".to_string(),
                name: "Quentin".to_string(),
            }];
        }
        assert!(remove_account(&state, "uuid-q".to_string()).await);
        assert!(state.lock().await.settings.accounts.is_empty());
        assert!(!remove_account(&state, "uuid-zz".to_string()).await);
    }

    #[test]
    fn avatar_cache_freshness_is_mtime_based() {
        // Missing file → stale (refetch path).
        let dir = tempfile::tempdir().unwrap();
        assert!(!avatar_cache_fresh(&dir.path().join("absent.png")));
        // Just-written file → fresh (network short-circuit).
        let fresh = dir.path().join("fresh.png");
        std::fs::write(&fresh, b"fake").unwrap();
        assert!(avatar_cache_fresh(&fresh));
    }

    #[test]
    fn avatar_cache_key_accepts_dashed_and_bare_uuids() {
        assert_eq!(
            avatar_cache_key("123e4567-e89b-12d3-a456-426614174000"),
            Some("123e4567e89b12d3a456426614174000".to_string())
        );
        assert_eq!(
            avatar_cache_key("123E4567E89B12D3A456426614174000"),
            Some("123e4567e89b12d3a456426614174000".to_string())
        );
        assert!(avatar_cache_key("xyz").is_none());
        assert!(avatar_cache_key("").is_none());
        assert!(avatar_cache_key("123e4567-e89b-12d3-a456-42661417400").is_none());
        assert!(avatar_cache_key("123e4567-e89b-12d3-a456-42661417400Z").is_none());
    }

    #[tokio::test]
    async fn avatar_rejects_invalid_uuid_without_network() {
        // Hermetic: validation fails before any HTTP request is issued.
        let (state, _dir) = state_with_tmp();
        for bad in [
            "xyz",
            "",
            "not-a-uuid",
            "123e4567-e89b-12d3-a456-42661417400",
        ] {
            assert_eq!(
                get_avatar(&state, bad.to_string()).await.unwrap_err(),
                "UUID invalide."
            );
        }
    }

    #[tokio::test]
    async fn avatar_cache_hit_returns_data_url_without_network() {
        // Hermetic: a pre-written cache file short-circuits before any
        // HTTP request is issued.
        let (state, _dir) = state_with_tmp();
        let undashed = "123e4567e89b12d3a456426614174000";
        let path = avatar_cache_path(&state.lock().await.game_dir(), undashed);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut png = vec![0x89, 0x50, 0x4E, 0x47];
        png.resize(200, 0);
        std::fs::write(&path, &png).unwrap();
        let url = get_avatar(&state, "123e4567-e89b-12d3-a456-426614174000".to_string())
            .await
            .unwrap();
        use base64::Engine as _;
        assert_eq!(
            url,
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&png)
            )
        );
    }

    #[test]
    fn open_external_allowlists_browser_urls() {
        // Real destinations used by the UI (validation only — no spawn).
        assert!(validate_external_url("https://discord.46anarchy.fr").is_ok());
        assert!(validate_external_url("https://www.microsoft.com/link?foo=1").is_ok());
        assert!(validate_external_url("https://46anarchy.fr/").is_ok());
        assert!(validate_external_url("https://sub.discord.gg/x").is_ok());
        // Scheme lockdown.
        assert!(validate_external_url("http://46anarchy.fr/").is_err());
        assert!(validate_external_url("file:///etc/passwd").is_err());
        assert!(validate_external_url("javascript:alert(1)").is_err());
        // Host allowlist (incl. lookalike-domain tricks).
        assert!(validate_external_url("https://evil.example/").is_err());
        assert!(validate_external_url("https://evil-microsoft.com/").is_err());
        assert!(validate_external_url("https://microsoft.com.evil.example/").is_err());
    }

    // ------------------------------------------------------------------
    // Suivi post-lancement : assistants purs (aucun spawn réel, faux PID)
    // ------------------------------------------------------------------

    /// Extrait réel du `latest.log` du plantage 14:23 (format texte FML).
    const CRASH_LOG: &str = "[14:23:01] [main/INFO]: Launching wrapped minecraft {net.minecraft.client.main.Main}\n\
        [14:23:02] [main/ERROR]: Unable to launch\n\
        java.lang.reflect.InvocationTargetException\n\
        \tat sun.reflect.NativeMethodAccessorImpl.invoke0(Native Method) ~[?:1.8.0_504]\n\
        \tat net.minecraft.launchwrapper.Launch.launch(Launch.java:159) [launchwrapper-1.13.jar:?]\n\
        Caused by: java.lang.NoSuchMethodError: net.minecraft.util.JsonUtils.func_151218_a(Lcom/google/gson/JsonObject;Ljava/lang/String;Lcom/google/gson/JsonObject;)Lcom/google/gson/JsonObject;\n\
        \tat net.minecraft.client.resources.ResourceIndex.<init>(SourceFile:37) ~[bqo.class:?]\n\
        \tat net.minecraft.client.Minecraft.<init>(Minecraft.java:319) ~[bao.class:?]\n\
        [14:23:02] [main/INFO]: [java.lang.Throwable$WrappedPrintStream:println:749]: cpw.mods.fml.relauncher.FMLSecurityManager$ExitTrappedException\n";

    #[test]
    fn crash_cause_prefers_caused_by_over_unable_over_exception() {
        // La ligne `Caused by:` (cause nommée pour le toast) gagne.
        let cause = find_crash_cause(CRASH_LOG).expect("cause trouvée");
        assert!(
            cause.starts_with("Caused by: java.lang.NoSuchMethodError: net.minecraft.util.JsonUtils.func_151218_a"),
            "unexpected: {cause}"
        );
        // Sans `Caused by:`, `Unable to launch` gagne sur l'exception générique.
        let no_caused = CRASH_LOG.replace(
            "Caused by: java.lang.NoSuchMethodError",
            "Suite: java.lang.NoSuchMethodError",
        );
        assert_eq!(
            find_crash_cause(&no_caused).as_deref(),
            Some("[14:23:02] [main/ERROR]: Unable to launch")
        );
        // Sans les deux, repli sur la première ligne d'exception…
        let generic = "quelque chose\njava.lang.RuntimeException: boom\nfin";
        assert_eq!(
            find_crash_cause(generic).as_deref(),
            Some("java.lang.RuntimeException: boom")
        );
        // … mais le bruit `ExitTrappedException` (post-plantage, bénin) ne
        // vaut jamais verdict à lui seul, pas plus qu'un log sain.
        let noise = "[main/INFO]: [java.lang.Throwable$WrappedPrintStream:println:749]: cpw.mods.fml.relauncher.FMLSecurityManager$ExitTrappedException\n";
        assert_eq!(find_crash_cause(noise), None);
        // Priorité liaison : l'enveloppe bénigne `IllegalArgumentException`
        // (découverte ASM) ne doit pas masquer la vraie chaîne fatale qui
        // suit (`NoSuchMethodError` JsonUtils — observé en direct).
        let shadowed = "Caused by: java.lang.IllegalArgumentException: enveloppe ASM\n\
            \tat net.mixin.Découverte(SourceFile:1)\n\
            Caused by: java.lang.NoSuchMethodError: net.minecraft.util.JsonUtils.func_151218_a(Lcom/google/gson/JsonObject;)Lcom/google/gson/JsonObject;\n\
            \tat net.minecraft.client.Minecraft.<init>(Minecraft.java:319)";
        let cause = find_crash_cause(shadowed).expect("cause de liaison trouvée");
        assert!(
            cause.starts_with("Caused by: java.lang.NoSuchMethodError"),
            "unexpected: {cause}"
        );
        // Sans marqueur de liaison, la première ligne `Caused by:`
        // générique gagne toujours (ordre conservé).
        let generic_first = "Caused by: java.lang.IllegalArgumentException: enveloppe\n\
            Caused by: java.lang.RuntimeException: suite";
        assert_eq!(
            find_crash_cause(generic_first).as_deref(),
            Some("Caused by: java.lang.IllegalArgumentException: enveloppe")
        );
        assert_eq!(
            find_crash_cause("démarrage normal\nchargement des mods\n"),
            None
        );
        assert_eq!(find_crash_cause(""), None);
    }

    #[test]
    fn crash_cause_found_inside_xml_console_layout() {
        // `launcher-console.log` utilise le layout XML log4j (config
        // `client-1.7.xml`) : les marqueurs restent trouvables en
        // sous-chaînes, y compris `Caused by:` dans le CDATA Throwable.
        let xml = "<log4j:Event logger=\"LaunchWrapper\" level=\"ERROR\">\n\
            <log4j:Message><![CDATA[Unable to launch]]></log4j:Message>\n\
            <log4j:Throwable><![CDATA[java.lang.reflect.InvocationTargetException\n\
            Caused by: java.lang.NoSuchMethodError: net.minecraft.util.JsonUtils.func_151218_a(Lcom/google/gson/JsonObject;Ljava/lang/String;Lcom/google/gson/JsonObject;)Lcom/google/gson/JsonObject;\n\
            ]]></log4j:Throwable>\n</log4j:Event>";
        let cause = find_crash_cause(xml).expect("cause trouvée dans le XML");
        assert!(
            cause.starts_with("Caused by: java.lang.NoSuchMethodError"),
            "unexpected: {cause}"
        );
    }

    #[test]
    fn crash_tail_lines_returns_last_non_empty() {
        let tail = crash_tail_lines("a\n\nb\nc\n\n", 2);
        assert_eq!(tail, vec!["b".to_string(), "c".to_string()]);
        let all = crash_tail_lines(CRASH_LOG, 3);
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|l| !l.trim().is_empty()));
        assert!(all.last().unwrap().contains("ExitTrappedException"));
        assert!(crash_tail_lines("", 10).is_empty());
        assert!(crash_tail_lines("\n  \n", 10).is_empty());
    }

    #[test]
    fn log_delta_after_handles_rotation_bounds_and_utf8() {
        let content = "ligne1\nligne2\nligne3\n";
        // Delta normal : tout ce qui suit l'offset.
        assert_eq!(log_delta_after(content, 7, 1024), "ligne2\nligne3\n");
        // Offset au-delà de la fin (fichier remplacé/tronqué) : reprise à 0.
        assert_eq!(log_delta_after(content, 9999, 1024), content);
        // Borné aux derniers octets.
        assert_eq!(log_delta_after(content, 0, 7), "ligne3\n");
        // Frontières UTF-8 respectées (ni panique ni remplacement).
        let accent = "début—é\nfin\n";
        let mid = accent.find('—').unwrap() + 1; // milieu du tiret
        let delta = log_delta_after(accent, mid as u64, 1024);
        assert!(delta.is_ascii() || !delta.is_empty());
        std::str::from_utf8(delta.as_bytes()).expect("frontière valide");
        // `max_bytes` qui coupe dans un caractère multi-octets : recalé.
        let tiny = log_delta_after("é", 0, 1);
        assert!(tiny.is_empty() || tiny == "é");
    }

    #[test]
    fn split_complete_lines_holds_partial_and_force_flushes_huge() {
        // Lignes complètes renvoyées, fragment final conservé (non consommé).
        let chunk = b"un\ndeux\ntrois-partiel";
        let (lines, consumed) = split_complete_lines(chunk);
        assert_eq!(lines, vec!["un".to_string(), "deux".to_string()]);
        assert_eq!(consumed, b"un\ndeux\n".len());
        // Le fragment est relu au passage suivant et complété.
        let rest = b"trois-partiel-suite\nquatre\n";
        let (lines, consumed) = split_complete_lines(rest);
        assert_eq!(lines[0], "trois-partiel-suite");
        assert_eq!(lines[1], "quatre");
        assert_eq!(consumed, rest.len());
        // Sans `\n` : rien n'est consommé (attente de la fin de ligne)…
        let (lines, consumed) = split_complete_lines(b"attente");
        assert!(lines.is_empty());
        assert_eq!(consumed, 0);
        // … sauf fragment démesuré : affiché de force, tronqué.
        let huge = vec![b'x'; CONSOLE_PENDING_MAX_BYTES + 10];
        let (lines, consumed) = split_complete_lines(&huge);
        assert_eq!(lines.len(), 1);
        assert_eq!(consumed, huge.len());
        assert!(lines[0].ends_with('…'));
        assert_eq!(lines[0].chars().count(), GAME_TAIL_LINE_CHARS + 1);
    }

    #[test]
    fn format_console_line_truncates_and_strips_cr() {
        // (`\n` est découpé en amont par `split_complete_lines`, ici on ne
        // traite que des lignes unitaires.)
        assert_eq!(format_console_line("a\rb\r"), "ab");
        let long = "é".repeat(GAME_TAIL_LINE_CHARS + 5);
        let out = format_console_line(&long);
        assert!(out.ends_with('…'));
        // Compté en caractères (pas d'octets) : pas de `é` coupé.
        assert_eq!(out.chars().count(), GAME_TAIL_LINE_CHARS + 1);
        assert_eq!(format_console_line("court"), "court");
    }

    #[test]
    fn decide_game_outcome_never_dead_ends_journal() {
        // Faux PID : aucune spawning réel, seules les phrases comptent.
        let running = decide_game_outcome(false, None, None, 4242);
        assert_eq!(running.status, "running");
        assert_eq!(running.code, None);
        assert_eq!(running.cause, None);
        assert!(
            running.detail.contains("4242"),
            "unexpected: {}",
            running.detail
        );
        assert!(
            running.detail.contains("en cours"),
            "unexpected: {}",
            running.detail
        );

        let crashed = decide_game_outcome(
            true,
            Some("Caused by: java.lang.NoSuchMethodError: boom".to_string()),
            Some(1),
            4242,
        );
        assert_eq!(crashed.status, "crashed");
        assert_eq!(crashed.code, Some(1));
        assert_eq!(
            crashed.cause.as_deref(),
            Some("Caused by: java.lang.NoSuchMethodError: boom")
        );
        assert!(
            crashed.detail.contains("Plantage"),
            "unexpected: {}",
            crashed.detail
        );
        assert!(
            crashed.detail.contains("Console"),
            "unexpected: {}",
            crashed.detail
        );

        let exited = decide_game_outcome(true, None, Some(0), 4242);
        assert_eq!(exited.status, "exited");
        assert_eq!(exited.code, Some(0));
        assert_eq!(exited.cause, None);
        assert!(
            exited.detail.contains("code 0"),
            "unexpected: {}",
            exited.detail
        );

        // Code 0 AVEC marqueurs dans la fenêtre précoce (fermeture par la
        // croix, lignes `Exception` bénignes) : toujours `exited`, jamais
        // `crashed` — un jeu vraiment planté ne sort quasiment jamais en 0.
        let clean_exit = decide_game_outcome(
            true,
            Some("java.lang.RuntimeException: boom".to_string()),
            Some(0),
            4242,
        );
        assert_eq!(clean_exit.status, "exited");
        assert_eq!(clean_exit.code, Some(0));
        assert_eq!(clean_exit.cause, None);
        assert!(
            clean_exit.detail.contains("code 0"),
            "unexpected: {}",
            clean_exit.detail
        );

        // Code inconnu : jamais de panique, point d'interrogation honnête.
        let unknown = decide_game_outcome(true, None, None, 7);
        assert_eq!(unknown.status, "exited");
        assert!(
            unknown.detail.contains('?'),
            "unexpected: {}",
            unknown.detail
        );
    }

    #[test]
    fn short_cause_truncates_for_toast() {
        assert_eq!(short_cause("court"), "court");
        let long = "x".repeat(CAUSE_TOAST_CHARS + 20);
        let short = short_cause(&long);
        assert!(short.ends_with('…'));
        assert_eq!(short.chars().count(), CAUSE_TOAST_CHARS + 1);
    }

    #[test]
    fn loki_constants_point_at_cdn_jar() {
        // L'URL et la destination portent le nom CDN `.jar` (vanilla Forge).
        assert_eq!(LOKI_FILE_NAME, "loki.jar");
        assert_eq!(LOKI_URL, "https://cdn.46anarchy.fr/files/loki.jar");
    }

    #[test]
    fn manifest_without_loki_needs_explicit_download() {
        assert!(!manifest_lists_loki(&manifest()));
        let mut m = manifest();
        let mut file = m.files[0].clone();
        file.name = LOKI_FILE_NAME.to_string();
        m.files.push(file);
        assert!(manifest_lists_loki(&m));
    }

    #[test]
    fn spawn_config_carries_game_dir_as_cwd() {
        // Pure assertion, aucun processus : le CWD du jeu détaché est la
        // racine d'instance (FS-Patches résout `./` contre lui).
        let dir = tempfile::tempdir().unwrap();
        let game_dir = launcher_core::normalize_game_dir(dir.path());
        let argv = vec!["java".to_string(), "-cp".to_string(), "x.jar".to_string()];
        let log = game_dir.join("logs").join("launcher-console.log");
        let cfg = spawn_config_for(&argv, &game_dir, &log);
        assert_eq!(cfg.cwd, game_dir);
        assert_eq!(cfg.argv, argv);
        assert_eq!(cfg.log_path, log);
        // Jamais hérité, jamais `/`.
        assert_ne!(cfg.cwd, PathBuf::from("/"));
    }

    // ------------------------------------------------------------------
    // Install-progress plumbing (recording emitters, loopback stub server)
    // ------------------------------------------------------------------

    /// Minimal loopback HTTP stub: serves `bodies[path]`, 404 otherwise.
    /// Returns the base URL (no external network touched). Bodies stream in
    /// 16 Kio slices (~1 ms apart) so multi-Kio files yield many transport
    /// chunks, like a real CDN.
    async fn stub_server(
        bodies: std::collections::HashMap<String, Vec<u8>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let bodies = bodies.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let mut head = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                head.extend_from_slice(&buf[..n]);
                                if head.len() > 8192 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let path = String::from_utf8_lossy(&head)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .to_string();
                    let body = bodies.get(&path).cloned().unwrap_or_default();
                    let status = if bodies.contains_key(&path) {
                        "200 OK"
                    } else {
                        "404 Not Found"
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                    for slice in body.chunks(16 * 1024) {
                        let _ = sock.write_all(slice).await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                });
            }
        });
        (base, handle)
    }

    fn recording_emit() -> (
        Option<Arc<ProgressEmitter>>,
        Arc<std::sync::Mutex<Vec<ProgressPayload>>>,
    ) {
        let seen: Arc<std::sync::Mutex<Vec<ProgressPayload>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe = seen.clone();
        let emit: Option<Arc<ProgressEmitter>> = Some(Arc::new(move |p: ProgressPayload| {
            probe.lock().unwrap().push(p);
        }));
        (emit, seen)
    }

    /// Manifest with stub-served files (correct size + SHA1).
    fn stub_manifest(base: &str, files: &[(&str, &[u8])]) -> Manifest {
        let entries: Vec<serde_json::Value> = files
            .iter()
            .map(|(name, body)| {
                serde_json::json!({
                    "name": name,
                    "model": "mods",
                    "url": format!("{base}/{name}"),
                    "size": body.len(),
                    "sha1": launcher_core::sha1_hex(body),
                    "path": name,
                    "os": [],
                })
            })
            .collect();
        launcher_core::parse_manifest(
            &serde_json::json!({
                "name": "Stub",
                "root": "stub",
                "version": "v9.9.9-test",
                "assetIndex": "https://example.com/index.json",
                "mc_version": "1.7.10",
                "maintenance": {"enabled": false, "message": ""},
                "main": "net.minecraft.launchwrapper.Launch",
                "arguments": {"game": [], "jvm": []},
                "models": [{"name": "mods", "dest": "mods"}],
                "files": entries,
            })
            .to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn run_install_streams_progress_to_recording_emitter() {
        // `install-progress` plumbing end to end (no Tauri needed): every
        // payload the frontend `renderProgress` consumes flows through here.
        let bodies: std::collections::HashMap<String, Vec<u8>> = [
            ("/one.jar".to_string(), vec![0xA5u8; 8192]),
            ("/two.jar".to_string(), vec![0x3Cu8; 4096]),
        ]
        .into_iter()
        .collect();
        let (base, srv) = stub_server(bodies.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let m = stub_manifest(
            &base,
            &[
                ("one.jar", &bodies["/one.jar"]),
                ("two.jar", &bodies["/two.jar"]),
            ],
        );
        let (emit, seen) = recording_emit();
        let run = std::sync::Arc::new(InstallRun::new(emit));
        let summary = run_install(&m, dir.path(), &run, None, false)
            .await
            .expect("stub install succeeds");
        assert_eq!((summary.total_files, summary.downloaded_files), (2, 2));
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "emitter must receive progress payloads");
        assert!(
            seen.iter().all(|p| (0.0..=1.0).contains(&p.fraction)),
            "fractions stay on the 0..1 scale, got {:?}",
            seen.iter().map(|p| p.fraction).collect::<Vec<_>>()
        );
        assert!(
            seen.iter().any(|p| p.fraction == 1.0),
            "a terminal 1.0 payload must be emitted"
        );
        assert!(seen.iter().all(|p| !p.detail.is_empty()));
        // Modèle unifié : fraction == octets/total sur chaque émission, vrais
        // noms de fichiers, aucun mot de phase.
        for p in seen.iter() {
            let expected = if p.total_bytes > 0 {
                p.completed_bytes.min(p.total_bytes) as f32 / p.total_bytes as f32
            } else if p.total_files > 0 {
                p.completed_files as f32 / p.total_files as f32
            } else {
                0.0
            };
            assert!(
                (p.fraction - expected).abs() < 1e-6,
                "fraction {p:?} != bytes/total"
            );
            assert!(
                !["mojang", "modern", "java21", "java", "terminé", "mods", "flavor", "bytecode"]
                    .contains(&p.current_file.as_str()),
                "placeholder filename: {:?}",
                p.current_file
            );
        }
        srv.abort();
    }

    #[tokio::test]
    async fn run_install_progress_is_byte_weighted_and_spread() {
        // Non-régression « barre chargée à la fin » : sur un stub
        // multi-fichiers aux tailles déséquilibrées, les fractions
        // intermédiaires doivent s'étaler sur (0, 1) dès les premiers
        // octets — pas uniquement un 1.0 terminal. Modèle unifié : chaque
        // émission vaut octets réglés / total du run (aucune bande), les
        // morceaux en vol font grimper la barre pendant les gros fichiers.
        // Tailles choisies pour que TOUT sous-ensemble de 3 fichiers sur 4
        // pèse moins de 90 % du total : les 3 premières fins de fichier
        // donnent alors forcément 3 fractions distinctes sous 0.9, et les
        // mises à jour par tronçon ajoutent l'étalement continu.
        let sizes = [768usize, 512, 896, 384];
        let total: usize = sizes.iter().sum();
        assert!(sizes.iter().map(|s| total - s).all(|s| s * 10 < total * 9));
        let bodies: std::collections::HashMap<String, Vec<u8>> = sizes
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("/f{i}.jar"), vec![(i as u8) + 1; s * 1024]))
            .collect();
        let (base, srv) = stub_server(bodies.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<(String, Vec<u8>)> = ["f0.jar", "f1.jar", "f2.jar", "f3.jar"]
            .iter()
            .map(|n| (n.to_string(), bodies[&format!("/{n}")].clone()))
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let m = stub_manifest(&base, &refs);
        let (emit, seen) = recording_emit();
        let run = std::sync::Arc::new(InstallRun::new(emit));
        let summary = run_install(&m, dir.path(), &run, None, false)
            .await
            .expect("stub install succeeds");
        assert_eq!((summary.total_files, summary.downloaded_files), (4, 4));
        let seen = seen.lock().unwrap();
        let fractions: Vec<f32> = seen.iter().map(|p| p.fraction).collect();
        assert!(!fractions.is_empty());
        assert!(
            fractions.iter().all(|f| (0.0..=1.0).contains(f)),
            "fractions hors échelle : {fractions:?}"
        );
        // Terminal pile à 1.0, et jamais avant (l'intermédiaire du dernier
        // morceau est supprimé : la fin de fichier exacte reste l'unique 1.0).
        assert_eq!(*fractions.last().unwrap(), 1.0);
        assert!(
            fractions[..fractions.len() - 1].iter().all(|f| *f < 1.0),
            "1.0 prématuré : {fractions:?}"
        );
        // Étalement : au moins 4 fractions distinctes sous 0.9 avant le
        // 1.0 terminal (3 fins de fichier garanties par construction + les
        // tronçons en continu).
        let distinct: std::collections::BTreeSet<u32> = fractions
            .iter()
            .filter(|f| **f < 0.9)
            .map(|f| f.to_bits())
            .collect();
        assert!(
            distinct.len() >= 4,
            "barre non étalée ({} distinctes sous 0.9) : {fractions:?}",
            distinct.len()
        );
        // Chaque émission porte le vrai nom du fichier en cours et sa
        // fraction vaut octets/total (lignes `Fichier i/n — nom — x/y (p %)`).
        for p in seen.iter() {
            assert!(
                p.current_file.ends_with(".jar"),
                "vrai nom attendu, got {:?}",
                p.current_file
            );
            let expected = p.completed_bytes.min(p.total_bytes) as f32 / p.total_bytes as f32;
            assert!(
                (p.fraction - expected).abs() < 1e-6,
                "fraction {p:?} != bytes/total"
            );
            assert!(
                p.detail.starts_with("Fichier "),
                "format de détail attendu, got {:?}",
                p.detail
            );
        }
        srv.abort();
    }

    #[tokio::test]
    async fn cdn_verify_streams_before_any_download() {
        // Non-régression « première minute morte » : monde stub où TOUS les
        // fichiers sont déjà présents — aucun octet à télécharger, seule la
        // vérification (re-hachage) travaille. La PREMIÈRE émission doit
        // arriver pendant cette vérification (avant tout événement de
        // téléchargement) et porter une ligne `Vérification i/n — nom` avec
        // un vrai nom de fichier. Un comportement silencieux (vérification
        // comptée sans émettre, tout différé après les totaux tardifs)
        // échoue ici : aucune émission, ou première émission groupée à
        // `completed_files == N` au lieu du `1` diffusé en continu.
        let bodies: std::collections::HashMap<String, Vec<u8>> = [
            ("/alpha.jar".to_string(), vec![0xA5u8; 8192]),
            ("/beta.jar".to_string(), vec![0x3Cu8; 4096]),
        ]
        .into_iter()
        .collect();
        let (base, srv) = stub_server(bodies.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let m = stub_manifest(
            &base,
            &[
                ("alpha.jar", &bodies["/alpha.jar"]),
                ("beta.jar", &bodies["/beta.jar"]),
            ],
        );
        // Tout est présent et frais : la boucle de vérification ne fait que
        // re-hacher, sans jamais toucher le réseau.
        for (name, body) in [
            ("alpha.jar", &bodies["/alpha.jar"]),
            ("beta.jar", &bodies["/beta.jar"]),
        ] {
            let dest = cdn_plan_for_mode(&m, dir.path(), false)
                .into_iter()
                .find(|p| p.file.name == name)
                .map(|p| p.dest)
                .unwrap();
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&dest, body).unwrap();
        }
        let (emit, seen) = recording_emit();
        let run = std::sync::Arc::new(InstallRun::new(emit));
        let summary = run_install(&m, dir.path(), &run, None, false)
            .await
            .expect("monde tout-présent se vérifie sans réseau");
        assert_eq!(
            (
                summary.total_files,
                summary.downloaded_files,
                summary.skipped_files
            ),
            (2, 0, 2),
            "tout est frais : zéro téléchargement"
        );
        let seen = seen.lock().unwrap();
        assert!(
            !seen.is_empty(),
            "la vérification doit émettre, pas compter en silence"
        );
        // Première émission = première fin de contrôle, pas un résumé groupé.
        let first = &seen[0];
        assert_eq!(
            first.completed_files, 1,
            "première émission en continu (1 fichier réglé), got {first:?}"
        );
        assert!(
            first.detail.starts_with("Vérification 1/"),
            "ligne de vérification attendue, got {:?}",
            first.detail
        );
        assert!(
            first.current_file.ends_with(".jar") && !first.current_file.is_empty(),
            "vrai nom de fichier attendu, got {:?}",
            first.current_file
        );
        assert!(
            first.detail.contains(&first.current_file),
            "le détail reprend le nom réel, got {:?}",
            first.detail
        );
        // Aucun téléchargement avant elle (ici : aucun du tout).
        assert!(
            seen.iter().all(|p| p.detail.starts_with("Vérification")),
            "monde tout-présent : que des lignes de vérification, got {:?}",
            seen.iter().map(|p| &p.detail).collect::<Vec<_>>()
        );
        srv.abort();
    }

    #[test]
    fn install_run_counters_are_unified_bytes() {
        // Le run additionne les phases : totaux enregistrés au plan, lignes
        // fichier au prorata des octets, actions sans fichier ni octets.
        let (emit, seen) = recording_emit();
        let run = InstallRun::new(emit);
        run.add_work(2, 300);
        run.add_work(1, 100);
        run.add_work(1, 0); // fichier à taille inconnue (compté, 0 octet)
        let base = run.base();
        assert_eq!(base, (0, 0));
        // Mise à jour locale (lot : 100/300 réglés, 1/3 fichiers).
        run.on_phase_update(
            base,
            &launcher_core::ProgressUpdate {
                completed_files: 1,
                total_files: 3,
                completed_bytes: 100,
                total_bytes: 300,
                current_file: "a.jar".to_string(),
                current_bytes: 60,
                current_total: 200,
                skipped: false,
            },
        );
        // Fin de lot : crédit des totaux planifiés.
        run.end_phase(300, 3);
        assert_eq!(run.base(), (300, 3));
        // Action sans fichier : compteurs inchangés, fichier vide.
        run.say("Extraction des natifs…");
        assert_eq!(run.base(), (300, 3));
        // Taille inconnue : done et total ensemble — la barre ne retombe
        // jamais (`(d+x)/(t+x) >= d/t` quand `d <= t`).
        let before = run.fraction();
        run.credit_unknown("mods/x.jar", 50);
        assert!(run.fraction() >= before);
        assert!(run.fraction() <= 1.0);
        // Clôture explicite.
        run.finish();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(
            seen[0].detail,
            format!(
                "Fichier 1/4 — a.jar — {} (25 %)",
                crate::state::format_progress_bytes(100, 400)
            )
        );
        assert!((seen[0].fraction - 0.25).abs() < 1e-6);
        assert_eq!(seen[1].current_file, "");
        assert_eq!(seen[1].detail, "Extraction des natifs…");
        assert!(seen[2].detail.contains("mods/x.jar"));
        assert_eq!(seen[3].fraction, 1.0);
        assert_eq!(seen[3].detail, "Jeu installé et vérifié.");
        assert!(seen[3].current_file.is_empty());
    }

    /// Fully offline `start_install` world (default flavor): session +
    /// in-memory stub manifest + seeded Mojang caches + fake Java 8 script.
    /// The only network is the loopback stub. With `missing_cdn` the CDN
    /// file 404s and the install fails (slow: bounded downloader retries).
    #[cfg(unix)]
    async fn offline_world(
        missing_cdn: bool,
    ) -> (SharedState, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use std::collections::HashMap;
        let mod_body = vec![0x11u8; 2048];
        let vanilla_body = vec![0x22u8; 1024];
        let index_body = br#"{"objects":{}}"#.to_vec();
        let mut bodies: HashMap<String, Vec<u8>> = HashMap::new();
        bodies.insert("/stub-mod.jar".to_string(), mod_body.clone());
        bodies.insert("/vanilla.jar".to_string(), vanilla_body.clone());
        bodies.insert("/index.json".to_string(), index_body.clone());
        if missing_cdn {
            bodies.remove("/stub-mod.jar");
        }
        let (base, srv) = stub_server(bodies).await;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(AppState::default()));
        let game_dir = {
            let mut s = state.lock().await;
            s.settings.game_dir = dir.path().to_string_lossy().into_owned();
            s.upsert_session(session());
            s.game_dir()
        };
        // CDN manifest (in-memory → `ensure_manifest` never hits the net).
        let m = stub_manifest(&base, &[("stub-mod.jar", &mod_body)]);
        // Mojang caches (vanilla jar via stub, empty asset objects).
        let caches = launcher_core::caches_dir(&game_dir);
        std::fs::create_dir_all(&caches).unwrap();
        std::fs::write(
            caches.join(launcher_core::MOJANG_CACHE_FILE),
            serde_json::json!({
                "id": "1.7.10",
                "libraries": [],
                "downloads": {"client": {
                    "sha1": launcher_core::sha1_hex(&vanilla_body),
                    "size": vanilla_body.len(),
                    "url": format!("{base}/vanilla.jar")}},
                "assetIndex": {"id": "1.7.10",
                    "sha1": launcher_core::sha1_hex(&index_body),
                    "size": index_body.len(),
                    "url": format!("{base}/index.json")},
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            caches.join(launcher_core::MOJANG_ASSET_INDEX_CACHE_FILE),
            &index_body,
        )
        .unwrap();
        // Fake Java 8 (bundled wins over JAVA_HOME/PATH → deterministic).
        let java_bin = game_dir.join("java").join("bin").join("java");
        std::fs::create_dir_all(java_bin.parent().unwrap()).unwrap();
        std::fs::write(
            &java_bin,
            "#!/bin/sh\necho 'openjdk version \"1.8.0_392\"'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&java_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        state.lock().await.manifest = Some(m);
        (state, dir, srv)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn start_install_completes_offline_with_progress() {
        let (state, _dir, srv) = offline_world(false).await;
        let game_dir = state.lock().await.game_dir();
        let (emit, seen) = recording_emit();
        let summary = start_install(&state, emit, false)
            .await
            .expect("offline stub install succeeds");
        assert_eq!((summary.total_files, summary.downloaded_files), (1, 1));
        assert!(!state.lock().await.install_running);
        // Destination directe `.jar`, aucun intermédiaire.
        assert!(game_dir.join("mods").join("stub-mod.jar").is_file());
        assert!(!game_dir.join("mods").join("stub-mod.pala").exists());
        assert!(game_dir
            .join("versions")
            .join("1.7.10")
            .join("1.7.10.jar")
            .is_file());
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|p| (0.0..=1.0).contains(&p.fraction)));
        assert!(seen.iter().any(|p| p.fraction == 1.0));
        srv.abort();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn full_install_never_emits_placeholder_filenames() {
        // Demande explicite : FICHIERS et ACTIONS, pas de bandes de phases.
        // Sur le chemin complet hors-ligne, `current_file` ne porte qu'un
        // VRAI nom de fichier ou `""` (actions) — jamais un mot de phase — et
        // chaque fraction vaut octets/total de sa propre émission.
        let (state, _dir, srv) = offline_world(false).await;
        let (emit, seen) = recording_emit();
        start_install(&state, emit, false)
            .await
            .expect("offline stub install succeeds");
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty());
        const BANNED: [&str; 8] = [
            "mojang", "modern", "java21", "java", "terminé", "mods", "flavor", "bytecode",
        ];
        for p in seen.iter() {
            assert!(
                !BANNED.contains(&p.current_file.as_str()),
                "mot de phase comme nom de fichier : {p:?}"
            );
            if p.total_bytes > 0 {
                let expected = p.completed_bytes.min(p.total_bytes) as f32 / p.total_bytes as f32;
                assert!(
                    (p.fraction - expected).abs() < 1e-6,
                    "fraction {p:?} != octets/total"
                );
            }
            // Lignes fichier au format stable (nom repris), lignes d'action
            // sans fichier mais jamais vides.
            if p.current_file.is_empty() {
                assert!(!p.detail.is_empty(), "action muette : {p:?}");
            } else {
                assert!(
                    p.detail.contains(&p.current_file) || p.detail.starts_with("Vérification"),
                    "détail {p:?} sans nom de fichier"
                );
            }
        }
        // Clôture explicite : 1.0 + ligne, sans nom de fichier.
        let last = seen.last().unwrap();
        assert_eq!(last.fraction, 1.0);
        assert_eq!(last.detail, "Jeu installé et vérifié.");
        assert!(last.current_file.is_empty());
        srv.abort();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn failed_start_install_releases_slot_and_stays_reusable() {
        // Regression: a failed run must never wedge `install_running`
        // (previously any `ensure_manifest` failure poisoned every later
        // install AND play with zero progress until app restart).
        let (state, _dir, srv) = offline_world(true).await;
        let err = start_install(&state, None, false).await.unwrap_err();
        assert!(
            !err.contains("déjà en cours"),
            "slot wedged instead of real error: {err}"
        );
        assert!(!state.lock().await.install_running);
        // A retry hits the same real download error — never the wedge.
        let err2 = start_install(&state, None, false).await.unwrap_err();
        assert!(
            !err2.contains("déjà en cours") && !err2.contains("en cours"),
            "second run wedged: {err2}"
        );
        assert!(!state.lock().await.install_running);
        srv.abort();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn exec_modern_phase_provisions_offline_from_seed() {
        // Headless reproduction of the experimental install: seeded profile
        // cache (one lib WITH path, one guava-shaped lib WITHOUT — the live
        // lwjgl3ify shape that used to kill the whole step with
        // `missing field 'path'`), fake Java 21, pre-seeded coremods.
        // No external network may be touched.
        use std::collections::HashMap;
        let red_body = vec![0x52u8; 1024];
        let guava_body = vec![0x47u8; 512];
        let mut bodies: HashMap<String, Vec<u8>> = HashMap::new();
        bodies.insert("/red-1.0.jar".to_string(), red_body.clone());
        bodies.insert("/guava-17.0.jar".to_string(), guava_body.clone());
        let (base, srv) = stub_server(bodies).await;
        let dir = tempfile::tempdir().unwrap();
        let game_dir = launcher_core::normalize_game_dir(dir.path());
        let caches = launcher_core::caches_dir(&game_dir);
        std::fs::create_dir_all(&caches).unwrap();
        std::fs::write(
            caches.join(launcher_core::MODERN_CACHE_FILE),
            serde_json::json!({
                "id": "seed",
                "libraries": [
                    {"name": "org.example:red:1.0",
                     "downloads": {"artifact": {
                         "path": "org/example/red/1.0/red-1.0.jar",
                         "sha1": launcher_core::sha1_hex(&red_body),
                         "size": red_body.len(),
                         "url": format!("{base}/red-1.0.jar")}}},
                    {"name": "com.google.guava:guava:17.0",
                     "downloads": {"artifact": {
                         "sha1": launcher_core::sha1_hex(&guava_body),
                         "size": guava_body.len(),
                         "url": format!("{base}/guava-17.0.jar")}}}
                ]
            })
            .to_string(),
        )
        .unwrap();
        // Fake Java 21 (valid major → GraalVM provision skipped, offline).
        let java_bin = game_dir.join("java21").join("bin").join("java");
        std::fs::create_dir_all(java_bin.parent().unwrap()).unwrap();
        std::fs::write(&java_bin, "#!/bin/sh\necho 'openjdk version \"21.0.1\"'\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&java_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Coremods already present (size-checked skip → no GitHub fetch).
        let mods = launcher_core::mods_dir(&game_dir);
        std::fs::create_dir_all(&mods).unwrap();
        for name in [
            "lwjgl3ify.jar",
            launcher_core::MODERN_UNIMIXINS_FILE,
            launcher_core::MODERN_COMPAT_FILE,
        ] {
            std::fs::write(mods.join(name), b"x").unwrap();
        }
        let (emit, seen) = recording_emit();
        let run = std::sync::Arc::new(InstallRun::new(emit));
        let client = http_client().expect("client HTTP");
        let plan = plan_modern_phase(&run, &client, &game_dir, false)
            .await
            .expect("plan expérimental");
        let report = exec_modern_phase(&run, &client, &game_dir, plan, None, false)
            .await
            .expect("seeded experimental provision succeeds");
        assert_eq!(report.libs_downloaded, 2);
        assert_eq!(report.coremods_downloaded, 0);
        assert!(!report.java21_provisioned);
        // Derived maven layout for the path-less guava (never the libs root).
        assert!(game_dir
            .join("libraries/com/google/guava/guava/17.0/guava-17.0.jar")
            .is_file());
        assert!(game_dir
            .join("libraries/org/example/red/1.0/red-1.0.jar")
            .is_file());
        // Coremod dummies untouched (no re-download).
        for name in [
            "lwjgl3ify.jar",
            launcher_core::MODERN_UNIMIXINS_FILE,
            launcher_core::MODERN_COMPAT_FILE,
        ] {
            assert_eq!(std::fs::read(mods.join(name)).unwrap(), b"x");
        }
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|p| (0.0..=1.0).contains(&p.fraction)));
        assert!(seen.iter().any(|p| p.fraction == 1.0));
        srv.abort();
    }

    fn assert_french_no_emoji(line: &str) {
        assert!(!line.is_empty());
        assert!(
            line.chars()
                .all(|c| !(('\u{1F300}'..='\u{1FAFF}').contains(&c)
                    || ('\u{2600}'..='\u{27BF}').contains(&c)
                    || ('\u{FE00}'..='\u{FE0F}').contains(&c))),
            "pas d'emoji : {line}"
        );
    }

    #[tokio::test]
    async fn set_pack_flavor_swaps_stack_and_persists_flags() {
        let (state, _dir) = state_with_tmp();
        let game_dir = state.lock().await.game_dir();
        let mods = launcher_core::mods_dir(&game_dir);
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("lwjgl3ify.jar"), b"y").unwrap();
        std::fs::write(mods.join(launcher_core::MODERN_OPT_BEDDIUM_FILE), b"x").unwrap();
        std::fs::write(
            mods.join(format!(
                "{}.disabled",
                launcher_core::MODERN_OPT_COMPUTIUM_FILE
            )),
            b"z",
        )
        .unwrap();
        std::fs::write(mods.join("PalaMod-9.5.0.jar"), b"pack").unwrap();
        // Bascule classique : drapeaux persistés, pile mise de côté, une
        // ligne Console française par déplacement. Chemin non verrouillé
        // (`gated = false` = interrupteur réactivé : le mécanisme de
        // bascule reste couvert quel que soit l'état du verrou).
        let swap = set_pack_flavor_gated(&state, false, false).await.unwrap();
        assert!(!swap.experimental);
        assert_eq!(swap.moved.len(), 3);
        assert_eq!(swap.lines.len(), 3);
        for line in &swap.lines {
            assert!(line.starts_with("Classique : "), "unexpected: {line}");
            assert!(line.contains("mis de côté"), "unexpected: {line}");
            assert_french_no_emoji(line);
        }
        assert!(!get_settings(&state).await.experimental_runtime);
        assert!(!get_settings(&state).await.experimental_optimizations);
        let stash = launcher_core::modern_stash_dir(&game_dir);
        assert!(stash.join("lwjgl3ify.jar").is_file());
        assert!(stash
            .join(format!(
                "{}.disabled",
                launcher_core::MODERN_OPT_COMPUTIUM_FILE
            ))
            .is_file());
        assert!(mods.join("PalaMod-9.5.0.jar").is_file());
        // Idempotent : second appel silencieux.
        let again = set_pack_flavor_gated(&state, false, false).await.unwrap();
        assert!(again.moved.is_empty());
        assert!(again.lines.is_empty());
        // Bascule expérimentale : tout revient, sans réseau.
        let back = set_pack_flavor_gated(&state, true, false).await.unwrap();
        assert!(back.experimental);
        assert_eq!(back.moved.len(), 3);
        for line in &back.lines {
            assert!(line.starts_with("Expérimental : "), "unexpected: {line}");
            assert!(line.contains("restauré"), "unexpected: {line}");
            assert_french_no_emoji(line);
        }
        assert!(get_settings(&state).await.experimental_runtime);
        assert!(mods.join("lwjgl3ify.jar").is_file());
        assert!(mods.join(launcher_core::MODERN_OPT_BEDDIUM_FILE).is_file());
        // Le registre des comptes survit (même fusion que `save_settings`).
        assert!(get_settings(&state).await.accounts.is_empty());
    }

    #[tokio::test]
    async fn set_pack_flavor_gated_rejects_experimental_keeps_default() {
        let (state, _dir) = state_with_tmp();
        // Verrou actif (constante) : l'expérimental est refusé en français,
        // sans emoji, sans toucher aux drapeaux ni au disque.
        let err = set_pack_flavor(&state, true).await.expect_err("gated");
        assert!(err.contains("temporairement"), "unexpected: {err}");
        assert_french_no_emoji(&err);
        assert!(!get_settings(&state).await.experimental_runtime);
        assert!(!get_settings(&state).await.experimental_optimizations);
        // La branche par défaut reste commutable sous verrou.
        let swap = set_pack_flavor(&state, false).await.unwrap();
        assert!(!swap.experimental);
        assert!(swap.moved.is_empty());
        // Les deux positions de l'interrupteur via le chemin paramétré :
        // verrouillé = refus, déverrouillé = comportement antérieur.
        assert!(set_pack_flavor_gated(&state, true, true).await.is_err());
        assert!(set_pack_flavor_gated(&state, true, false).await.is_ok());
    }

    #[tokio::test]
    async fn get_boot_notices_drains_once() {
        let (state, _dir) = state_with_tmp();
        assert!(get_boot_notices(&state).await.is_empty());
        state
            .lock()
            .await
            .boot_notices
            .push("Ligne de démarrage.".to_string());
        assert_eq!(
            get_boot_notices(&state).await,
            vec!["Ligne de démarrage.".to_string()]
        );
        assert!(get_boot_notices(&state).await.is_empty());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn start_install_stashes_modern_pollution_on_classic() {
        let (state, _dir, srv) = offline_world(false).await;
        let game_dir = state.lock().await.game_dir();
        let mods = launcher_core::mods_dir(&game_dir);
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("lwjgl3ify.jar"), b"pollution").unwrap();
        std::fs::write(
            mods.join(launcher_core::MODERN_OPT_COMPUTIUM_FILE),
            b"pollution",
        )
        .unwrap();
        let (emit, seen) = recording_emit();
        start_install(&state, emit, false)
            .await
            .expect("offline stub install succeeds");
        // La pile moderne est mise de côté (pas supprimée), le dossier mods
        // ne garde que le contenu CDN.
        let stash = launcher_core::modern_stash_dir(&game_dir);
        assert_eq!(
            std::fs::read(stash.join("lwjgl3ify.jar")).unwrap(),
            b"pollution"
        );
        assert!(stash
            .join(launcher_core::MODERN_OPT_COMPUTIUM_FILE)
            .is_file());
        assert!(!mods.join("lwjgl3ify.jar").exists());
        assert!(mods.join("stub-mod.jar").is_file());
        // Chaque déplacement est journalisé en français sur le canal progrès.
        let seen = seen.lock().unwrap();
        let details: Vec<&str> = seen.iter().map(|p| p.detail.as_str()).collect();
        assert!(
            details.iter().any(|d| d.contains("mis de côté")),
            "lignes françaises attendues : {details:?}"
        );
        srv.abort();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn play_heals_mixed_mods_without_explicit_switch() {
        // Monde entièrement frais (aucune réinstallation) mais `mods/`
        // pollué par la pile moderne : JOUER met de côté et lance, sans
        // exiger de bascule explicite.
        let (state, _dir, srv) = offline_world(false).await;
        let game_dir = state.lock().await.game_dir();
        let manifest = state.lock().await.manifest.clone().unwrap();
        launcher_core::save_manifest(&launcher_core::cdn_last_path(&game_dir), &manifest).unwrap();
        // Runtime Mojang frais (contrôles disque uniquement, sans réseau).
        let versions = game_dir.join("versions").join("1.7.10");
        std::fs::create_dir_all(&versions).unwrap();
        std::fs::write(versions.join("1.7.10.jar"), b"vanilla").unwrap();
        let natives = game_dir.join("natives");
        std::fs::create_dir_all(&natives).unwrap();
        std::fs::write(natives.join("liblwjgl.so"), b"natif").unwrap();
        let forge_dest = launcher_core::forge_universal_dest(&game_dir);
        std::fs::create_dir_all(forge_dest.parent().unwrap()).unwrap();
        std::fs::write(&forge_dest, b"forge").unwrap();
        launcher_core::write_installed_flavor(&game_dir, false);
        // `mods/` : le mod CDN attendu + la pollution moderne (état du crash
        // actuel : ces jars détourneraient la chaîne Mixin/ASM de Java 8).
        let mods = launcher_core::mods_dir(&game_dir);
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("stub-mod.jar"), vec![0x11u8; 2048]).unwrap();
        std::fs::write(mods.join("lwjgl3ify.jar"), b"pollution").unwrap();
        std::fs::write(
            mods.join(launcher_core::MODERN_OPT_BEDDIUM_FILE),
            b"pollution",
        )
        .unwrap();
        // Console d'enregistrement : chaque déplacement parle français.
        let lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe = lines.clone();
        let console: Option<Arc<ConsoleEmitter>> = Some(Arc::new(move |l: String| {
            probe.lock().unwrap().push(l);
        }));
        play(&state, None, console)
            .await
            .expect("play heals then spawns");
        // Dossier guéri : la pile moderne est en réserve, le jeu démarre sur
        // un `mods/` propre (le faux binaire Java quitte aussitôt, le spawn
        // détaché réussit).
        let stash = launcher_core::modern_stash_dir(&game_dir);
        assert!(stash.join("lwjgl3ify.jar").is_file());
        assert!(stash.join(launcher_core::MODERN_OPT_BEDDIUM_FILE).is_file());
        assert!(!mods.join("lwjgl3ify.jar").exists());
        assert!(mods.join("stub-mod.jar").is_file());
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 2, "deux déplacements : {lines:?}");
        for line in lines.iter() {
            assert!(line.starts_with("Classique : "), "unexpected: {line}");
            assert!(line.contains("mis de côté"), "unexpected: {line}");
            assert_french_no_emoji(line);
        }
        srv.abort();
    }

    #[tokio::test]
    async fn loki_jar_on_disk_lists_as_optional_toggleable() {
        let (state, dir) = state_with_tmp();
        state.lock().await.manifest = Some(manifest());
        let mods_dir = launcher_core::mods_dir(&state.lock().await.game_dir());
        let _dir = dir;
        std::fs::create_dir_all(&mods_dir).unwrap();
        std::fs::write(mods_dir.join(LOKI_FILE_NAME), b"fake-loki").unwrap();
        // Not in the fixture manifest → optional + toggleable, never required.
        let mods = get_mods_list(&state).await.unwrap();
        let loki = mods.iter().find(|m| m.id == LOKI_FILE_NAME).unwrap();
        assert!(!loki.required, "loki.jar must stay optional");
        assert!(loki.enabled);
        let off = toggle_mod(&state, LOKI_FILE_NAME.to_string(), false)
            .await
            .unwrap();
        assert!(!off.enabled);
        assert!(mods_dir
            .join(format!("{LOKI_FILE_NAME}{DISABLED_SUFFIX}"))
            .is_file());
        let on = toggle_mod(&state, LOKI_FILE_NAME.to_string(), true)
            .await
            .unwrap();
        assert!(on.enabled);
    }

    // ------------------------------------------------------------------
    // Garde contenu JOUER : `missing_content_files` + comblement ciblé
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn play_ensure_heals_missing_mod_without_full_rehash() {
        // Monde « version OK » amputé d'un mod : la garde contenu signale
        // stale (présence seule), le comblement ciblé restaure l'octet exact
        // sans toucher au présent — même corrompu (pas de re-hachage, reste
        // du ressort de Vérifier/install, choix documenté).
        let present_body = vec![0xABu8; 1024];
        let missing_body = vec![0xCDu8; 512];
        let bodies: std::collections::HashMap<String, Vec<u8>> = [
            ("/present.jar".to_string(), present_body.clone()),
            ("/missing.jar".to_string(), missing_body.clone()),
        ]
        .into_iter()
        .collect();
        let (base, srv) = stub_server(bodies).await;
        let dir = tempfile::tempdir().unwrap();
        let m = stub_manifest(
            &base,
            &[
                ("present.jar", &present_body),
                ("missing.jar", &missing_body),
            ],
        );
        let mods = launcher_core::mods_dir(dir.path());
        std::fs::create_dir_all(&mods).unwrap();
        // Présent mais corrompu : le comblement ne doit pas le réparer.
        let wrong = vec![0x00u8; 1024];
        std::fs::write(mods.join("present.jar"), &wrong).unwrap();
        assert!(cdn_content_stale(&m, dir.path(), false));
        let missing = missing_content_files(&m, dir.path(), false);
        assert_eq!(
            missing
                .iter()
                .map(|p| p.file.name.as_str())
                .collect::<Vec<_>>(),
            vec!["missing.jar"]
        );
        let summary = {
            let run = std::sync::Arc::new(InstallRun::new(None));
            run.add_work(missing.len(), plan_bytes(&missing));
            run_planned(missing, &run, None)
                .await
                .expect("comblement ciblé réussit")
        };
        assert_eq!((summary.total_files, summary.downloaded_files), (1, 1));
        assert_eq!(
            std::fs::read(mods.join("missing.jar")).unwrap(),
            missing_body,
            "mod guéri à l'octet exact"
        );
        assert_eq!(
            std::fs::read(mods.join("present.jar")).unwrap(),
            wrong,
            "présent corrompu intouché (pas de re-hachage)"
        );
        assert!(!cdn_content_stale(&m, dir.path(), false));
        srv.abort();
    }

    #[tokio::test]
    async fn play_ensure_respects_disabled_twins() {
        // Jumeau `.disabled` seul sur disque : PAS stale, jamais
        // re-téléchargé (mod volontairement désactivé).
        let body = vec![0xEEu8; 256];
        let dir = tempfile::tempdir().unwrap();
        let m = stub_manifest("http://example.invalid", &[("optmod.jar", &body)]);
        let mods = launcher_core::mods_dir(dir.path());
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join(format!("optmod.jar{DISABLED_SUFFIX}")), &body).unwrap();
        assert!(!cdn_content_stale(&m, dir.path(), false));
        assert!(missing_content_files(&m, dir.path(), false).is_empty());
        let summary = {
            let run = std::sync::Arc::new(InstallRun::new(None));
            run_planned(Vec::new(), &run, None).await.unwrap()
        };
        assert_eq!(
            (
                summary.total_files,
                summary.downloaded_files,
                summary.skipped_files
            ),
            (0, 0, 0)
        );
        assert!(!mods.join("optmod.jar").exists());
        assert_eq!(
            std::fs::read(mods.join(format!("optmod.jar{DISABLED_SUFFIX}"))).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn play_ensure_never_requires_declined_loki() {
        // Loki décliné et absent : pas de stale, jamais téléchargé de force
        // (JOUER ne porte aucun opt-in — voir `start_install`).
        let mod_body = vec![0x11u8; 128];
        let loki_body = vec![0x22u8; 64];
        let dir = tempfile::tempdir().unwrap();
        let m = stub_manifest(
            "http://example.invalid",
            &[("pack-mod.jar", &mod_body), (LOKI_FILE_NAME, &loki_body)],
        );
        let mods = launcher_core::mods_dir(dir.path());
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("pack-mod.jar"), &mod_body).unwrap();
        assert!(!cdn_content_stale(&m, dir.path(), false));
        assert!(missing_content_files(&m, dir.path(), false).is_empty());
    }

    #[test]
    fn content_check_is_mode_aware_and_ignores_stash() {
        // Plan mode-actif : classique exige le mod `forge-*`, expérimental
        // l'exclut (même filtre que `cdn_plan_for_mode` — jamais d'exigence
        // inter-saveur) ; les bibliothèques restent du ressort de la garde
        // libs, pas d'ici. La réserve `modstack-experimental/` ne satisfait
        // RIEN dans les deux sens.
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "net.minecraft.launchwrapper.Launch",
            "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "mods", "dest": "mods"},
                {"name": "libraries-windows", "dest": "libraries",
                 "properties": {"classpath": true}, "os": ["WINDOWS"]}
            ],
            "files": [
                {"name": "pack-mod.jar", "model": "mods",
                 "url": "https://x/pack-mod.jar", "size": 4, "sha1": "aa",
                 "path": "pack-mod.jar", "os": []},
                {"name": "forge-side-mod.jar", "model": "mods",
                 "url": "https://x/forge-side-mod.jar", "size": 5, "sha1": "bb",
                 "path": "forge-side-mod.jar", "os": []},
                {"name": "somelib-1.0.jar", "model": "libraries-windows",
                 "url": "https://x/somelib.jar", "size": 3, "sha1": "cc",
                 "path": "org/example/somelib-1.0.jar", "os": ["WINDOWS"]}
            ]
        }"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let mods = launcher_core::mods_dir(game);
        std::fs::create_dir_all(&mods).unwrap();
        // Seul le mod du pack présent : classique exige aussi le `forge-*`,
        // expérimental l'a hors plan (et la lib reste hors champ ici).
        std::fs::write(mods.join("pack-mod.jar"), b"pack").unwrap();
        assert!(
            cdn_content_stale(&m, game, false),
            "classique : forge-side-mod manquant → stale"
        );
        assert!(
            !cdn_content_stale(&m, game, true),
            "expérimental : forge-side-mod hors plan → frais"
        );
        // Les deux mods présents : frais des deux côtés (la lib manquante
        // reste du ressort de la garde bibliothèques).
        std::fs::write(mods.join("forge-side-mod.jar"), b"forge").unwrap();
        assert!(!cdn_content_stale(&m, game, false));
        assert!(!cdn_content_stale(&m, game, true));
        // Réserve expérimentale : ne satisfait RIEN, dans les deux sens.
        std::fs::remove_file(mods.join("pack-mod.jar")).unwrap();
        std::fs::remove_file(mods.join("forge-side-mod.jar")).unwrap();
        let stash = launcher_core::modern_stash_dir(game);
        std::fs::create_dir_all(&stash).unwrap();
        std::fs::write(stash.join("pack-mod.jar"), b"pack").unwrap();
        std::fs::write(stash.join("forge-side-mod.jar"), b"forge").unwrap();
        assert!(
            cdn_content_stale(&m, game, false),
            "réserve ≠ mods/ (classique)"
        );
        assert!(
            cdn_content_stale(&m, game, true),
            "réserve ≠ mods/ (expérimental)"
        );
    }

    #[tokio::test]
    async fn complete_world_is_fast_noop_without_network() {
        // Monde complet : liste vide, comblement no-op — les URL sont
        // injoignables (`example.invalid`), donc tout accès réseau échouerait.
        let one = vec![0xA5u8; 64];
        let two = vec![0x3Cu8; 32];
        let dir = tempfile::tempdir().unwrap();
        let m = stub_manifest(
            "http://example.invalid",
            &[("one.jar", &one), ("two.jar", &two)],
        );
        let mods = launcher_core::mods_dir(dir.path());
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("one.jar"), &one).unwrap();
        std::fs::write(mods.join("two.jar"), &two).unwrap();
        assert!(!cdn_content_stale(&m, dir.path(), false));
        assert!(!cdn_content_stale(&m, dir.path(), true));
        let summary = {
            let missing = missing_content_files(&m, dir.path(), false);
            let run = std::sync::Arc::new(InstallRun::new(None));
            run.add_work(missing.len(), plan_bytes(&missing));
            run_planned(missing, &run, None).await.unwrap()
        };
        assert_eq!(
            (
                summary.total_files,
                summary.downloaded_files,
                summary.skipped_files
            ),
            (0, 0, 0)
        );
    }

    // ------------------------------------------------------------------
    // Sondage `En jeu` : marqueur de ligne de commande + PID enregistré
    // ------------------------------------------------------------------

    #[test]
    fn cmdline_marker_matches_game_only() {
        let game = Path::new("/home/u/.46anarchy/paladium");
        // Classe principale launchwrapper : marqueur, quel que soit le dir.
        assert!(cmdline_is_game(
            &[
                "java".to_string(),
                "net.minecraft.launchwrapper.Launch".to_string(),
            ],
            Path::new("/ailleurs/jeu"),
        ));
        // Chemin sous le game dir (classpath du lancement) : marqueur.
        assert!(cmdline_is_game(
            &[
                "java".to_string(),
                "-cp".to_string(),
                "/home/u/.46anarchy/paladium/mods/PalaMod-9.5.0.jar".to_string(),
            ],
            game,
        ));
        // Séparateurs Windows + casse : normalisés avant comparaison.
        assert!(cmdline_is_game(
            &["C:\\Users\\u\\.46anarchy\\PALADIUM\\mods\\x.jar".to_string()],
            Path::new("C:/Users/u/.46anarchy/paladium"),
        ));
        // Processus étrangers : jamais le jeu (garde anti recyclage de PID).
        assert!(!cmdline_is_game(
            &["sleep".to_string(), "60".to_string()],
            game
        ));
        assert!(!cmdline_is_game(&[], game));
        assert!(!cmdline_is_game(
            &[
                "java".to_string(),
                "-jar".to_string(),
                "autre.jar".to_string()
            ],
            game,
        ));
    }

    #[test]
    fn game_status_never_reports_foreign_process() {
        let probe = tempfile::tempdir().unwrap();
        let game = probe.path().join("46anarchy-game-status-probe-paladium");
        // PID absent/nul : inconnu → fermé.
        assert!(!game_process_running(None, &game));
        assert!(!game_process_running(Some(0), &game));
        // PID inexistant : inconnu → fermé.
        assert!(!game_process_running(Some(u32::MAX), &game));
        // Positif vivant SANS marqueur : notre propre processus existe mais sa
        // ligne (harness de test) ne porte aucun marqueur → `false`, ce qui
        // prouve que la garde marqueur fonctionne (pas de simple `alive`).
        let me = std::process::id();
        assert_ne!(me, 0);
        assert!(
            !game_process_running(Some(me), &game),
            "le processus courant vivant sans marqueur ne doit pas passer pour le jeu"
        );
    }

    #[test]
    #[cfg(unix)]
    fn sysinfo_probe_voit_un_enfant_reel() {
        // Sonde du maillon non vérifié de la chaîne « En jeu » : un enfant
        // réel de courte vie (`sleep`) est-il visible via sysinfo DANS cet
        // environnement (conteneur) ? Les deux stratégies sont sondées et
        // journalisées ; le test exige les deux (constat versé au rapport).
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep réussi");
        let pid = child.id();
        assert_ne!(pid, 0);
        let sys_pid = sysinfo::Pid::from_u32(pid);
        // (a) balayage complet.
        let mut plein = sysinfo::System::new();
        plein.refresh_processes();
        let vu_plein = plein.process(sys_pid).is_some();
        // (b) rafraîchissement singulier.
        let mut seul = sysinfo::System::new();
        let rafraichi = seul.refresh_process(sys_pid);
        let vu_seul = seul.process(sys_pid).is_some();
        eprintln!(
            "[sonde sysinfo] pid={pid} balayage={vu_plein} singulier={vu_seul} (refresh_process->{rafraichi})"
        );
        child.kill().ok();
        let _ = child.wait();
        assert!(
            vu_plein,
            "balayage refresh_processes()+process() aveugle (pid {pid})"
        );
        assert!(
            vu_seul,
            "singulier refresh_process()+process() aveugle (pid {pid})"
        );
    }

    #[test]
    #[cfg(unix)]
    fn game_process_running_rejects_live_child_without_marker() {
        // Enfant réel vivant SANS marqueur : la sonde le voit (voir la
        // sonde ci-dessus) mais la garde marqueur le refuse — un PID vivant
        // recyclé ne passe jamais pour le jeu.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep réussi");
        let pid = child.id();
        assert_ne!(pid, 0);
        let probe = tempfile::tempdir().unwrap();
        let game = probe.path().join("46anarchy-sans-marqueur-paladium");
        assert!(
            !game_process_running(Some(pid), &game),
            "un enfant vivant sans marqueur ne doit pas passer pour le jeu"
        );
        child.kill().ok();
        let _ = child.wait();
    }

    #[test]
    #[cfg(unix)]
    fn game_process_running_voit_un_enfant_marquee_vivant() {
        // Régression du repli « En jeu » : un enfant vivant dont la ligne
        // porte le marqueur (chemin sous `game_dir`, comme le classpath du
        // lancement réel) est vu par `game_process_running` ; après sa mort
        // le même PID vaut `false`. Les sucres sysinfo par défaut ne
        // chargent jamais la ligne de commande (`cmd()` vide) et faisaient
        // retomber ce cas à `false` — ce test verrouille la correction.
        let probe = tempfile::tempdir().unwrap();
        let game = probe.path().join("46anarchy-avec-marqueur-paladium");
        std::fs::create_dir_all(&game).unwrap();
        let marqueur = game.join("bin").join("faux-jeu").display().to_string();
        let mut enfant = std::process::Command::new("bash")
            .args(["-c", "exec -a \"$0\" sleep 30", &marqueur])
            .spawn()
            .expect("spawn enfant réussi");
        let pid = enfant.id();
        assert_ne!(pid, 0);
        // L'enfant vient de naître : sur une machine chargée (CI 2 cœurs),
        // /proc/<pid>/cmdline peut être vide le temps de l'exec — réessayer
        // avec échéance au lieu d'exiger une visibilité immédiate.
        let debut = std::time::Instant::now();
        while !game_process_running(Some(pid), &game) {
            if debut.elapsed() > std::time::Duration::from_secs(10) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            game_process_running(Some(pid), &game),
            "un enfant vivant marqué doit passer pour le jeu (pid {pid})"
        );
        enfant.kill().ok();
        let _ = enfant.wait();
        assert!(
            !game_process_running(Some(pid), &game),
            "un enfant tué ne doit plus passer pour le jeu (pid {pid})"
        );
    }

    #[tokio::test]
    async fn game_status_defaults_to_stopped_without_spawn() {
        // Sans lancement cette session : `{ running: false }`, jamais d'erreur.
        let (state, _dir) = state_with_tmp();
        assert!(!get_game_status(&state).await.running);
        // Sondages stables : aucun basculement enregistré (silencieux, le
        // journal ne prouve que les transitions).
        assert!(!get_game_status(&state).await.running);
        assert!(!state.lock().await.last_game_running);
    }

    #[test]
    fn game_status_payload_contract_pins_running() {
        // Contrat frontend/backend du sondage `get_game_status` : `main.js`
        // `pollGameStatus` lit `st.running` (minuscule — un renommage fige
        // silencieusement le bouton « En jeu »).
        let value = serde_json::to_value(&crate::state::GameStatusDto { running: true }).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 1, "aucun champ parasite : {obj:?}");
        assert_eq!(obj["running"], serde_json::json!(true));
        let back: crate::state::GameStatusDto =
            serde_json::from_str(r#"{"running":false}"#).unwrap();
        assert!(!back.running);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_detached_game_returns_recordable_pid() {
        // Le PID du spawn est connu même en détaché (`Child::id`) pour
        // l'enregistrer dans l'état (`get_game_status` le sonde ensuite).
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("launcher-console.log");
        let pid = spawn_detached_game(
            &["sh".to_string(), "-c".to_string(), "exit 0".to_string()],
            dir.path(),
            &log,
            None,
            false,
            None,
            &[],
        )
        .expect("spawn sh réussi");
        assert_ne!(pid, 0);
    }
}
