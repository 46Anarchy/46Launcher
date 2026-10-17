// SPDX-License-Identifier: AGPL-3.0-only

//! Shared Tauri backend state (no WebView types here, so `cargo test` stays headless).
//!
//! [`AppState`] is the single `tokio::sync::Mutex`-guarded value owned by the
//! Tauri app: launcher [`SettingsDto`], in-memory Microsoft [`MinecraftSession`]
//! (`None` = logged out, there is no offline mode), the fresh [`Manifest`] and
//! the pending device-code flow. [`commands`](crate::commands) implements the
//! `invoke` surface on top of it; the WebView wiring lives in
//! [`tauri_app`](crate::tauri_app) (feature `tauri-backend` only).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize},
    Arc,
};

use launcher_auth::{AuthConfig, DeviceCodeResponse, MinecraftSession, PLACEHOLDER_CLIENT_ID};
use launcher_core::{
    build_full_command, compare_manifests, default_game_dir, load_manifest, normalize_game_dir,
    quote_command, LaunchProfile, Manifest, Os, ProgressUpdate, UpdateStatus, UserType,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Settings file name inside the game dir (also read by
/// [`AuthConfig::from_settings_file`] for the `client_id` override).
pub const SETTINGS_FILE_NAME: &str = "launcher-settings.json";
/// Prefix of every "login mandatory" backend error; the frontend detects it
/// with [`is_login_required_error`] and opens the login modal.
pub const LOGIN_REQUIRED_CODE: &str = "LOGIN_REQUIRED";
/// Human part of the login-mandatory error (French UI copy).
pub const LOGIN_REQUIRED_MSG: &str = "Connexion Microsoft requise pour jouer";
/// Display fallback when no manifest has been fetched yet.
pub const APP_NAME_FALLBACK: &str = "46Anarchy";
/// Server address shown on the home screen.
pub const SERVER_ADDRESS: &str = "mc.46anarchy.fr";
/// Community Discord.
pub const DISCORD_URL: &str = "https://discord.46anarchy.fr";

/// Build the `LOGIN_REQUIRED` backend error (mandatory login gate).
pub fn login_required_error() -> String {
    format!("{LOGIN_REQUIRED_CODE}: {LOGIN_REQUIRED_MSG}")
}

/// Session freshness for launch: the Minecraft token lives ~24h, so a
/// session older than this relaunches the refresh chain silently instead
/// of launching with a dead token (Prism behavior).
pub const SESSION_MAX_AGE_SECS: u64 = 20 * 3600;

/// `true` when a backend error string is the login-mandatory gate.
pub fn is_login_required_error(e: &str) -> bool {
    e.starts_with(LOGIN_REQUIRED_CODE)
}

/// Nom de la variable d'environnement activant les journaux de routine
/// (lignes avatar, frontières de phases d'installation, verdicts
/// `running`/`exited-0`, restauration de session réussie…).
pub const VERBOSE_ENV_VAR: &str = "LAUNCHER_VERBOSE";

/// Millisecondes murales (horodatage bon marché des lignes de démarrage :
/// un appel système par ligne, aucune allocation — même horloge que les
/// journaux de phases d'installation).
pub fn boot_wall_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `true` quand les journaux de routine sont activés (`LAUNCHER_VERBOSE`
/// définie, non vide et différente de `"0"`). Résolu une seule fois par
/// processus : quand désactivé, chaque appel se réduit à une lecture
/// atomique (coût nul — ni formatage ni appel système). Les erreurs, les
/// plantages et les états anormaux s'impriment toujours, hors de ce gate.
pub fn verbose_logging_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var_os(VERBOSE_ENV_VAR)
            .and_then(|v| v.into_string().ok())
            .is_some_and(|s| !s.trim().is_empty() && s.trim() != "0")
    })
}

// ---------------------------------------------------------------------------
// Pack flavor gate (experimental branch)
// ---------------------------------------------------------------------------

/// Master switch for the experimental branch (LWJGL3 + Java 21): `true`
/// keeps the entry VISIBLE but disabled everywhere (install-modal picker,
/// `set_pack_flavor`, boot migration) while every modern-stack module and
/// test stays in the tree. Flip to `false` to re-enable the branch — no
/// other edit needed (the frontend mirrors this constant, see
/// `EXPERIMENTAL_GATED` in `frontend/dist/main.js`).
pub const EXPERIMENTAL_GATED: bool = false;
/// French refusal for `set_pack_flavor(experimental = true)` while gated.
pub const EXPERIMENTAL_GATED_MSG: &str = "La branche expérimentale est temporairement désactivée — le modpack Paladium reste disponible.";
/// French one-shot boot notice when the stored settings said experimental
/// (the user is moved back to default, nothing else is wiped).
pub const EXPERIMENTAL_GATED_MIGRATION_MSG: &str = "Branche expérimentale temporairement désactivée — retour au modpack Paladium (aucune donnée supprimée).";

/// Gate verdict for a flavor request: `Some(french error)` when
/// `experimental` is requested while `gated`, `None` otherwise. Pure so both
/// positions of the switch stay unit-testable (the live path always passes
/// [`EXPERIMENTAL_GATED`).
pub fn experimental_gate_rejects(experimental: bool, gated: bool) -> Option<String> {
    if experimental && gated {
        Some(EXPERIMENTAL_GATED_MSG.to_string())
    } else {
        None
    }
}

/// Boot migration: when `gated` and the stored settings select the
/// experimental branch, revert both flags to default. Returns `true` when
/// something moved (caller persists + queues
/// [`EXPERIMENTAL_GATED_MIGRATION_MSG` once). Pure w.r.t. disk.
pub fn apply_experimental_gate_to(settings: &mut SettingsDto, gated: bool) -> bool {
    if gated && (settings.experimental_runtime || settings.experimental_optimizations) {
        settings.experimental_runtime = false;
        settings.experimental_optimizations = false;
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// One connected account in the persisted roster (no secrets — the
/// refresh token lives per-UUID in the OS keyring).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRef {
    /// Player UUID (dashes included, as returned by Mojang).
    pub uuid: String,
    /// Player name at connect time (refreshed on each login).
    pub name: String,
}

/// Editable launcher knobs (JSON-serializable for the settings screen).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsDto {
    /// Substituted into `${options.memory.min}`.
    #[serde(default = "default_memory_min")]
    pub memory_min: String,
    /// Substituted into `${options.memory.max}`.
    #[serde(default = "default_memory_max")]
    pub memory_max: String,
    /// Optional `java` binary override (empty = auto-detect).
    #[serde(default)]
    pub java_path: String,
    /// Legacy Java runtime selector (`"temurin8"` | `"graalvm21"`).
    /// Compat-only: the file field is preserved on load/save but nothing
    /// reads it anymore — the runtime derives from the pack flavor
    /// (`java_runtime_for_flavor`), with the `java_path` override winning.
    #[serde(default = "default_java_runtime")]
    pub java_runtime: String,
    /// Instance root override (empty = [`default_game_dir`]).
    #[serde(default)]
    pub game_dir: String,
    /// Custom Azure client-ID override (empty = env / build default).
    #[serde(default)]
    pub client_id: String,
    /// Game resolution, e.g. `"1920x1080"` (persisted; applied at launch when
    /// the manifest/game supports it — currently stored for the UI only).
    #[serde(default = "default_resolution")]
    pub resolution: String,
    /// Start the game in fullscreen (persisted for the UI; see above).
    #[serde(default)]
    pub fullscreen: bool,
    /// Show the live console while the game runs.
    #[serde(default = "default_true")]
    pub console_live: bool,
    /// Selected JVM preset key (`"46anarchy"` | `"g1gc"` | `"lowram"` |
    /// `"custom"`).
    #[serde(default = "default_jvm_preset")]
    pub jvm_preset: String,
    /// Full custom JVM args line (used when `jvm_preset == "custom"`).
    #[serde(default)]
    pub jvm_args: String,
    /// Experimental LWJGL3 + Java 21 runtime (default OFF — the Java 8 game
    /// is untouched). Provisioned and launched via `launcher_core::modern`.
    #[serde(default)]
    pub experimental_runtime: bool,
    /// Optimization stack on the experimental path (GTNHLib, Hodgepodge,
    /// ArchaicFix, FalseTweaks + FalsePatternLib — default OFF, separate
    /// opt-in; only meaningful with `experimental_runtime`).
    #[serde(default)]
    pub experimental_optimizations: bool,
    /// Known accounts roster (uuid + name, no secrets) for boot restore
    /// and the switcher. The live sessions live in `AppState`; this is
    /// the persisted half, synced on every account mutation.
    #[serde(default)]
    pub accounts: Vec<AccountRef>,
    /// Active account UUID (`None` = first connected account wins).
    #[serde(default)]
    pub active_account: Option<String>,
}

fn default_memory_min() -> String {
    launcher_core::DEFAULT_MEMORY_MIN.to_string()
}

fn default_memory_max() -> String {
    launcher_core::DEFAULT_MEMORY_MAX.to_string()
}

/// Default game resolution shown in the Java configuration screen.
fn default_resolution() -> String {
    "1920x1080".to_string()
}

fn default_true() -> bool {
    true
}

/// Default JVM preset key (matches the `46anarchy` preset in the UI).
fn default_jvm_preset() -> String {
    "46anarchy".to_string()
}

/// Default Java runtime (Temurin 8 — the safe game default).
fn default_java_runtime() -> String {
    "temurin8".to_string()
}

impl Default for SettingsDto {
    fn default() -> Self {
        Self {
            memory_min: default_memory_min(),
            memory_max: default_memory_max(),
            java_path: String::new(),
            java_runtime: default_java_runtime(),
            game_dir: String::new(),
            client_id: String::new(),
            resolution: default_resolution(),
            fullscreen: false,
            console_live: true,
            jvm_preset: default_jvm_preset(),
            jvm_args: String::new(),
            experimental_runtime: false,
            experimental_optimizations: false,
            accounts: Vec::new(),
            active_account: None,
        }
    }
}

/// On-disk shape. Every field is optional so hand-written or older files
/// still load; unknown fields are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    memory_min: Option<String>,
    #[serde(default)]
    memory_max: Option<String>,
    #[serde(default)]
    java_path: Option<String>,
    #[serde(default)]
    java_runtime: Option<String>,
    #[serde(default)]
    game_dir: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    msa_client_id: Option<String>,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    fullscreen: Option<bool>,
    #[serde(default)]
    console_live: Option<bool>,
    #[serde(default)]
    jvm_preset: Option<String>,
    #[serde(default)]
    jvm_args: Option<String>,
    #[serde(default)]
    experimental_runtime: Option<bool>,
    #[serde(default)]
    experimental_optimizations: Option<bool>,
    #[serde(default)]
    accounts: Option<Vec<AccountRef>>,
    #[serde(default)]
    active_account: Option<String>,
    #[serde(default)]
    auth: Option<AuthSection>,
}

/// Nested `{ "auth": { "client_id": "..." } }` override (same as `AuthConfig`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AuthSection {
    #[serde(default)]
    client_id: Option<String>,
}

impl From<&SettingsDto> for SettingsFile {
    fn from(s: &SettingsDto) -> Self {
        Self {
            memory_min: Some(s.memory_min.clone()),
            memory_max: Some(s.memory_max.clone()),
            java_path: Some(s.java_path.clone()),
            java_runtime: Some(s.java_runtime.clone()),
            game_dir: Some(s.game_dir.clone()),
            client_id: Some(s.client_id.clone()),
            msa_client_id: None,
            resolution: Some(s.resolution.clone()),
            fullscreen: Some(s.fullscreen),
            console_live: Some(s.console_live),
            jvm_preset: Some(s.jvm_preset.clone()),
            jvm_args: Some(s.jvm_args.clone()),
            experimental_runtime: Some(s.experimental_runtime),
            experimental_optimizations: Some(s.experimental_optimizations),
            accounts: Some(s.accounts.clone()),
            active_account: s.active_account.clone(),
            auth: None,
        }
    }
}

/// Settings file path for a game dir.
pub fn settings_path(game_dir: &Path) -> PathBuf {
    game_dir.join(SETTINGS_FILE_NAME)
}

/// Load settings, falling back to defaults on any error.
pub fn load_settings_file(path: &Path) -> SettingsDto {
    let Ok(bytes) = std::fs::read(path) else {
        return SettingsDto::default();
    };
    let Ok(file): Result<SettingsFile, _> = serde_json::from_slice(&bytes) else {
        return SettingsDto::default();
    };
    let mut out = SettingsDto::default();
    if let Some(v) = file.memory_min.filter(|v| !v.trim().is_empty()) {
        out.memory_min = v;
    }
    if let Some(v) = file.memory_max.filter(|v| !v.trim().is_empty()) {
        out.memory_max = v;
    }
    if let Some(v) = file.java_path {
        out.java_path = v;
    }
    if let Some(v) = file.java_runtime.filter(|v| !v.trim().is_empty()) {
        out.java_runtime = v;
    }
    if let Some(v) = file.game_dir {
        out.game_dir = v;
    }
    let nested = file.auth.and_then(|a| a.client_id).unwrap_or_default();
    let id = file
        .client_id
        .or(file.msa_client_id)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(nested);
    out.client_id = id;
    if let Some(v) = file.resolution.filter(|v| !v.trim().is_empty()) {
        out.resolution = v;
    }
    if let Some(v) = file.fullscreen {
        out.fullscreen = v;
    }
    if let Some(v) = file.console_live {
        out.console_live = v;
    }
    if let Some(v) = file.jvm_preset.filter(|v| !v.trim().is_empty()) {
        out.jvm_preset = v;
    }
    if let Some(v) = file.jvm_args {
        out.jvm_args = v;
    }
    if let Some(v) = file.experimental_runtime {
        out.experimental_runtime = v;
    }
    if let Some(v) = file.experimental_optimizations {
        out.experimental_optimizations = v;
    }
    if let Some(v) = file.accounts {
        out.accounts = v;
    }
    if let Some(v) = file.active_account.filter(|v| !v.trim().is_empty()) {
        out.active_account = Some(v);
    }
    out
}

/// Persist settings (creates parent folders, pretty JSON).
pub fn save_settings_file(path: &Path, settings: &SettingsDto) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = SettingsFile::from(settings);
    let text = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

/// Resolve the Azure client id: settings override → `PRISM_MSA_CLIENT_ID`
/// env / build default. `None` when only the placeholder is configured.
pub fn effective_client_id(settings: &SettingsDto) -> Option<String> {
    if !settings.client_id.trim().is_empty() {
        return Some(settings.client_id.trim().to_string());
    }
    let cfg = AuthConfig::from_env();
    if cfg.client_id.trim().is_empty() || cfg.client_id == PLACEHOLDER_CLIENT_ID {
        None
    } else {
        Some(cfg.client_id)
    }
}

pub(crate) fn missing_client_id_help() -> String {
    "Aucun client-ID Azure configuré : renseignez PRISM_MSA_CLIENT_ID ou le champ « Client-ID » des réglages.".to_string()
}

// ---------------------------------------------------------------------------
// Frontend DTOs
// ---------------------------------------------------------------------------

/// Public session view (tokens never leave the backend).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Player name.
    pub name: String,
    /// Player UUID.
    pub uuid: String,
    /// First 8 hex chars (header chip).
    pub uuid_short: String,
}

impl From<&MinecraftSession> for SessionInfo {
    fn from(s: &MinecraftSession) -> Self {
        let raw: String = s.uuid.chars().filter(|c| *c != '-').collect();
        let uuid_short = if raw.len() > 8 {
            format!("{}…", &raw[..8])
        } else {
            raw
        };
        Self {
            name: s.name.clone(),
            uuid: s.uuid.clone(),
            uuid_short,
        }
    }
}

/// Manifest + freshness view for the home/install screens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestStatusDto {
    /// Display name (`manifest.name` or fallback).
    pub name: String,
    /// Manifest `version` (empty when no manifest yet).
    pub version: String,
    /// Minecraft version (e.g. `"1.7.10"`).
    pub mc_version: String,
    /// `init` | `ok` | `out_of_date` | `cdn_failure`.
    pub update_status: String,
    /// French one-liner for the status row.
    pub detail: String,
    /// Maintenance flag (blocks play).
    pub maintenance_enabled: bool,
    /// Maintenance message (may be empty).
    pub maintenance_message: String,
}

/// Device-code view shown in the login modal (no secret material).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCodeInfo {
    /// Page where the user approves the login.
    pub verification_uri: String,
    /// Optional complete uri (embeds the user code).
    pub verification_uri_complete: Option<String>,
    /// Code the user types on the verification page.
    pub user_code: String,
    /// Human-readable instructions.
    pub message: String,
    /// Lifetime of the device code, seconds.
    pub expires_in: u64,
    /// Recommended poll interval, seconds.
    pub interval: u64,
}

impl From<&DeviceCodeResponse> for DeviceCodeInfo {
    fn from(d: &DeviceCodeResponse) -> Self {
        Self {
            verification_uri: d.verification_uri.clone(),
            verification_uri_complete: d.verification_uri_complete.clone(),
            user_code: d.user_code.clone(),
            message: d.message.clone(),
            expires_in: d.expires_in,
            interval: d.interval,
        }
    }
}

/// Download totals returned by `start_install` / `play`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallSummaryDto {
    /// Files in this run.
    pub total_files: usize,
    /// Files actually fetched from the CDN.
    pub downloaded_files: usize,
    /// Files skipped (size + SHA1 already matched).
    pub skipped_files: usize,
    /// Sum of manifest `size` over all files.
    pub total_bytes: u64,
}

/// File-verification report (no downloads performed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReportDto {
    /// Files relevant on this OS.
    pub total: usize,
    /// Files missing or failing the size/SHA1 check.
    pub stale: usize,
    /// Sum of manifest `size` over stale files.
    pub stale_bytes: u64,
}

/// Uninstall report (payload wiped, settings/runtimes/logs kept).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallReportDto {
    /// Files deleted.
    pub removed_files: u64,
    /// Directories deleted.
    pub removed_dirs: u64,
}

/// Pack-flavor switch result (`set_pack_flavor` backend path): flags
/// persisted plus the immediate stash/restore moves, with ready-to-display
/// French Console lines (one per move; empty when nothing moved — repeat
/// calls are silent no-ops).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackFlavorSwapDto {
    /// Requested flavor now active (`true` = experimental).
    pub experimental: bool,
    /// Moved file names (sorted; stash or restore direction).
    pub moved: Vec<String>,
    /// French Console line per move (same order as `moved`).
    pub lines: Vec<String>,
}

/// Java probe view for the install screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaInfoDto {
    /// Resolved binary path.
    pub path: String,
    /// Parsed major version, if recognizable.
    pub major: Option<u32>,
    /// `major == 8` (Forge 1.7.10 requirement).
    pub is_java8: bool,
    /// French warning when the probe is not Java 8 (`None` = no warning).
    pub warning: Option<String>,
}

/// Both JDK statuses for the JDK pane (serializable pair).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRuntimesDto {
    /// Temurin 8 probe (bundled `java/` → JAVA_HOME → PATH).
    pub temurin8: JavaInfoDto,
    /// GraalVM 21 probe (`java21/`, empty path when absent).
    pub graalvm21: JavaInfoDto,
}

/// One row of the accounts switcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfoDto {
    /// Player UUID.
    pub uuid: String,
    /// Player name.
    pub name: String,
    /// Currently launching with this account.
    pub active: bool,
}
///
/// `required` mods are listed in the CDN manifest and stay locked in the UI
/// (the toggle refuses them); anything else found in the `mods` folder is an
/// optional local mod toggled by renaming `<name>.jar` ↔ `<name>.jar.disabled`
/// (vanilla Forge discovers `.jar`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModInfo {
    /// Toggle key: the on-disk `.jar` name (e.g. `"OptiFine_1.7.10_HD_U_E7.jar"`).
    pub id: String,
    /// On-disk file name (`"<id>"` when enabled, `"<id>.disabled"` when off).
    pub file_name: String,
    /// Short display name (file stem, `.jar` stripped).
    pub display_name: String,
    /// One-liner (manifest model description, empty for local-only mods).
    pub description: String,
    /// Version (manifest model version, empty for local-only mods).
    pub version: String,
    /// Size in bytes (manifest size, else on-disk length).
    pub size: u64,
    /// `true` when listed in the CDN manifest (locked in the UI).
    pub required: bool,
    /// `false` when the file carries the `.disabled` suffix.
    pub enabled: bool,
}

/// Streaming install progress (emitted as the `install-progress` event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressPayload {
    /// Files finished so far.
    pub completed_files: usize,
    /// Total files in this run.
    pub total_files: usize,
    /// Sum of `size` for finished files.
    pub completed_bytes: u64,
    /// Sum of `size` for all files.
    pub total_bytes: u64,
    /// File currently being written.
    pub current_file: String,
    /// 0.0–1.0 overall progress.
    pub fraction: f32,
    /// French detail line (`"12/48 fichiers — 45% — mods/a.jar"`).
    pub detail: String,
}

impl From<&ProgressUpdate> for ProgressPayload {
    fn from(u: &ProgressUpdate) -> Self {
        let fraction = progress_fraction(u);
        Self {
            completed_files: u.completed_files,
            total_files: u.total_files,
            completed_bytes: u.completed_bytes,
            total_bytes: u.total_bytes,
            current_file: u.current_file.clone(),
            fraction,
            detail: format_progress_detail(
                u.completed_files,
                u.total_files,
                &u.current_file,
                u.completed_bytes,
                u.total_bytes,
                fraction,
            ),
        }
    }
}

/// Streaming Java 8 download progress (emitted as the `java-progress` event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JavaProgressPayload {
    /// Bytes received so far.
    pub downloaded_bytes: u64,
    /// Total bytes announced by the server (`0` = unknown).
    pub total_bytes: u64,
    /// 0.0–1.0 download progress (`0.0` when the total is unknown).
    pub fraction: f32,
    /// French detail line (`"12,3 / 98,4 Mio — 13%"`).
    pub detail: String,
}

impl JavaProgressPayload {
    /// Build a payload from raw byte counters.
    pub fn from_bytes(downloaded_bytes: u64, total_bytes: u64) -> Self {
        let fraction = if total_bytes > 0 {
            (downloaded_bytes.min(total_bytes) as f32 / total_bytes as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let detail = if total_bytes > 0 {
            let pct = (fraction * 100.0).round() as u32;
            format!(
                "{} / {} — {pct}%",
                human_bytes(downloaded_bytes),
                human_bytes(total_bytes)
            )
        } else {
            format!("{} téléchargés…", human_bytes(downloaded_bytes))
        };
        Self {
            downloaded_bytes,
            total_bytes,
            fraction,
            detail,
        }
    }
}

/// `play` result (detached spawn; the game outlives the launcher).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayResultDto {
    /// Shell-quoted argv (shown in the log).
    pub command: String,
    /// Honest notes for settings the command line could not express
    /// (e.g. unparsable resolution); shown in the Console log.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `get_game_status` response: polling fallback for the `game-outcome`
/// `running` event (event delivery to the WebView proved unreliable, while
/// `invoke` polling works for everything). The frontend polls this every 3s
/// and reuses the same `gameRunning` flag + `renderGameRunningButton` path.
/// Never errors: unknown state (no recorded PID, dead process, recycled PID
/// without the game marker) is `{ running: false }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameStatusDto {
    /// `true` when the recorded game PID is alive AND its command line
    /// carries a game marker (pid-reuse safety).
    pub running: bool,
}

/// `game-outcome` event payload: pushed by the post-launch watcher AFTER
/// `play` resolved (never blocks the launch). The frontend appends `detail`
/// to the Journal card and toasts `cause` on crashes — the journal card
/// therefore never dead-ends at the launch-command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameOutcomeDto {
    /// `running` | `exited` | `crashed`.
    pub status: String,
    /// Process exit code (`None` when still running).
    pub code: Option<i32>,
    /// Crash cause line (`Caused by: …`, `None` unless `crashed`).
    pub cause: Option<String>,
    /// French journal line (always present).
    pub detail: String,
}

/// `get_console_tail` response: pollable console lines (never errors).
/// `since` echoes the caller's last seen cursor, `next` is the new cursor
/// to pass back as `since` on the next poll. Lines are already formatted
/// (same strings as the `console-line` events).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleTailDto {
    /// Console lines after `since` (oldest first, bounded).
    pub lines: Vec<String>,
    /// New cursor (total lines ever appended, monotonic across runs).
    pub next: usize,
}

/// Live Minecraft server status (Server List Ping view for the home screen).
///
/// `online == false` means every other `Option` is `None` and `error` carries
/// the honest French message. `tps` is always `None`: TPS is NOT exposed by
/// the Server List Ping protocol, so the frontend shows "—".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerStatusDto {
    /// `true` when the status + ping round-trip succeeded.
    pub online: bool,
    /// Pinged address (`"host:port"`).
    pub address: String,
    /// Players currently online (`None` when offline).
    pub players_online: Option<u32>,
    /// Player slots (`None` when offline).
    pub players_max: Option<u32>,
    /// Server version name (e.g. `"1.7.10"`).
    pub version: Option<String>,
    /// Plain-text MOTD (§ codes stripped).
    pub motd: Option<String>,
    /// Ping/pong round-trip in milliseconds.
    pub ping_ms: Option<u64>,
    /// Always `None` (TPS non exposé par le ping).
    pub tps: Option<f32>,
    /// French error when `online == false`.
    pub error: Option<String>,
}

impl ServerStatusDto {
    /// Offline view (no fake numbers, error propagated honestly).
    pub fn offline(address: String, error: String) -> Self {
        Self {
            online: false,
            address,
            players_online: None,
            players_max: None,
            version: None,
            motd: None,
            ping_ms: None,
            tps: None,
            error: Some(error),
        }
    }
}

/// One patch-note entry (changelog / installed version / GitHub release).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchNoteDto {
    /// Entry version (e.g. `"v1.2.0"`).
    pub version: String,
    /// ISO date (`"2024-05-01"`, empty when unknown).
    pub date: String,
    /// Short title.
    pub title: String,
    /// Markdown-ish body (may be empty).
    pub body: String,
    /// `"major"` | `"hotfix"` | `"info"`.
    pub kind: String,
}

/// Patch-notes feed + reachability verdict for the home screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchNotesFeedDto {
    /// Newest-first entries (remote + local + installed merge, capped —
    /// single `"Aucune note"` fallback when empty).
    pub notes: Vec<PatchNoteDto>,
    /// `true` when the fresh GitHub fetch failed on every repo (stale cache
    /// still served when present, else empty): the frontend shows the muted
    /// offline hint instead of a silent empty state. No toast, no error.
    pub unreachable: bool,
}

/// Launcher version for the bottom status bar (never the modpack version,
/// which stays in `#version-pill`). `display` is the short text
/// (`v0.1.0`, commit hash, or `"dev"`); `detail` is the human source line
/// shown as tooltip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// Short display text for `#stat-version`.
    pub display: String,
    /// Human source line for the `title` tooltip.
    pub detail: String,
}

/// Tampon d'identité du build (jamais d'erreur, `"?"` quand inconnu) :
/// hachés à la compilation par `build.rs` (FNV-1a 32 bits, 8 hex), lus
/// via `option_env!`. Le frontend l'affiche au démarrage dans le journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfoDto {
    /// Haché du triplet `frontend/dist/{main.js,index.html,styles.css}`.
    pub frontend_hash: String,
    /// Haché des sources `src/*.rs` (triées par nom).
    pub backend_hash: String,
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Single mutex-guarded backend value owned by the Tauri app.
#[derive(Debug)]
pub struct AppState {
    /// Editable knobs.
    pub settings: SettingsDto,
    /// Connected Microsoft accounts (empty = logged out; no offline mode).
    /// Refresh tokens live per-UUID in the OS keyring; only the roster
    /// (uuid + name, no secrets) persists in settings.
    pub sessions: Vec<MinecraftSession>,
    /// Active account UUID (`None` = first session wins; always `Some`
    /// when `sessions` is non-empty — maintained by the helpers below).
    pub active_account: Option<String>,
    /// Fresh manifest (drives version label + maintenance banner).
    pub manifest: Option<Manifest>,
    /// Cached-vs-fresh freshness.
    pub update_status: UpdateStatus,
    /// Pending device-code flow (set by `request_login`, consumed by `poll_login`).
    pub pending_login: Option<DeviceCodeResponse>,
    /// Downloader running (guards concurrent installs).
    pub install_running: bool,
    /// Set by `cancel_install`; checked by the downloader (each new run
    /// resets it, so a stale cancel never kills a fresh install).
    pub install_cancel: Arc<AtomicBool>,
    /// Set by `cancel_java_download`; checked per download chunk.
    pub java_cancel: Arc<AtomicBool>,
    /// PID recorded at spawn by `spawn_detached_game` (`None` = no launch
    /// this session, e.g. fresh boot while the game runs — known caveat).
    /// Read by `get_game_status` (liveness + command-line marker gate).
    pub game_pid: Option<u32>,
    /// Dernier verdict `get_game_status` (journalisation à la transition
    /// uniquement : le journal du lanceur prouve `false→true`/`true→false`
    /// + PID, silencieux sinon — même contrat que les magasins sondables).
    pub last_game_running: bool,
    /// Dernier `install-progress` émis (sondage `get_install_progress`) :
    /// `None` tant qu'aucun run n'a émis. `std::sync::Mutex` (et non le
    /// `tokio` Mutex de l'état) pour rester enregistrable depuis la
    /// fermeture d'émission synchrone ; section critique minuscule
    /// (un `clone`), jamais tenue entre deux `await`.
    pub last_install_progress: Arc<std::sync::Mutex<Option<ProgressPayload>>>,
    /// Dernier `java-progress` émis (sondage `get_java_progress`) : même
    /// contrat que `last_install_progress` pour la modale Java.
    pub last_java_progress: Arc<std::sync::Mutex<Option<JavaProgressPayload>>>,
    /// Dernier verdict `game-outcome` (sondage `get_last_outcome`) : `None`
    /// tant qu'aucun lancement n'a produit de verdict. Même contrat que les
    /// magasins de progression (`std::sync::Mutex`, section minuscule).
    pub last_outcome: Arc<std::sync::Mutex<Option<GameOutcomeDto>>>,
    /// Anneau des lignes console du lancement (sondage `get_console_tail`) :
    /// les mêmes chaînes que les événements `console-line`, borné aux 500
    /// dernières. `console_seq` est le curseur monotone (total ajouté sur
    /// tous les runs, jamais remis à zéro — un curseur frontend périmé reste
    /// valide après un nouveau lancement).
    pub console_lines: Arc<std::sync::Mutex<VecDeque<String>>>,
    /// Total des lignes console ajoutées (curseur `next` du sondage).
    pub console_seq: Arc<AtomicUsize>,
    /// One-shot boot notices for the frontend to toast (French lines,
    /// drained by `commands::get_boot_notices`): currently the
    /// experimental-gate migration notice, if the stored settings said
    /// experimental at boot.
    pub boot_notices: Vec<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            settings: SettingsDto::default(),
            sessions: Vec::new(),
            active_account: None,
            manifest: None,
            update_status: UpdateStatus::Init,
            pending_login: None,
            install_running: false,
            install_cancel: Arc::new(AtomicBool::new(false)),
            java_cancel: Arc::new(AtomicBool::new(false)),
            game_pid: None,
            last_game_running: false,
            last_install_progress: Arc::new(std::sync::Mutex::new(None)),
            last_java_progress: Arc::new(std::sync::Mutex::new(None)),
            last_outcome: Arc::new(std::sync::Mutex::new(None)),
            console_lines: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            console_seq: Arc::new(AtomicUsize::new(0)),
            boot_notices: Vec::new(),
        }
    }
}

impl AppState {
    /// Boot state: settings file + cached `cdn_last.json` (sync, best effort).
    pub fn boot() -> Self {
        let mut state = Self::default();
        let hinted = std::env::var("ANARCHY_GAME_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty());
        let mut game_dir = hinted.map(PathBuf::from).unwrap_or_else(default_game_dir);
        let mut settings = load_settings_file(&settings_path(&game_dir));
        if !settings.game_dir.trim().is_empty() {
            game_dir = PathBuf::from(settings.game_dir.trim());
            settings = load_settings_file(&settings_path(&game_dir));
            settings.game_dir = game_dir.to_string_lossy().into_owned();
        }
        // Gated experimental branch: a stored experimental selection reverts
        // to default ONCE (persisted here, toasted via `boot_notices`). The
        // modern stack on disk is untouched — the next play/install prunes it
        // into the stash (never deleted) through the existing self-heal.
        if apply_experimental_gate_to(&mut settings, EXPERIMENTAL_GATED) {
            let _ = save_settings_file(&settings_path(&game_dir), &settings);
            state
                .boot_notices
                .push(EXPERIMENTAL_GATED_MIGRATION_MSG.to_string());
        }
        state.settings = settings;
        if let Ok(Some(m)) = load_manifest(&launcher_core::cdn_last_path(&game_dir)) {
            state.update_status = UpdateStatus::Ok {
                version: m.version.clone(),
            };
            state.manifest = Some(m);
        }
        state
    }

    /// Instance root (settings override or OS default).
    pub fn game_dir(&self) -> PathBuf {
        let trimmed = self.settings.game_dir.trim();
        if trimmed.is_empty() {
            default_game_dir()
        } else {
            // Custom dirs get the same `paladium` trailing segment
            // (PalaMod tripwire) unless already present.
            normalize_game_dir(Path::new(trimmed))
        }
    }

    /// Java binary used for launch (settings override or auto-detect).
    pub fn effective_java_bin(&self) -> PathBuf {
        let trimmed = self.settings.java_path.trim();
        if trimmed.is_empty() {
            launcher_core::resolve_java(&self.game_dir())
        } else {
            PathBuf::from(trimmed)
        }
    }

    /// `true` once at least one Microsoft session is in memory.
    pub fn is_logged_in(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Active session (explicit choice, else first connected account).
    pub fn active_session(&self) -> Option<&MinecraftSession> {
        match &self.active_account {
            Some(uuid) => self.sessions.iter().find(|s| &s.uuid == uuid),
            None => self.sessions.first(),
        }
    }

    /// Mutable active session (same rule as [`active_session`]).
    pub fn active_session_mut(&mut self) -> Option<&mut MinecraftSession> {
        match &self.active_account {
            Some(uuid) => {
                let uuid = uuid.clone();
                self.sessions.iter_mut().find(|s| s.uuid == uuid)
            }
            None => self.sessions.first_mut(),
        }
    }

    /// Insert or replace by UUID; first account becomes active when none is.
    pub fn upsert_session(&mut self, session: MinecraftSession) {
        if let Some(slot) = self.sessions.iter_mut().find(|s| s.uuid == session.uuid) {
            *slot = session;
        } else {
            self.sessions.push(session);
        }
        if self.active_account.is_none() {
            self.active_account = self.sessions.first().map(|s| s.uuid.clone());
        }
    }

    /// Drop one account; the active pointer falls back to the first
    /// remaining account (or `None` when empty). Returns `true` if removed.
    pub fn remove_session(&mut self, uuid: &str) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.uuid != uuid);
        let removed = self.sessions.len() != before;
        if self.active_account.as_deref() == Some(uuid) {
            self.active_account = self.sessions.first().map(|s| s.uuid.clone());
        }
        removed
    }

    /// Switch the active account. Returns `false` for unknown UUIDs
    /// (active pointer untouched). Roster-only entries (quarantined or not
    /// yet restored this boot) are switchable too: the caller then refreshes
    /// the token on demand.
    pub fn set_active_account(&mut self, uuid: &str) -> bool {
        if self.sessions.iter().any(|s| s.uuid == uuid)
            || self.settings.accounts.iter().any(|a| a.uuid == uuid)
        {
            self.active_account = Some(uuid.to_string());
            true
        } else {
            false
        }
    }

    /// Quarantine a dead account (`invalid_grant`): the refresh token is gone
    /// (cleared by the async caller — it really is dead), the live session
    /// corpse is dropped so it can never launch, but the roster entry is
    /// KEPT with its last-known name so the account stays visible in Comptes
    /// with a reconnect path instead of vanishing. When the dead account was
    /// active, the pointer falls back to another live session, else `None`.
    /// Pure w.r.t. network (keyring + persistence stay with the caller).
    pub(crate) fn quarantine_dead_session(&mut self, uuid: &str) {
        let live_name = self
            .sessions
            .iter()
            .find(|s| s.uuid == uuid)
            .map(|s| s.name.clone());
        let roster_name = self
            .settings
            .accounts
            .iter()
            .find(|a| a.uuid == uuid)
            .map(|a| a.name.clone());
        let name = live_name
            .clone()
            .or(roster_name)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "Compte déconnecté".to_string());
        self.sessions.retain(|s| s.uuid != uuid);
        if self.active_account.as_deref() == Some(uuid) {
            self.active_account = self.sessions.first().map(|s| s.uuid.clone());
        }
        match self.settings.accounts.iter_mut().find(|a| a.uuid == uuid) {
            Some(entry) => {
                // The live session knew the freshest label; otherwise keep
                // the stored one untouched.
                if live_name.is_some() {
                    entry.name = name;
                }
            }
            None => self.settings.accounts.push(AccountRef {
                uuid: uuid.to_string(),
                name,
            }),
        }
        self.settings.active_account = self.active_session().map(|s| s.uuid.clone());
    }

    /// Mandatory login gate: `Err(LOGIN_REQUIRED…)` when logged out.
    pub fn ensure_logged_in(&self) -> Result<&MinecraftSession, String> {
        self.active_session().ok_or_else(login_required_error)
    }

    /// Human-readable display name (`manifest.name` or fallback).
    pub fn display_name(&self) -> String {
        self.manifest
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| APP_NAME_FALLBACK.to_string())
    }

    /// Flat manifest status view for the frontend.
    pub fn manifest_status(&self) -> ManifestStatusDto {
        let (key, detail) = match &self.update_status {
            UpdateStatus::Init => ("init", "Première installation requise.".to_string()),
            UpdateStatus::Ok { version } => ("ok", format!("À jour ({version}).")),
            UpdateStatus::OutOfDate {
                last_version,
                new_version,
            } => (
                "out_of_date",
                format!("Mise à jour disponible : {last_version} → {new_version}."),
            ),
            UpdateStatus::CdnFailure(e) => ("cdn_failure", format!("Comparaison impossible : {e}")),
        };
        let (maintenance_enabled, maintenance_message) = self
            .manifest
            .as_ref()
            .map(|m| (m.maintenance.enabled, m.maintenance.message.clone()))
            .unwrap_or((false, String::new()));
        ManifestStatusDto {
            name: self.display_name(),
            version: self
                .manifest
                .as_ref()
                .map(|m| m.version.clone())
                .unwrap_or_default(),
            mc_version: self
                .manifest
                .as_ref()
                .map(|m| m.mc_version.clone())
                .unwrap_or_default(),
            update_status: key.to_string(),
            detail,
            maintenance_enabled,
            maintenance_message,
        }
    }

    /// Maintenance message when the CDN enables maintenance.
    pub fn maintenance_message(&self) -> Option<String> {
        self.manifest.as_ref().and_then(|m| {
            if m.maintenance.enabled {
                let msg = m.maintenance.message.trim();
                Some(if msg.is_empty() {
                    "Maintenance en cours. Le jeu est temporairement indisponible.".to_string()
                } else {
                    msg.to_string()
                })
            } else {
                None
            }
        })
    }

    /// Instantané cloné pour construire le lancement hors mutex : le
    /// `tokio::Mutex` ne couvre que ce clone (microsecondes), tout le
    /// travail lourd (`build_full_command`, balayage bytecode des jars)
    /// tourne ensuite sans verrou (voir `launch_argv_from`).
    pub fn launch_snapshot(
        &self,
    ) -> Result<(Manifest, MinecraftSession, PathBuf, SettingsDto), String> {
        let manifest = self
            .manifest
            .clone()
            .ok_or_else(|| "Manifeste introuvable.".to_string())?;
        let session = self.ensure_logged_in()?.clone();
        let game_dir = self.game_dir();
        let settings = self.settings.clone();
        Ok((manifest, session, game_dir, settings))
    }

    /// Assemble the Forge launch command (no spawn — testable).
    ///
    /// Returns `(argv, game_dir, warnings)`: `warnings` carries honest notes
    /// for settings the 1.7.10 command line cannot express (e.g. an
    /// unparsable resolution), surfaced by the caller in the Console log.
    pub fn launch_argv(&self) -> Result<(Vec<String>, PathBuf, Vec<String>), String> {
        let (manifest, session, game_dir, settings) = self.launch_snapshot()?;
        Self::launch_argv_from(&manifest, &session, &game_dir, &settings)
    }

    /// Construction pure du lancement à partir d'un instantané (hors mutex).
    /// Même résultat que [`launch_argv`], sans emprunter l'état partagé :
    /// les appelants async clonent via [`launch_snapshot`] sous un verrou
    /// court puis calculent ici, verrou relâché (le balayage bytecode ouvre
    /// chaque jar du classpath — secondes sur un gros `libraries/`).
    pub fn launch_argv_from(
        manifest: &Manifest,
        session: &MinecraftSession,
        game_dir: &Path,
        settings: &SettingsDto,
    ) -> Result<(Vec<String>, PathBuf, Vec<String>), String> {
        let game_dir = game_dir.to_path_buf();
        let manifest = manifest.clone();
        let session = session.clone();
        let profile = LaunchProfile::with_memory(
            session.name.clone(),
            session.uuid.clone(),
            session.mc_token.clone(),
            UserType::Msa,
            manifest.mc_version.clone(),
            game_dir.clone(),
            &launcher_core::Settings::new(settings.memory_min.trim(), settings.memory_max.trim()),
        );
        let cmd = build_full_command(&manifest, &profile, Os::current());
        let mut argv = cmd.full_command;
        let mut warnings = Vec::new();
        let experimental = settings.experimental_runtime;
        // Settings JVM preset / custom args: spliced before `-cp` (JVM
        // dedups repeated flags, last occurrence wins). The Log4Shell flag
        // and the FS-Patches updater opt-out always ride along (both
        // flavors — see LOG4J_NOLOOKUPS_FLAG / FSPATCHES_CDN_BYPASS_FLAG).
        let mut extras = vec![
            LOG4J_NOLOOKUPS_FLAG.to_string(),
            FSPATCHES_CDN_BYPASS_FLAG.to_string(),
        ];
        if experimental && settings.jvm_preset.trim() == "lowram" {
            // CMS was removed in JDK 14 — the lowram preset would crash
            // Java 21 on startup. Fall back loudly instead of silently.
            warnings.push(
                "Preset « lowram » ignoré en mode expérimental (CMS supprimé de Java 21) — preset 46anarchy appliqué.".to_string(),
            );
            extras.extend(JVM_PRESET_46ANARCHY.split_whitespace().map(str::to_string));
        } else {
            // Part utilisateur (preset ou textarea partagé) ajoutée BRUTE :
            // c'est `enforce_single_collector` (post-assemblage, plus bas)
            // qui tranche, jamais un assainissement partiel avant le canonique.
            extras.extend(extra_jvm_args(settings));
        }
        if experimental {
            // The upstream wall (RFB classloader, file encoding, add-opens).
            extras.extend(launcher_core::MODERN_JVM_WALL.iter().map(|s| s.to_string()));
            extras.extend(launcher_core::modern_jvm_extra_args(
                Os::current(),
                std::env::consts::ARCH,
            ));
            // Le tuning ZGC canonique est garanti par `enforce_single_collector`
            // (point unique : présent exactement une fois, sans doublon avec
            // un ZGC venu du preset ou du textarea).
            // Zephyr-UI refuses to boot without a stencil buffer.
            extras.push(launcher_core::MODERN_STENCIL_FLAG.to_string());
            // `-Xverify:none` is dead on modern JVMs — drop it.
            argv.retain(|a| a != "-Xverify:none");
        }
        if !extras.is_empty() {
            if let Some(cp) = argv.iter().position(|a| a == "-cp" || a == "-classpath") {
                for (j, arg) in extras.iter().enumerate() {
                    argv.insert(cp + j, arg.clone());
                }
            } else {
                warnings
                    .push("Arguments JVM personnalisés ignorés (aucun -cp trouvé).".to_string());
            }
        }
        // Resolution + fullscreen: 1.7.10 accepts `--width/--height/--fullscreen`
        // game args, appended after the template args.
        match parse_resolution(&settings.resolution) {
            Some((w, h)) => {
                argv.push("--width".to_string());
                argv.push(w.to_string());
                argv.push("--height".to_string());
                argv.push(h.to_string());
            }
            None if settings.resolution.trim().is_empty() => {}
            None => warnings.push(format!(
                "Résolution « {} » illisible — attendue sous la forme 1920x1080.",
                settings.resolution.trim()
            )),
        }
        if settings.fullscreen {
            argv.push("--fullscreen".to_string());
        }
        let override_java = settings.java_path.trim();
        if !override_java.is_empty() {
            if let Some(first) = argv.first_mut() {
                *first = override_java.to_string();
            }
        }
        if experimental {
            apply_modern_launch_for(settings, &mut argv, &game_dir, &manifest, &mut warnings)?;
        }
        // Point d'étranglement unique post-assemblage (preset + customs +
        // mur + mémoire : tout est déjà dans argv) : une seule famille de
        // collecteur survit, le reste part en avertissement français visible.
        if let Some(cp) = argv.iter().position(|a| a == "-cp" || a == "-classpath") {
            if cp > 1 {
                let flags: Vec<String> = argv[1..cp].to_vec();
                let (kept, stripped) = enforce_single_collector(&flags, experimental);
                push_gc_warning(&mut warnings, &stripped, experimental);
                argv.splice(1..cp, kept);
            }
        }
        // Garde bytecode : les jars du `-cp` résolu qui dépassent le plafond
        // du runtime (v52 classique / v65 expérimental) sont signalés en
        // français, jars nommés — AVERTISSEMENTS seulement, jamais d'échec
        // (FML ignore lui-même les jars illisibles : `it will be ignored`,
        // et le boot continue — bloquer le lancement serait strictement
        // pire). Niveau info/debug, lancement inchangé.
        {
            let cap = launcher_core::class_version_cap_for(experimental);
            if let Some(cp_pos) = argv.iter().position(|a| a == "-cp" || a == "-classpath") {
                if let Some(cp_value) = argv.get(cp_pos + 1) {
                    let sep = launcher_core::classpath_separator(Os::current());
                    let paths: Vec<std::path::PathBuf> = cp_value
                        .split(sep)
                        .filter(|s| !s.is_empty())
                        .map(std::path::PathBuf::from)
                        .collect();
                    warnings.extend(launcher_core::check_jar_class_versions(&paths, cap));
                }
            }
        }
        Ok((argv, game_dir, warnings))
    }

    /// Shell-quoted launch command (log display).
    pub fn launch_command_string(&self) -> Result<String, String> {
        Ok(quote_command(&self.launch_argv()?.0))
    }
}

/// Experimental rewrite, forme pure (voir `AppState::apply_modern_launch`) :
/// mêmes lectures disque, sans emprunter l'état — appelée par
/// `launch_argv_from` hors mutex.
fn apply_modern_launch_for(
    settings: &SettingsDto,
    argv: &mut [String],
    game_dir: &Path,
    manifest: &launcher_core::Manifest,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let profile = launcher_core::load_cached_modern_profile(game_dir).ok_or_else(|| {
        "Profil LWJGL3 manquant — relancez une installation en mode expérimental.".to_string()
    })?;
    // Java runtime selection (auto-managed from the pack flavor:
    // experimental always launches on GraalVM 21 —
    // `java_runtime_for_flavor(true)`; the `java_runtime` settings
    // field is compat-only and ignored here). Explicit `java_path`
    // override still wins when set. A missing/invalid GraalVM falls
    // back with a warning instead of erroring (a stale `java21/` dir
    // must never break launch — the next install/play re-provisions it).
    if settings.java_path.trim().is_empty() {
        debug_assert_eq!(launcher_core::java_runtime_for_flavor(true), "graalvm21");
        match launcher_core::resolve_modern_java(game_dir) {
                Some(java21) => {
                    if let Some(first) = argv.first_mut() {
                        *first = java21.to_string_lossy().into_owned();
                    }
                }
                None => warnings.push(
                    "GraalVM 21 absent ou invalide — Temurin 8 utilisé (runtime réinstallé à la prochaine vérification)."
                        .to_string(),
                ),
            }
    }
    // Classpath: upstream profile order + vanilla jar last.
    let mut cp = launcher_core::modern_classpath_entries(
        &profile,
        game_dir,
        &manifest.mc_version,
        Os::current(),
        std::env::consts::ARCH,
    );
    // Pinned CDN libraries the profile omits (slf4j, MongoDB, molang,
    // mclib, glm, guava-17.0) — downloaded by the install plan with SHAs.
    cp.extend(launcher_core::modern_cdn_classpath_entries(
        manifest,
        game_dir,
        Os::current(),
    ));
    let sep = launcher_core::classpath_separator(Os::current()).to_string();
    let Some(cp_pos) = argv.iter().position(|a| a == "-cp" || a == "-classpath") else {
        warnings.push("Mode expérimental : aucun -cp trouvé, classpath inchangé.".to_string());
        return Ok(());
    };
    if cp_pos + 1 < argv.len() {
        argv[cp_pos + 1] = cp.join(&sep);
        if cp_pos + 2 < argv.len() {
            argv[cp_pos + 2] = launcher_core::MODERN_MAIN_CLASS.to_string();
        }
    }
    Ok(())
}

/// Canonical JVM preset arg lines. MUST stay in sync with `JVM_PRESETS` in
/// `frontend/dist/main.js` (the settings screen shows the same text).
pub const JVM_PRESET_46ANARCHY: &str = "-XX:+UseG1GC -XX:+ParallelRefProcEnabled -XX:MaxGCPauseMillis=200 -XX:+UnlockExperimentalVMOptions -XX:+DisableExplicitGC -XX:+AlwaysPreTouch -Dfml.ignoreInvalidMinecraftCertificates=true -Dfml.ignorePatchDiscrepancies=true";
pub const JVM_PRESET_G1GC: &str = "-XX:+UseG1GC -XX:+ParallelRefProcEnabled -XX:MaxGCPauseMillis=150 -XX:+UnlockExperimentalVMOptions -XX:+AlwaysPreTouch -XX:G1ReservePercent=15 -Dfml.ignoreInvalidMinecraftCertificates=true";
pub const JVM_PRESET_LOWRAM: &str = "-XX:+UseConcMarkSweepGC -XX:+CMSIncrementalMode -XX:-UseAdaptiveSizePolicy -Xmn512m -Dfml.ignoreInvalidMinecraftCertificates=true";
/// Base ZGC du modpack expérimental (GraalVM 21) : même ligne que le preset
/// « graal-zgc » du frontend. « graal-lowram » = même base sans
/// `AlwaysPreTouch` (le pré-touch réserve tout le tas d'avance).
pub const JVM_PRESET_GRAAL_ZGC: &str = "-XX:+UseZGC -XX:+ZGenerational -XX:+AlwaysPreTouch -XX:+ParallelRefProcEnabled -XX:+DisableExplicitGC -Dfml.ignoreInvalidMinecraftCertificates=true -Dfml.ignorePatchDiscrepancies=true";
pub const JVM_PRESET_GRAAL_LOWRAM: &str = "-XX:+UseZGC -XX:+ZGenerational -XX:+ParallelRefProcEnabled -XX:+DisableExplicitGC -Dfml.ignoreInvalidMinecraftCertificates=true -Dfml.ignorePatchDiscrepancies=true";

/// Extra JVM args from settings: the selected preset line, or the custom
/// textarea content when `jvm_preset == "custom"`. Split on whitespace.
pub fn extra_jvm_args(settings: &SettingsDto) -> Vec<String> {
    let line = match settings.jvm_preset.trim() {
        "g1gc" => JVM_PRESET_G1GC,
        "lowram" => JVM_PRESET_LOWRAM,
        "graal-zgc" => JVM_PRESET_GRAAL_ZGC,
        "graal-lowram" => JVM_PRESET_GRAAL_LOWRAM,
        "custom" => settings.jvm_args.trim(),
        _ => JVM_PRESET_46ANARCHY,
    };
    line.split_whitespace().map(str::to_string).collect()
}

/// Nature d'un sélecteur GC (`-XX:[+-]Use<Collecteur>GC`,
/// `-XX:[+-]ZGenerational`, `-Xincgc`) : `"zgc"` | `"zgenerational"` |
/// `"epsilon"` | `"incgc"` | `"autre"`, `None` sinon. Les réglages
/// (`MaxGCPauseMillis`, `G1ReservePercent`, `CMSIncrementalMode`,
/// `UseAdaptiveSizePolicy`…) ne sont PAS des sélecteurs : sans leur
/// collecteur la JVM les ignore avec un avertissement, inoffensifs — on
/// ne retire que les sélecteurs (voir [`enforce_single_collector`]).
/// `UseEpsilonGC` n'est jamais un conflit « Multiple garbage collectors
/// selected » : classé à part pour être conservé dans les deux saveurs.
fn gc_selector_kind(arg: &str) -> Option<&'static str> {
    if arg == "-Xincgc" {
        return Some("incgc");
    }
    let body = arg
        .strip_prefix("-XX:+")
        .or_else(|| arg.strip_prefix("-XX:-"))?;
    if body == "ZGenerational" {
        return Some("zgenerational");
    }
    if body.starts_with("Use") && body.contains("GC") {
        if body.contains("ZGC") {
            return Some("zgc");
        }
        if body == "UseEpsilonGC" {
            return Some("epsilon");
        }
        return Some("autre");
    }
    None
}

/// `-XX:-…` (désactivation explicite) ou non.
fn is_negative_xx(arg: &str) -> bool {
    arg.starts_with("-XX:-")
}

/// Point d'étranglement unique post-assemblage contre le refus de démarrage
/// « Multiple garbage collectors selected » (pur, testé). S'applique à la
/// liste JVM COMPLÈTE (preset + customs + mur + mémoire — voir
/// `launch_argv`), jamais à une part isolée : tout ce qui a pu glisser un
/// second collecteur (preset G1GC, textarea partagé, mur, tuning) est
/// normalisé ici, en un seul endroit.
///
/// - expérimental : toute la famille classique (`UseG1GC`,
///   `UseParallelGC`, `UseParallelOldGC`, `UseConcMarkSweepGC`,
///   `UseSerialGC`, `UseShenandoahGC`, …, `-Xincgc`) et les désactivations
///   explicites du ZGC (`-XX:-UseZGC`, `-XX:-ZGenerational`) sont retirées ;
///   le contenu canonique [`launcher_core::MODERN_GC_TUNING`] gagne (chacun
///   exactement une fois, ordre canonique, en fin de liste) ;
/// - classique : les sélecteurs ZGC (`UseZGC`, `ZGenerational`, `+` comme
///   `-`) sont retirés, le reste est intact (ordre conservé).
///
/// `UseEpsilonGC` n'est jamais retiré (pas un conflit) ;
/// `-XX:+UseAdaptiveSizePolicy` et les autres non-sélecteurs passent
/// intacts, mémoire (`-Xms`/`-Xmx`/`-Xmn`/`-Xss`) incluse. Retour
/// `(conservés, retirés)` : l'appelant journalise les retirés en français
/// (aucune magie silencieuse).
pub fn enforce_single_collector(
    flags: &[String],
    experimental: bool,
) -> (Vec<String>, Vec<String>) {
    if experimental {
        let mut stripped = Vec::new();
        let mut rest = Vec::with_capacity(flags.len());
        for f in flags {
            let drop = match gc_selector_kind(f) {
                Some("autre") | Some("incgc") => true,
                Some("zgc") | Some("zgenerational") => is_negative_xx(f),
                _ => false,
            };
            if drop {
                stripped.push(f.clone());
            } else {
                rest.push(f.clone());
            }
        }
        // Le canonique gagne : dédupliqué (présent exactement une fois,
        // ordre canonique, en fin de liste — un ZGC venu du preset ou du
        // textarea ne se retrouve jamais en double).
        let tuning = launcher_core::MODERN_GC_TUNING;
        let mut kept = Vec::with_capacity(rest.len() + tuning.len());
        for f in rest {
            if tuning.contains(&f.as_str()) {
                continue;
            }
            kept.push(f);
        }
        kept.extend(tuning.iter().map(|s| s.to_string()));
        (kept, stripped)
    } else {
        let mut kept = Vec::with_capacity(flags.len());
        let mut stripped = Vec::new();
        for f in flags {
            match gc_selector_kind(f) {
                Some("zgc") | Some("zgenerational") => stripped.push(f.clone()),
                _ => kept.push(f.clone()),
            }
        }
        (kept, stripped)
    }
}

/// Jetons collecteurs seuls pour le diagnostic au spawn (jamais la commande
/// complète) : `Use*GC` / `ZGenerational` / `Xincgc` présents dans `argv`.
pub fn gc_spawn_tokens(argv: &[String]) -> Vec<String> {
    argv.iter()
        .filter(|a| gc_selector_kind(a).is_some())
        .cloned()
        .collect()
}

/// Ligne d'avertissement Console pour les sélecteurs GC retirés (français,
/// cause visible — jamais de magie silencieuse). Rien quand rien n'est retiré.
fn push_gc_warning(warnings: &mut Vec<String>, stripped: &[String], experimental: bool) {
    if stripped.is_empty() {
        return;
    }
    warnings.push(format!(
        "Sélecteurs GC incompatibles ignorés (contrôle post-assemblage, {}) : {}",
        if experimental {
            "mode expérimental, ZGC conservé"
        } else {
            "mode classique, ZGC retiré"
        },
        stripped.join(", ")
    ));
}

/// Log4Shell defense-in-depth (CVE-2021-44228): honored by Log4j ≥ 2.10,
/// harmless on the provisioned 2.0-beta9 (whose `JndiLookup.class` is
/// stripped instead — see `launcher_core::mojang::harden_log4j`). Always
/// appended to the JVM args, independent of the preset.
pub const LOG4J_NOLOOKUPS_FLAG: &str = "-Dlog4j2.formatMsgNoLookups=true";

/// FS-Patches in-game updater opt-out: the launcher owns install/verify
/// (manifest + SHA1 + progress), so the mod's own CDN re-download prompt
/// (`UpdaterTickHandler` opening `UpdaterScreen` on the first client tick —
/// no healthy auto-dismiss, Télécharger re-downloads everything, Quitter
/// calls `Minecraft.shutdown()`) is skipped via this documented launcher
/// property (`Fspatches.postInit` skips the tick registration when it
/// exists; ASM/mixin patches, launcher-check removal included, keep
/// working). Always appended to the JVM args, both flavors.
pub const FSPATCHES_CDN_BYPASS_FLAG: &str = "-Dbypass-cdn-need";

/// Parse `"1920x1080"` (also accepts `1920 x 1080`, `x`/`×` separators).
pub fn parse_resolution(value: &str) -> Option<(u32, u32)> {
    let cleaned = value.trim().replace(['×', 'X'], "x").replace(' ', "");
    let (w, h) = cleaned.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Overall progress 0.0–1.0: run bytes done / run bytes total, ONE unified
/// model (no phase bands). `completed_bytes` already includes the in-flight
/// chunk (see `launcher_core::ProgressUpdate`), so it is used verbatim —
/// adding `current_bytes` on top would double-count. File-count fallback
/// when the byte total is unknown (0 files + 0 bytes → 0.0).
pub fn progress_fraction(update: &ProgressUpdate) -> f32 {
    if update.total_bytes > 0 {
        (update.completed_bytes.min(update.total_bytes) as f32 / update.total_bytes as f32)
            .clamp(0.0, 1.0)
    } else if update.total_files > 0 {
        (update.completed_files as f32 / update.total_files as f32).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// French decimal with comma (`12.4` → `12,4`).
fn fr_decimal(value: f64) -> String {
    format!("{value:.1}").replace('.', ",")
}

/// `done/total` byte pair in ONE shared unit (the total's scale), French
/// style: `12,4/43,2 Mo`. Shared unit keeps the line parseable and stable
/// (the frontend shows `detail` verbatim, never appends to it).
pub fn format_progress_bytes(done: u64, total: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Ko", "Mo", "Go", "To"];
    let scale = total.max(done);
    let mut unit = 0;
    let mut div = 1u64;
    while scale >= div * 1024 && unit < UNITS.len() - 1 {
        div *= 1024;
        unit += 1;
    }
    if unit == 0 {
        format!("{done}/{total} {}", UNITS[unit])
    } else {
        format!(
            "{}/{} {}",
            fr_decimal(done as f64 / div as f64),
            fr_decimal(total as f64 / div as f64),
            UNITS[unit]
        )
    }
}

/// Backend-built file detail line (French, stable, parseable):
/// `Fichier 12/48 — Apollon-9.1.0.jar — 12,4/43,2 Mo (29 %)`.
/// Single builder so every phase spells it the same way.
pub fn format_progress_detail(
    completed_files: usize,
    total_files: usize,
    current_file: &str,
    done_bytes: u64,
    total_bytes: u64,
    fraction: f32,
) -> String {
    let pct = (fraction.clamp(0.0, 1.0) * 100.0).round() as u32;
    let name = if current_file.is_empty() {
        "—"
    } else {
        current_file
    };
    format!(
        "Fichier {completed_files}/{total_files} — {name} — {} ({pct} %)",
        format_progress_bytes(done_bytes, total_bytes)
    )
}

/// Hash-check-flavored detail for already-fresh files (bytes credited like a
/// download, but the line says verification): `Vérification 12/48 — foo.jar`.
pub fn format_verify_detail(completed_files: usize, total_files: usize, file: &str) -> String {
    format!("Vérification {completed_files}/{total_files} — {file}")
}

/// Human-readable byte count for the French UI.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Kio", "Mio", "Gio", "Tio"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Recompute freshness of `fresh` against the on-disk `cdn_last.json`.
pub fn freshness_against_cache(game_dir: &Path, fresh: &Manifest) -> UpdateStatus {
    let cached = load_manifest(&launcher_core::cdn_last_path(game_dir))
        .ok()
        .flatten();
    compare_manifests(cached.as_ref(), Some(fresh))
}

// ---------------------------------------------------------------------------
// Tests (headless backend state, no WebView)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_auth::MinecraftSession;
    use launcher_core::parse_manifest;

    const JSON: &str = include_str!("../../core/tests/fixtures/minimal_manifest.json");

    fn manifest() -> Manifest {
        parse_manifest(JSON).unwrap()
    }

    fn logged_in(game_dir: &Path) -> AppState {
        let mut s = AppState::default();
        s.settings.game_dir = game_dir.to_string_lossy().into_owned();
        s.manifest = Some(manifest());
        s.update_status = UpdateStatus::Ok {
            version: "v1.0.0".to_string(),
        };
        s.upsert_session(MinecraftSession {
            uuid: "123e4567-e89b-12d3-a456-426614174000".to_string(),
            name: "Steve".to_string(),
            mc_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            obtained: std::time::Instant::now(),
        });
        s
    }

    #[test]
    fn settings_roundtrip_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = settings_path(dir.path());
        // Missing file → defaults.
        assert_eq!(load_settings_file(&path), SettingsDto::default());
        let s = SettingsDto {
            memory_max: "6G".to_string(),
            client_id: "custom-id".to_string(),
            ..Default::default()
        };
        save_settings_file(&path, &s).unwrap();
        assert_eq!(load_settings_file(&path), s);
        // Corrupt file → defaults, never panics.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(load_settings_file(&path), SettingsDto::default());
    }

    #[test]
    fn settings_file_accepts_legacy_client_id_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = settings_path(dir.path());
        std::fs::write(&path, r#"{"msa_client_id": "legacy-1"}"#).unwrap();
        assert_eq!(load_settings_file(&path).client_id, "legacy-1");
        std::fs::write(&path, r#"{"auth": {"client_id": "nested-1"}}"#).unwrap();
        assert_eq!(load_settings_file(&path).client_id, "nested-1");
        // Explicit `client_id` wins over the legacy alias.
        std::fs::write(
            &path,
            r#"{"client_id": "new", "msa_client_id": "legacy-1"}"#,
        )
        .unwrap();
        assert_eq!(load_settings_file(&path).client_id, "new");
    }

    #[test]
    fn client_id_priority_settings_then_env_then_none() {
        let saved = std::env::var("PRISM_MSA_CLIENT_ID").ok();
        // Settings override wins over env.
        std::env::set_var("PRISM_MSA_CLIENT_ID", "env-id");
        let s = SettingsDto {
            client_id: "settings-id".to_string(),
            ..Default::default()
        };
        assert_eq!(effective_client_id(&s).as_deref(), Some("settings-id"));
        // Env fallback.
        assert_eq!(
            effective_client_id(&SettingsDto::default()).as_deref(),
            Some("env-id")
        );
        // Placeholder / empty → None.
        std::env::set_var("PRISM_MSA_CLIENT_ID", PLACEHOLDER_CLIENT_ID);
        assert!(effective_client_id(&SettingsDto::default()).is_none());
        std::env::remove_var("PRISM_MSA_CLIENT_ID");
        assert!(effective_client_id(&SettingsDto::default()).is_none());
        match saved {
            Some(v) => std::env::set_var("PRISM_MSA_CLIENT_ID", v),
            None => std::env::remove_var("PRISM_MSA_CLIENT_ID"),
        }
    }

    #[test]
    fn login_gate_blocks_launch_when_logged_out() {
        let s = AppState::default();
        assert!(!s.is_logged_in());
        let err = s.ensure_logged_in().unwrap_err();
        assert!(is_login_required_error(&err));
        assert_eq!(err, login_required_error());
        assert_eq!(s.launch_argv().unwrap_err(), "Manifeste introuvable.");
        // Manifest present but no session → LOGIN_REQUIRED.
        let s = AppState {
            manifest: Some(manifest()),
            ..Default::default()
        };
        assert_eq!(s.launch_argv().unwrap_err(), login_required_error());
    }

    #[test]
    fn launch_snapshot_then_from_equals_direct_argv() {
        // Garde anti-régression du découpage verrou court : l'instantané
        // cloné + construction hors mutex donne exactement le même
        // lancement que la méthode directe (comportement identique, le
        // balayage bytecode des jars ne tient plus le mutex).
        for settings in [
            SettingsDto::default(),
            SettingsDto {
                jvm_preset: "lowram".to_string(),
                resolution: "1280x720".to_string(),
                fullscreen: true,
                ..Default::default()
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut s = logged_in(dir.path());
            s.settings = settings;
            let direct = s.launch_argv().expect("direct");
            let (manifest, session, game_dir, snap_settings) =
                s.launch_snapshot().expect("snapshot");
            let via = AppState::launch_argv_from(&manifest, &session, &game_dir, &snap_settings)
                .expect("via snapshot");
            assert_eq!(direct, via);
        }
    }

    #[test]
    fn launch_argv_uses_msa_session() {
        let dir = tempfile::tempdir().unwrap();
        let s = logged_in(dir.path());
        assert!(s.is_logged_in());
        let (argv, _, _) = s.launch_argv().expect("session + manifest launch");
        assert!(argv.iter().any(|a| a == "Steve"));
        assert!(argv.iter().any(|a| a == "tok"));
        assert!(!argv.iter().any(|a| a == "legacy"));
        assert!(!argv.iter().any(|a| a == "0"));
        assert!(s
            .launch_command_string()
            .unwrap()
            .contains("net.minecraft.launchwrapper.Launch"));
        let info = SessionInfo::from(s.active_session().unwrap());
        assert_eq!(info.name, "Steve");
        assert_eq!(info.uuid_short, "123e4567…");
    }

    #[test]
    fn launch_argv_applies_jvm_preset_resolution_and_fullscreen() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = logged_in(dir.path());
        // Defaults: 46anarchy preset + 1920x1080, no fullscreen, no warnings.
        let (argv, _, warnings) = s.launch_argv().expect("launch");
        assert!(warnings.is_empty(), "unexpected: {warnings:?}");
        assert!(argv.iter().any(|a| a == "-XX:+ParallelRefProcEnabled"));
        assert!(argv
            .iter()
            .any(|a| a == "-Dfml.ignorePatchDiscrepancies=true"));
        assert!(argv.iter().any(|a| a == LOG4J_NOLOOKUPS_FLAG));
        assert!(argv.iter().any(|a| a == FSPATCHES_CDN_BYPASS_FLAG));
        let w = argv
            .iter()
            .position(|a| a == "--width")
            .expect("width flag");
        assert_eq!(argv[w + 1], "1920");
        assert_eq!(
            argv[argv.iter().position(|a| a == "--height").unwrap() + 1],
            "1080"
        );
        assert!(!argv.iter().any(|a| a == "--fullscreen"));
        // Preset switch + fullscreen + custom resolution.
        s.settings.jvm_preset = "lowram".to_string();
        s.settings.resolution = "1280 x 720".to_string();
        s.settings.fullscreen = true;
        let (argv, _, warnings) = s.launch_argv().expect("launch");
        assert!(warnings.is_empty(), "unexpected: {warnings:?}");
        assert!(argv.iter().any(|a| a == "-XX:+UseConcMarkSweepGC"));
        assert!(argv.iter().any(|a| a == "--fullscreen"));
        let w = argv.iter().position(|a| a == "--width").unwrap();
        assert_eq!(
            (argv[w + 1].as_str(), argv[w + 3].as_str()),
            ("1280", "720")
        );
        // Custom textarea content is spliced before -cp.
        s.settings.jvm_preset = "custom".to_string();
        s.settings.jvm_args = "-Dfoo=1 -Dbar=2".to_string();
        let (argv, _, _) = s.launch_argv().expect("launch");
        let cp = argv.iter().position(|a| a == "-cp").expect("-cp");
        assert!(argv[..cp].iter().any(|a| a == "-Dfoo=1"));
        assert!(argv[..cp].iter().any(|a| a == "-Dbar=2"));
        // Garbage resolution warns instead of silently vanishing.
        s.settings.resolution = "nawak".to_string();
        let (argv, _, warnings) = s.launch_argv().expect("launch");
        assert!(!argv.iter().any(|a| a == "--width"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nawak"), "unexpected: {warnings:?}");
    }

    #[test]
    fn experimental_launch_uses_rfb_wall_and_java21() {
        let dir = tempfile::tempdir().unwrap();
        // Seed a minimal upstream profile cache (one lib, no rules) under
        // the EFFECTIVE game dir (`paladium` subdir — tripwire).
        let game_dir = launcher_core::normalize_game_dir(dir.path());
        let caches = game_dir.join("caches");
        std::fs::create_dir_all(&caches).unwrap();
        std::fs::write(
            caches.join(launcher_core::MODERN_CACHE_FILE),
            serde_json::json!({
                "id": "test",
                "libraries": [
                    {"name": "org.example:lib:1.0",
                     "downloads": {"artifact": {"path": "org/example/lib/1.0/lib-1.0.jar",
                         "sha1": "a", "size": 1, "url": "https://example.com/l.jar"}}}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let mut s = logged_in(dir.path());
        s.settings.experimental_runtime = true;
        // A stale `java_runtime` file value is ignored: experimental always
        // resolves GraalVM 21 (see `java_runtime_for_flavor`).
        s.settings.java_runtime = "temurin8".to_string();
        // Fake provisioned GraalVM (script printing a 21 version line —
        // no real JVM needed; `check_java` only parses `-version` output).
        let java21 = game_dir.join(launcher_core::MODERN_JAVA_DIR).join("bin");
        std::fs::create_dir_all(&java21).unwrap();
        let fake = java21.join(launcher_core::java_exe_name(launcher_core::Os::current()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&fake, "#!/bin/sh\necho 'openjdk version \"21.0.2\"' >&2\n").unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (argv, _, warnings) = s.launch_argv().expect("experimental launch");
        // Le preset classique (G1GC) voyage avec la saveur expérimentale :
        // le sélecteur adverse est retiré LOUDLY (pas de « Multiple garbage
        // collectors selected » au démarrage), le ZGC canonique reste unique.
        assert_eq!(warnings.len(), 1, "unexpected: {warnings:?}");
        assert!(
            warnings[0].contains("-XX:+UseG1GC"),
            "cause visible : {warnings:?}"
        );
        assert!(!argv.iter().any(|a| a == "-XX:+UseG1GC"));
        assert_eq!(argv.iter().filter(|a| *a == "-XX:+UseZGC").count(), 1);
        assert_eq!(
            argv.iter().filter(|a| *a == "-XX:+ZGenerational").count(),
            1
        );
        // FS-Patches updater opt-out: present on the experimental flavor too
        // (same mods dir, same fspatches — launcher-owned install/verify).
        assert!(argv.iter().any(|a| a == FSPATCHES_CDN_BYPASS_FLAG));
        // Java 21 binary up front (no override set).
        assert!(argv[0].ends_with("java21/bin/java") || argv[0].ends_with("java21\\bin\\java.exe"));
        // No dead Java 8 flags; the wall + RFB entry point present.
        assert!(!argv.iter().any(|a| a == "-Xverify:none"));
        assert!(argv
            .iter()
            .any(|a| a == launcher_core::MODERN_CLASSLOADER_FLAG));
        assert!(argv.iter().any(|a| a == "--add-opens"));
        // Tuned ZGC (generational, product — no unlock dance) plus the
        // stencil flag Zephyr-UI requires.
        assert!(argv.iter().any(|a| a == "-XX:+UseZGC"));
        assert!(argv.iter().any(|a| a == "-XX:+ZGenerational"));
        assert!(!argv.iter().any(|a| a.contains("G1NewSizePercent")));
        assert!(argv.iter().any(|a| a == launcher_core::MODERN_STENCIL_FLAG));
        // Classpath: profile lib in order, vanilla jar last, RFB main after.
        let cp = argv.iter().position(|a| a == "-cp").expect("-cp");
        assert!(argv[cp + 1].contains("org/example/lib/1.0/lib-1.0.jar"));
        assert!(
            argv[cp + 1].ends_with("versions/1.7.10/1.7.10.jar")
                || argv[cp + 1].contains("versions/1.7.10/1.7.10.jar")
        );
        assert_eq!(argv[cp + 2], launcher_core::MODERN_MAIN_CLASS);
        // CMS preset is forced off loudly on Java 21.
        s.settings.jvm_preset = "lowram".to_string();
        let (argv, _, warnings) = s.launch_argv().expect("experimental launch");
        assert!(!argv.iter().any(|a| a == "-XX:+UseConcMarkSweepGC"));
        assert!(
            warnings.iter().any(|w| w.contains("lowram")),
            "{warnings:?}"
        );
        // Explicit `java_path` override still wins over the flavor runtime.
        s.settings.java_path = "/usr/bin/java".to_string();
        s.settings.jvm_preset = "46anarchy".to_string();
        let (argv, _, warnings) = s.launch_argv().expect("override launch");
        assert_eq!(argv[0], "/usr/bin/java");
        // L'override gagne, l'assainissement GC s'applique quand même.
        assert!(
            warnings.iter().any(|w| w.contains("-XX:+UseG1GC")),
            "{warnings:?}"
        );
        assert!(!argv.iter().any(|a| a == "-XX:+UseG1GC"));
        s.settings.java_path = String::new();
        // Flag off → classic Java 8 path again.
        s.settings.experimental_runtime = false;
        s.settings.jvm_preset = "46anarchy".to_string();
        let (argv, _, _) = s.launch_argv().expect("classic launch");
        assert!(!argv
            .iter()
            .any(|a| a == launcher_core::MODERN_CLASSLOADER_FLAG));
    }

    #[test]
    fn enforce_single_collector_experimental_keeps_single_zgc() {
        // Preset G1GC + customs + tuning mélangés : seule la famille
        // ZGC survit (canonique exactement une fois) ; réglages, `-Dfml…`,
        // mémoire et Epsilon restent intacts.
        let mut args: Vec<String> = JVM_PRESET_G1GC
            .split_whitespace()
            .map(str::to_string)
            .collect();
        args.extend(
            [
                "-Xmx4G",
                "-Xincgc",
                "-XX:+UseParallelGC",
                "-XX:+UseShenandoahGC",
                "-XX:-UseG1GC",
                "-XX:-UseZGC",
                "-XX:+UseZGC",
                "-XX:+ZGenerational",
                "-XX:+UseEpsilonGC",
                "-XX:-UseAdaptiveSizePolicy",
                "-Dfml.ignoreInvalidMinecraftCertificates=true",
            ]
            .into_iter()
            .map(str::to_string),
        );
        args.extend(
            launcher_core::MODERN_GC_TUNING
                .iter()
                .map(|s| s.to_string()),
        );
        let (kept, stripped) = enforce_single_collector(&args, true);
        // Famille classique + incgc + désactivation ZGC : retirés.
        for flag in [
            "-XX:+UseG1GC",
            "-Xincgc",
            "-XX:+UseParallelGC",
            "-XX:+UseShenandoahGC",
            "-XX:-UseG1GC",
            "-XX:-UseZGC",
        ] {
            assert!(stripped.contains(&flag.to_string()), "{flag} retiré");
            assert!(!kept.contains(&flag.to_string()), "{flag} absent");
        }
        // Canonique présent exactement une fois, Epsilon conservé.
        for flag in launcher_core::MODERN_GC_TUNING {
            assert_eq!(
                kept.iter().filter(|a| a.as_str() == *flag).count(),
                1,
                "{flag} unique"
            );
        }
        assert!(kept.contains(&"-XX:+UseEpsilonGC".to_string()));
        assert!(!stripped.contains(&"-XX:+UseEpsilonGC".to_string()));
        // Mémoire et non-sélecteurs intacts.
        for flag in [
            "-Xmx4G",
            "-XX:+ParallelRefProcEnabled",
            "-XX:MaxGCPauseMillis=150",
            "-XX:G1ReservePercent=15",
            "-XX:-UseAdaptiveSizePolicy",
            "-Dfml.ignoreInvalidMinecraftCertificates=true",
        ] {
            assert!(kept.contains(&flag.to_string()), "{flag} gardé");
        }
        // Autres collecteurs aussi (pas seulement G1GC).
        for flag in [
            "-XX:+UseParallelOldGC",
            "-XX:+UseConcMarkSweepGC",
            "-XX:+UseSerialGC",
        ] {
            let (kept, stripped) = enforce_single_collector(&[flag.to_string()], true);
            assert!(
                kept.iter()
                    .all(|a| !matches!(super::gc_selector_kind(a), Some("autre"))),
                "{flag}"
            );
            assert_eq!(stripped, vec![flag.to_string()]);
        }
    }

    #[test]
    fn enforce_single_collector_classic_strips_zgc_only() {
        // Classique : ZGC retiré (`+` comme `-`), G1GC du preset et `-Xincgc`
        // (CMS Java 8) conservés, customs et ordre intacts.
        let args = [
            "-XX:+UseG1GC",
            "-XX:+UseZGC",
            "-XX:+ZGenerational",
            "-XX:-UseZGC",
            "-XX:-ZGenerational",
            "-Xincgc",
            "-XX:+UseEpsilonGC",
            "-Dfml.ignoreInvalidMinecraftCertificates=true",
            "-Xmx4G",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        let (kept, stripped) = enforce_single_collector(&args, false);
        assert_eq!(
            kept,
            [
                "-XX:+UseG1GC",
                "-Xincgc",
                "-XX:+UseEpsilonGC",
                "-Dfml.ignoreInvalidMinecraftCertificates=true",
                "-Xmx4G",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        assert_eq!(
            stripped,
            [
                "-XX:+UseZGC",
                "-XX:+ZGenerational",
                "-XX:-UseZGC",
                "-XX:-ZGenerational",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
        // Réglages nommés sans sélecteur : jamais retirés (inoffensifs).
        let (kept, stripped) = enforce_single_collector(
            &[
                "-XX:-UseAdaptiveSizePolicy".to_string(),
                "-Xmn512m".to_string(),
            ],
            true,
        );
        assert!(stripped.is_empty());
        assert!(kept.contains(&"-XX:-UseAdaptiveSizePolicy".to_string()));
        assert!(kept.contains(&"-Xmn512m".to_string()));
    }

    #[test]
    fn enforce_single_collector_empty_input() {
        // Expérimental : même sans entrée, le canonique est garanti (le
        // lancement ne part jamais sans collecteur défini).
        let (kept, stripped) = enforce_single_collector(&[], true);
        assert!(stripped.is_empty());
        assert_eq!(
            kept,
            launcher_core::MODERN_GC_TUNING
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        let (kept, stripped) = enforce_single_collector(&[], false);
        assert!(kept.is_empty());
        assert!(stripped.is_empty());
    }

    /// Familles de collecteurs présentes dans un argv final (`zgc` et/ou
    /// `classique` ; Epsilon ne compte pas — jamais un conflit).
    fn collector_families(argv: &[String]) -> Vec<&'static str> {
        let mut out = Vec::new();
        for a in argv {
            let fam = match super::gc_selector_kind(a) {
                Some("zgc") | Some("zgenerational") => "zgc",
                Some("autre") | Some("incgc") => "classique",
                _ => continue,
            };
            if !out.contains(&fam) {
                out.push(fam);
            }
        }
        out
    }

    #[test]
    fn launch_argv_post_assembly_never_mixes_collectors() {
        // Intégration argv final : preset-46anarchy+G1GC sur expérimental →
        // ZGC unique + avertissement post-assemblage ; miroir classique.
        let dir = tempfile::tempdir().unwrap();
        let game_dir = launcher_core::normalize_game_dir(dir.path());
        let caches = game_dir.join("caches");
        std::fs::create_dir_all(&caches).unwrap();
        std::fs::write(
            caches.join(launcher_core::MODERN_CACHE_FILE),
            serde_json::json!({
                "id": "test",
                "libraries": [
                    {"name": "org.example:lib:1.0",
                     "downloads": {"artifact": {"path": "org/example/lib/1.0/lib-1.0.jar",
                         "sha1": "a", "size": 1, "url": "https://example.com/l.jar"}}}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let mut s = logged_in(dir.path());
        s.settings.experimental_runtime = true;
        // Preset classique + customs + tuning : tout porte des sélecteurs.
        s.settings.jvm_preset = "custom".to_string();
        s.settings.jvm_args = format!(
            "{} {} -Xmx4G",
            JVM_PRESET_46ANARCHY,
            launcher_core::MODERN_GC_TUNING.join(" ")
        );
        let (argv, _, warnings) = s.launch_argv().expect("experimental full-stack");
        assert_eq!(collector_families(&argv), vec!["zgc"]);
        assert_eq!(argv.iter().filter(|a| *a == "-XX:+UseZGC").count(), 1);
        assert!(argv.iter().any(|a| a == "-Xmx4G"));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("post-assemblage") && w.contains("-XX:+UseG1GC")),
            "{warnings:?}"
        );
        // Miroir classique : ZGC copié retiré, une seule famille.
        s.settings.experimental_runtime = false;
        s.settings.jvm_args = "-XX:+UseZGC -XX:+ZGenerational -XX:+UseG1GC -Xmx4G".to_string();
        let (argv, _, warnings) = s.launch_argv().expect("classic mirror");
        assert_eq!(collector_families(&argv), vec!["classique"]);
        assert!(!argv.iter().any(|a| a == "-XX:+UseZGC"));
        assert!(argv.iter().any(|a| a == "-Xmx4G"));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("post-assemblage") && w.contains("-XX:+UseZGC")),
            "{warnings:?}"
        );
    }

    #[test]
    fn gc_spawn_tokens_lists_only_collector_flags() {
        // Diagnostic au spawn : jetons GC seuls, jamais la commande complète.
        let argv = [
            "/usr/bin/java",
            "-Xmx4G",
            "-XX:+UseG1GC",
            "-XX:MaxGCPauseMillis=200",
            "-Dfoo=1",
            "-cp",
            "a.jar:b.jar",
            "net.minecraft.launchwrapper.Launch",
            "--username",
            "Steve",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(super::gc_spawn_tokens(&argv), vec!["-XX:+UseG1GC"]);
        let empty: Vec<String> = vec!["/usr/bin/java".to_string(), "-Xmx4G".to_string()];
        assert!(super::gc_spawn_tokens(&empty).is_empty());
    }

    #[test]
    fn launch_argv_never_mixes_collectors() {
        // Custom expérimental hérité de l'ère G1GC (le bug « Multiple garbage
        // collectors selected ») : nettoyé + ZGC unique + cause visible.
        let dir = tempfile::tempdir().unwrap();
        let mut s = logged_in(dir.path());
        s.settings.experimental_runtime = true;
        s.settings.jvm_preset = "custom".to_string();
        s.settings.jvm_args =
            "-XX:+UseG1GC -XX:MaxGCPauseMillis=200 -Dfml.custom=1 -Xmx4G".to_string();
        // Profil LWJGL3 minimal requis par le chemin expérimental (une
        // lib, comme `experimental_launch_uses_rfb_wall_and_java21` — un
        // profil à `libraries` vide est refusé par le chargeur).
        let game_dir = launcher_core::normalize_game_dir(dir.path());
        let caches = game_dir.join("caches");
        std::fs::create_dir_all(&caches).unwrap();
        std::fs::write(
            caches.join(launcher_core::MODERN_CACHE_FILE),
            serde_json::json!({
                "id": "test",
                "libraries": [
                    {"name": "org.example:lib:1.0",
                     "downloads": {"artifact": {"path": "org/example/lib/1.0/lib-1.0.jar",
                         "sha1": "a", "size": 1, "url": "https://example.com/l.jar"}}}
                ]
            })
            .to_string(),
        )
        .unwrap();
        // Faux GraalVM provisionné (même script que le test voisin : seule
        // la ligne `-version` est parsée, aucun vrai JVM requis) pour que
        // les avertissements ne parlent que du GC.
        let java21 = game_dir.join(launcher_core::MODERN_JAVA_DIR).join("bin");
        std::fs::create_dir_all(&java21).unwrap();
        let fake = java21.join(launcher_core::java_exe_name(launcher_core::Os::current()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&fake, "#!/bin/sh\necho 'openjdk version \"21.0.2\"' >&2\n").unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (argv, _, warnings) = s.launch_argv().expect("experimental custom");
        assert!(!argv.iter().any(|a| a == "-XX:+UseG1GC"));
        assert_eq!(argv.iter().filter(|a| *a == "-XX:+UseZGC").count(), 1);
        assert!(argv.iter().any(|a| a == "-Dfml.custom=1"));
        assert!(argv.iter().any(|a| a == "-Xmx4G"));
        assert!(argv.iter().any(|a| a == "-XX:MaxGCPauseMillis=200"));
        assert!(
            warnings.iter().any(|w| w.contains("-XX:+UseG1GC")),
            "{warnings:?}"
        );
        // ZGC déjà présent dans le custom : pas de doublon, silence total.
        s.settings.jvm_args = JVM_PRESET_GRAAL_ZGC.to_string();
        let (argv, _, warnings) = s.launch_argv().expect("experimental zgc custom");
        assert_eq!(argv.iter().filter(|a| *a == "-XX:+UseZGC").count(), 1);
        assert_eq!(
            argv.iter().filter(|a| *a == "-XX:+ZGenerational").count(),
            1
        );
        assert!(warnings.is_empty(), "unexpected: {warnings:?}");
        // Miroir classique : ZGC copié retiré, G1GC du preset conservé.
        s.settings.experimental_runtime = false;
        s.settings.jvm_preset = "g1gc".to_string();
        s.settings.jvm_args = String::new();
        let (argv, _, warnings) = s.launch_argv().expect("classic g1gc");
        assert!(warnings.is_empty(), "unexpected: {warnings:?}");
        assert!(argv.iter().any(|a| a == "-XX:+UseG1GC"));
        s.settings.jvm_preset = "custom".to_string();
        s.settings.jvm_args = "-XX:+UseZGC -XX:+ZGenerational -Dfml.custom=1".to_string();
        let (argv, _, warnings) = s.launch_argv().expect("classic zgc custom");
        assert!(!argv.iter().any(|a| a == "-XX:+UseZGC"));
        assert!(!argv.iter().any(|a| a == "-XX:+ZGenerational"));
        assert!(argv.iter().any(|a| a == "-Dfml.custom=1"));
        assert!(
            warnings.iter().any(|w| w.contains("-XX:+UseZGC")),
            "{warnings:?}"
        );
    }

    #[test]
    fn play_result_old_json_without_warnings_still_parses() {
        let dto: PlayResultDto = serde_json::from_str(r#"{"command": "java -jar x"}"#).unwrap();
        assert!(dto.warnings.is_empty());
    }

    #[test]
    fn manifest_status_keys() {
        let s = AppState::default();
        let st = s.manifest_status();
        assert_eq!(st.update_status, "init");
        assert_eq!(st.name, APP_NAME_FALLBACK);
        let dir = tempfile::tempdir().unwrap();
        let s = logged_in(dir.path());
        let st = s.manifest_status();
        assert_eq!(st.update_status, "ok");
        assert_eq!(st.name, "Paladium V11");
        assert_eq!(st.mc_version, "1.7.10");
        assert!(!st.maintenance_enabled);
    }

    #[test]
    fn install_progress_math_and_bytes() {
        // `completed_bytes` carries the in-flight chunk already: fraction is
        // bytes/total, verbatim (`current_bytes` is per-file info only).
        let update = ProgressUpdate {
            completed_files: 1,
            total_files: 4,
            completed_bytes: 150,
            total_bytes: 400,
            current_file: "mods/a.jar".to_string(),
            current_bytes: 50,
            current_total: 100,
            skipped: false,
        };
        assert!((progress_fraction(&update) - 0.375).abs() < f32::EPSILON);
        let payload = ProgressPayload::from(&update);
        assert_eq!(
            payload.detail,
            "Fichier 1/4 — mods/a.jar — 150/400 o (38 %)"
        );
        assert_eq!(human_bytes(512), "512 o");
        assert_eq!(human_bytes(2048), "2.0 Kio");
    }

    #[test]
    fn install_progress_fraction_is_bytes_over_total() {
        // Unified model: every sampled emit satisfies fraction == done/total.
        for (done, total) in [(0, 400), (1, 400), (150, 400), (399, 400), (400, 400)] {
            let update = ProgressUpdate {
                completed_files: 1,
                total_files: 4,
                completed_bytes: done,
                total_bytes: total,
                current_file: "mods/a.jar".to_string(),
                current_bytes: done.min(100),
                current_total: 100,
                skipped: false,
            };
            let expected = (done as f32 / total as f32).clamp(0.0, 1.0);
            assert!(
                (progress_fraction(&update) - expected).abs() < f32::EPSILON,
                "done={done} total={total}"
            );
            let payload = ProgressPayload::from(&update);
            assert!(
                (payload.fraction - expected).abs() < f32::EPSILON,
                "done={done} total={total}"
            );
        }
        // Byte total unknown → file-count fallback; nothing known → 0.0.
        let no_bytes = ProgressUpdate {
            completed_files: 1,
            total_files: 4,
            completed_bytes: 0,
            total_bytes: 0,
            current_file: String::new(),
            current_bytes: 0,
            current_total: 0,
            skipped: false,
        };
        assert!((progress_fraction(&no_bytes) - 0.25).abs() < f32::EPSILON);
        let empty = ProgressUpdate {
            total_files: 0,
            total_bytes: 0,
            completed_files: 0,
            completed_bytes: 0,
            current_file: String::new(),
            current_bytes: 0,
            current_total: 0,
            skipped: false,
        };
        assert_eq!(progress_fraction(&empty), 0.0);
    }

    #[test]
    fn install_progress_detail_format() {
        // Shape: `Fichier i/n — name — x/y unit (p %)` (French comma).
        let detail = format_progress_detail(
            12,
            48,
            "Apollon-9.1.0.jar",
            12_996_000,
            45_302_000,
            12_996_000 as f32 / 45_302_000 as f32,
        );
        assert_eq!(
            detail,
            "Fichier 12/48 — Apollon-9.1.0.jar — 12,4/43,2 Mo (29 %)"
        );
        // Small files stay in octets, no decimal.
        assert_eq!(
            format_progress_detail(1, 4, "mods/a.jar", 150, 400, 0.375),
            "Fichier 1/4 — mods/a.jar — 150/400 o (38 %)"
        );
        // Verify-flavored line for hash-check skips.
        assert_eq!(
            format_verify_detail(12, 48, "foo.jar"),
            "Vérification 12/48 — foo.jar"
        );
    }

    /// Frontend/backend event contract for `install-progress`: `main.js`
    /// `renderProgress` reads `p.fraction` + `p.detail` (a renamed or
    /// missing field silently freezes the game-install bar).
    #[test]
    fn install_progress_payload_contract_matches_frontend() {
        let update = ProgressUpdate {
            completed_files: 4,
            total_files: 4,
            completed_bytes: 400,
            total_bytes: 400,
            current_file: "mods/a.jar".to_string(),
            current_bytes: 0,
            current_total: 100,
            skipped: false,
        };
        let value = serde_json::to_value(ProgressPayload::from(&update)).unwrap();
        let obj = value.as_object().unwrap();
        for key in [
            "completed_files",
            "total_files",
            "completed_bytes",
            "total_bytes",
            "current_file",
            "fraction",
            "detail",
        ] {
            assert!(obj.contains_key(key), "missing payload field: {key}");
        }
        assert_eq!(obj["fraction"], serde_json::json!(1.0));
        assert!(obj["detail"].as_str().unwrap().contains("Fichier 4/4"));
        // No speed/eta fields exist: the game modal must not show any
        // (it renders bar + pct + file + detail only; speed is client-side).
        assert!(!obj.contains_key("speed"));
        assert!(!obj.contains_key("eta"));
    }

    fn quarantined_state() -> AppState {
        let mut s = AppState::default();
        for (uuid, name) in [("uuid-a", "Alice"), ("uuid-b", "Bob")] {
            s.upsert_session(MinecraftSession {
                uuid: uuid.to_string(),
                name: name.to_string(),
                mc_token: "tok".to_string(),
                refresh_token: "ref".to_string(),
                obtained: std::time::Instant::now(),
            });
            s.settings.accounts.push(AccountRef {
                uuid: uuid.to_string(),
                name: name.to_string(),
            });
        }
        s.active_account = Some("uuid-a".to_string());
        s.settings.active_account = Some("uuid-a".to_string());
        s
    }

    #[test]
    fn quarantaine_du_compte_actif_mort_conserve_le_registre() {
        let mut s = quarantined_state();
        s.quarantine_dead_session("uuid-a");
        // Session cadavre abandonnée, survivant intact.
        assert!(s.sessions.iter().all(|x| x.uuid != "uuid-a"));
        assert_eq!(s.sessions.len(), 1);
        // Entrée du registre CONSERVÉE avec son dernier nom connu.
        let kept = s
            .settings
            .accounts
            .iter()
            .find(|a| a.uuid == "uuid-a")
            .expect("entrée conservée");
        assert_eq!(kept.name, "Alice");
        // Pointeur actif replié sur le survivant (mémoire + fichier).
        assert_eq!(s.active_account.as_deref(), Some("uuid-b"));
        assert_eq!(s.settings.active_account.as_deref(), Some("uuid-b"));
    }

    #[test]
    fn quarantaine_du_dernier_compte_mort_efface_l_actif() {
        let mut s = AppState::default();
        s.upsert_session(MinecraftSession {
            uuid: "uuid-seul".to_string(),
            name: "Solo".to_string(),
            mc_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            obtained: std::time::Instant::now(),
        });
        s.settings.accounts.push(AccountRef {
            uuid: "uuid-seul".to_string(),
            name: "Solo".to_string(),
        });
        s.settings.active_account = Some("uuid-seul".to_string());
        s.quarantine_dead_session("uuid-seul");
        assert!(s.sessions.is_empty());
        assert!(!s.is_logged_in());
        let kept = s
            .settings
            .accounts
            .iter()
            .find(|a| a.uuid == "uuid-seul")
            .expect("entrée conservée");
        assert_eq!(kept.name, "Solo");
        assert!(s.active_account.is_none());
        assert!(s.settings.active_account.is_none());
    }

    #[test]
    fn quarantaine_du_compte_non_actif_ne_touche_pas_l_actif() {
        let mut s = quarantined_state();
        s.quarantine_dead_session("uuid-b");
        assert!(s.sessions.iter().all(|x| x.uuid != "uuid-b"));
        let kept = s
            .settings
            .accounts
            .iter()
            .find(|a| a.uuid == "uuid-b")
            .expect("entrée conservée");
        assert_eq!(kept.name, "Bob");
        assert_eq!(s.active_account.as_deref(), Some("uuid-a"));
        assert_eq!(s.settings.active_account.as_deref(), Some("uuid-a"));
    }

    /// Frontend/backend event contract for `game-outcome`: `main.js`
    /// listens `o.status` (`running` | `exited` | `crashed`), journals
    /// `o.detail` and toasts `o.cause` on crashes (a renamed or missing
    /// field silently drops the verdict and the journal dead-ends again).
    #[test]
    fn game_outcome_payload_contract_matches_frontend() {
        let crashed = GameOutcomeDto {
            status: "crashed".to_string(),
            code: Some(1),
            cause: Some("Caused by: java.lang.NoSuchMethodError: boom".to_string()),
            detail: "Plantage détecté (PID 4242) : boom — voir la Console du jeu.".to_string(),
        };
        let value = serde_json::to_value(&crashed).unwrap();
        let obj = value.as_object().unwrap();
        for key in ["status", "code", "cause", "detail"] {
            assert!(obj.contains_key(key), "missing payload field: {key}");
        }
        assert_eq!(obj["status"], serde_json::json!("crashed"));
        assert!(obj["detail"].as_str().unwrap().contains("Plantage"));
        let running = GameOutcomeDto {
            status: "running".to_string(),
            code: None,
            cause: None,
            detail: "Jeu en cours d'exécution (PID 4242).".to_string(),
        };
        let value = serde_json::to_value(&running).unwrap();
        assert_eq!(value["status"], serde_json::json!("running"));
        assert!(value["code"].is_null());
        assert!(value["cause"].is_null());
    }

    /// Frontend/backend event contract for `java-progress`: `main.js`
    /// `renderJavaProgress` reads `p.fraction` + `p.detail` +
    /// `p.downloaded_bytes` + `p.total_bytes` (same silent-freeze rule).
    #[test]
    fn java_progress_payload_contract_matches_frontend() {
        let value = serde_json::to_value(JavaProgressPayload::from_bytes(50, 200)).unwrap();
        let obj = value.as_object().unwrap();
        for key in ["downloaded_bytes", "total_bytes", "fraction", "detail"] {
            assert!(obj.contains_key(key), "missing payload field: {key}");
        }
        assert!((obj["fraction"].as_f64().unwrap() - 0.25).abs() < f32::EPSILON as f64);
        assert!(obj["detail"].as_str().unwrap().contains("25%"));
        let unknown = serde_json::to_value(JavaProgressPayload::from_bytes(512, 0)).unwrap();
        assert_eq!(unknown["fraction"], serde_json::json!(0.0));
    }

    #[test]
    fn experimental_gate_both_positions() {
        // Interrupteur verrouillé : une sélection expérimentale active
        // retombe sur défaut (les deux drapeaux), une seule fois.
        let mut s = SettingsDto {
            experimental_runtime: true,
            experimental_optimizations: true,
            ..Default::default()
        };
        assert!(apply_experimental_gate_to(&mut s, true));
        assert!(!s.experimental_runtime);
        assert!(!s.experimental_optimizations);
        // Idempotent : second passage silencieux.
        assert!(!apply_experimental_gate_to(&mut s, true));
        // Partiel (un seul drapeau) : migré aussi.
        s.experimental_optimizations = true;
        assert!(apply_experimental_gate_to(&mut s, true));
        assert!(!s.experimental_optimizations);
        // Déjà sur défaut : rien ne bouge.
        assert!(!apply_experimental_gate_to(
            &mut SettingsDto::default(),
            true
        ));
        // Interrupteur réactivé (`false`) : comportement antérieur, la
        // sélection expérimentale est conservée telle quelle.
        let mut open = SettingsDto {
            experimental_runtime: true,
            experimental_optimizations: true,
            ..Default::default()
        };
        assert!(!apply_experimental_gate_to(&mut open, false));
        assert!(open.experimental_runtime);
        assert!(open.experimental_optimizations);
        // Verdicts du refus : erreur française sous verrou uniquement.
        assert!(experimental_gate_rejects(true, true)
            .expect("refus sous verrou")
            .contains("temporairement"));
        assert!(experimental_gate_rejects(true, false).is_none());
        assert!(experimental_gate_rejects(false, true).is_none());
        assert!(experimental_gate_rejects(false, false).is_none());
    }
}
