// SPDX-License-Identifier: AGPL-3.0-only

//! `launcher-auth`: Microsoft (device-code) + Xbox Live + Minecraft auth.
//!
//! Implements the exact chain used by Prism-like launchers:
//! 1. `POST .../devicecode` (MSA device code)
//! 2. `POST .../token` polling (`grant_type=urn:ietf:params:oauth:grant-type:device_code`)
//! 3. `POST https://user.auth.xboxlive.com/user/authenticate` (XBL)
//! 4. `POST https://xsts.auth.xboxlive.com/xsts/authorize` (XSTS, `XErr` mapping)
//! 5. `POST https://api.minecraftservices.com/authentication/login_with_xbox`
//! 6. `GET entitlements/mcstore` + `GET minecraft/profile`
//! 7. Refresh flow via `refresh_token`.
//!
//! The Minecraft `access_token` is kept in memory only ([`MinecraftSession`]);
//! only the MSA `refresh_token` is persisted via the OS keyring
//! (service [`KEYRING_SERVICE`]).

use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Microsoft device-code endpoint (consumers tenant).
pub const MS_DEVICE_CODE_URL: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
/// Microsoft token endpoint (consumers tenant).
pub const MS_TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
/// Xbox Live user authentication endpoint.
pub const XBL_AUTH_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
/// XSTS authorization endpoint.
pub const XSTS_AUTH_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
/// Minecraft Services login-with-Xbox endpoint.
pub const MC_LOGIN_WITH_XBOX_URL: &str =
    "https://api.minecraftservices.com/authentication/login_with_xbox";
/// Minecraft Services entitlements endpoint.
pub const MC_ENTITLEMENTS_URL: &str = "https://api.minecraftservices.com/entitlements/mcstore";
/// Minecraft Services profile endpoint.
pub const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

/// OAuth scope for the device-code flow.
pub const MSA_SCOPE: &str = "XboxLive.signin offline_access";

/// Keyring service used to store the MSA refresh token.
pub const KEYRING_SERVICE: &str = "fr.46anarchy.launcher";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Placeholder client id used when no id was injected at build time.
///
/// The Prism/Azure client id must be injected at build time via the
/// `PRISM_MSA_CLIENT_ID` compile-time env var
/// (`PRISM_MSA_CLIENT_ID=... cargo build`), or overridden at runtime via a
/// settings file ([`AuthConfig::from_settings_file`]) or the same env var.
/// The well-known Prism id (`00000000402b5328`) is deliberately NOT hardcoded
/// here as the production value: shipping it as default would leak a shared
/// secret-ish id and break once its registration is rotated; builds must pass
/// their own Azure app registration id.
pub const PLACEHOLDER_CLIENT_ID: &str = "00000000-0000-0000-0000-000000000000";

/// Authentication configuration (Azure app registration id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Azure application (client) id used for the MSA OAuth flow.
    pub client_id: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        // Compile-time injection; falls back to an obviously-invalid
        // placeholder so a misconfigured build fails loudly at login time
        // instead of silently using someone else's registration.
        let id = option_env!("PRISM_MSA_CLIENT_ID").unwrap_or(PLACEHOLDER_CLIENT_ID);
        Self {
            client_id: id.to_string(),
        }
    }
}

impl AuthConfig {
    /// Config with an explicit client id (e.g. custom Azure registration).
    pub fn with_client_id(id: impl Into<String>) -> Self {
        Self {
            client_id: id.into(),
        }
    }

    /// Runtime env override: `PRISM_MSA_CLIENT_ID`, if set and non-empty.
    pub fn from_env() -> Self {
        match std::env::var("PRISM_MSA_CLIENT_ID") {
            Ok(v) if !v.trim().is_empty() => Self::with_client_id(v.trim().to_string()),
            _ => Self::default(),
        }
    }

    /// Load `{ "client_id": "..." }` (also accepts `msa_client_id`) from a
    /// JSON settings file, falling back to [`Self::from_env`] on any error.
    ///
    /// This is the "settings file override" hook: point it at the launcher
    /// settings JSON so users/packagers can inject their own Azure id
    /// without rebuilding.
    pub fn from_settings_file(path: impl AsRef<Path>) -> Self {
        let fallback = Self::from_env();
        let Ok(bytes) = std::fs::read(path.as_ref()) else {
            return fallback;
        };
        let Ok(v): Result<serde_json::Value, _> = serde_json::from_slice(&bytes) else {
            return fallback;
        };
        let id = v
            .get("client_id")
            .or_else(|| v.get("msa_client_id"))
            .or_else(|| v.get("auth").and_then(|a| a.get("client_id")))
            .and_then(|s| s.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match id {
            Some(id) => Self::with_client_id(id.to_string()),
            None => fallback,
        }
    }

    fn validate(&self) -> Result<(), AuthError> {
        if self.client_id.trim().is_empty() || self.client_id == PLACEHOLDER_CLIENT_ID {
            return Err(AuthError::MissingClientId);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Response of the MSA device-code endpoint (step 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    /// Code the user types on the verification page.
    pub user_code: String,
    /// Device code used while polling the token endpoint.
    pub device_code: String,
    /// Page where the user approves the login.
    pub verification_uri: String,
    /// Optional complete uri (embeds the user code).
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Lifetime of the device code, seconds.
    pub expires_in: u64,
    /// Recommended poll interval, seconds.
    #[serde(default = "default_poll_interval")]
    pub interval: u64,
    /// Human-readable instructions shown to the user.
    #[serde(default)]
    pub message: String,
}

fn default_poll_interval() -> u64 {
    5
}

/// MSA OAuth tokens (step 2 / refresh flow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsaToken {
    /// Short-lived token forwarded to Xbox Live (`RpsTicket: d=<token>`).
    pub access_token: String,
    /// Long-lived token persisted in the OS keyring.
    pub refresh_token: String,
    /// Lifetime of the access token, seconds.
    #[serde(default)]
    pub expires_in: u64,
}

/// Xbox authentication result (XBL and XSTS share the shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XboxAuth {
    /// `Token` field of the Xbox response (JWT).
    pub token: String,
    /// `DisplayClaims.xui[0].uhs` (user hash).
    pub uhs: String,
}

/// Minecraft Services access token (kept in memory only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinecraftToken {
    /// Bearer for `api.minecraftservices.com`.
    pub access_token: String,
    /// Lifetime, seconds (may be absent in tests).
    #[serde(default)]
    pub expires_in: u64,
}

/// Minecraft profile (`GET minecraft/profile`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinecraftProfile {
    /// Player UUID (32 hex chars, no dashes, as returned by the API).
    pub id: String,
    /// Player name.
    pub name: String,
}

/// In-memory game session.
///
/// `mc_token` (Minecraft access token) is intentionally kept in memory only
/// and never written to disk/keyring; only `refresh_token` (MSA) is persisted
/// via [`save_refresh_token`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinecraftSession {
    /// Player UUID.
    pub uuid: String,
    /// Player name.
    pub name: String,
    /// Minecraft Services access token (memory only, do NOT persist).
    pub mc_token: String,
    /// MSA refresh token (persisted in the OS keyring).
    pub refresh_token: String,
    /// When this session was minted (monotonic). The Minecraft token lives
    /// ~24h; callers refresh proactively before launching with a stale one.
    pub obtained: std::time::Instant,
}

/// `true` when the auth failure means the refresh token is dead
/// (`invalid_grant`: revoked, expired, or rotated away elsewhere). Only
/// this class justifies deleting the stored token + prompting re-login;
/// everything else (network, parsing) is retryable with the token kept.
pub fn is_invalid_grant(e: &AuthError) -> bool {
    matches!(e, AuthError::Msa { code, .. } if code == "invalid_grant")
}

/// Progress stages reported by [`full_login_with_device_code`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStage {
    /// Device code received; show `verification_uri` + `user_code` to the user.
    DeviceCodeReceived(DeviceCodeResponse),
    /// Waiting for the user to approve on the Microsoft page.
    WaitingForUser,
    /// MSA token acquired.
    MsaDone,
    /// Xbox Live token acquired.
    XboxLiveDone,
    /// XSTS token acquired.
    XstsDone,
    /// Minecraft Services token acquired.
    MinecraftDone,
    /// Profile fetched, session ready.
    Finished,
}

// Backwards-compatible placeholder surface (pre-flow stub).
// Keep so older `ui` revisions referencing it still compile.

/// Placeholder session (legacy stub; prefer [`MinecraftSession`]).
#[derive(Debug, Clone, Default)]
pub struct AuthSession {
    /// Placeholder username.
    pub username: String,
}

/// Alias of [`MinecraftSession`] for callers expecting `Session`.
pub type Session = MinecraftSession;

/// Placeholder login (legacy stub).
pub fn placeholder_session(username: &str) -> AuthSession {
    AuthSession {
        username: username.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// All authentication failures.
#[derive(Debug, Error)]
pub enum AuthError {
    /// No usable Azure client id configured.
    #[error("aucun client-ID Azure configuré: injectez PRISM_MSA_CLIENT_ID au build ou via le fichier de réglages (client_id)")]
    MissingClientId,

    /// HTTP transport failure.
    #[error("erreur réseau: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON failure.
    #[error("réponse JSON invalide: {0}")]
    Json(#[from] serde_json::Error),

    /// MSA token endpoint returned an unexpected error.
    #[error("erreur Microsoft ({code}): {description}")]
    Msa {
        /// `error` field of the token response.
        code: String,
        /// `error_description` field.
        description: String,
    },

    /// User declined the device-code approval.
    #[error(
        "connexion refusée sur la page Microsoft (authorization declined). Relancez la connexion."
    )]
    DeviceDeclined,

    /// Device code expired before approval.
    #[error("le code d'appareil a expiré avant validation. Relancez la connexion.")]
    DeviceExpired,

    /// Missing field in an Xbox response.
    #[error("réponse Xbox inattendue: {0}")]
    Xbox(&'static str),

    /// MSA account has no Xbox profile (XErr 2148916233).
    #[error("ce compte Microsoft n'a pas de compte Xbox. Créez-en un sur https://www.xbox.com puis réessayez. (XErr 2148916233)")]
    NoXboxAccount,

    /// Child account outside a family (XErr 2148916238).
    #[error("compte enfant : un adulte doit l'ajouter à une famille Xbox sur https://account.microsoft.com/family puis réessayez. (XErr 2148916238)")]
    ChildAccount,

    /// Account banned/suspended by Xbox (XErr 2148916229).
    #[error("ce compte Xbox est banni ou suspendu. Voyez https://account.xbox.com et contactez le support Xbox. (XErr 2148916229)")]
    AccountBanned,

    /// Other XSTS error.
    #[error("erreur XSTS (XErr {code}): {message}")]
    Xsts {
        /// Numeric XErr code.
        code: u64,
        /// Raw message from the endpoint.
        message: String,
    },

    /// `login_with_xbox` 403: the Azure registration is not allow-listed.
    #[error("inscription d'application invalide (403 Invalid app registration) : ce client-ID Azure n'est pas autorisé pour Minecraft. Utilisez votre propre client-ID Azure ou celui par défaut de Prism Launcher, injecté via PRISM_MSA_CLIENT_ID ou le fichier de réglages (client_id).")]
    InvalidAppRegistration,

    /// Minecraft request failed with status + body excerpt.
    #[error("erreur Minecraft ({status}): {body}")]
    Minecraft {
        /// HTTP status code.
        status: u16,
        /// Body excerpt.
        body: String,
    },

    /// Account owns no Minecraft Java copy.
    #[error("ce compte ne possède pas Minecraft Java (aucun droit dans mcstore). Achetez le jeu ou utilisez le compte propriétaire.")]
    GameNotOwned,

    /// Keyring failure.
    #[error("stockage sécurisé (keyring): {0}")]
    Keyring(#[from] keyring::Error),

    /// Settings/config IO failure.
    #[error("config illisible: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-testable, no network)
// ---------------------------------------------------------------------------

/// Parse a device-code JSON payload (used by [`request_device_code`]).
pub fn parse_device_code_json(s: &str) -> Result<DeviceCodeResponse, AuthError> {
    Ok(serde_json::from_str(s)?)
}

/// Parse an MSA token JSON payload.
pub fn parse_msa_token_json(s: &str) -> Result<MsaToken, AuthError> {
    Ok(serde_json::from_str(s)?)
}

/// Raw Xbox (XBL/XSTS) success shape: `{ Token, DisplayClaims: { xui: [{ uhs }] } }`.
#[derive(Debug, Deserialize)]
struct XboxRaw {
    #[serde(rename = "Token")]
    token: String,
    #[serde(rename = "DisplayClaims")]
    display_claims: DisplayClaimsRaw,
}

/// Raw display claims.
#[derive(Debug, Deserialize)]
struct DisplayClaimsRaw {
    #[serde(default)]
    xui: Vec<XuiRaw>,
}

/// Raw single XUI claim.
#[derive(Debug, Deserialize)]
struct XuiRaw {
    #[serde(default)]
    uhs: String,
}

/// Parse an XBL/XSTS success JSON payload into [`XboxAuth`].
pub fn parse_xbox_auth_json(s: &str) -> Result<XboxAuth, AuthError> {
    let raw: XboxRaw = serde_json::from_str(s)?;
    if raw.token.is_empty() {
        return Err(AuthError::Xbox("champ Token vide"));
    }
    let uhs = raw
        .display_claims
        .xui
        .into_iter()
        .next()
        .map(|x| x.uhs)
        .unwrap_or_default();
    if uhs.is_empty() {
        return Err(AuthError::Xbox("DisplayClaims.xui[0].uhs manquant"));
    }
    Ok(XboxAuth {
        token: raw.token,
        uhs,
    })
}

/// XSTS error envelope (`{ XErr, Message, ... }`).
#[derive(Debug, Deserialize)]
struct XstsErrorEnvelope {
    /// Numeric error code (capital `XErr` in the wire format).
    #[serde(rename = "XErr", default)]
    xerr: Option<u64>,
    /// Human message (capital `Message` on the wire).
    #[serde(rename = "Message", default)]
    message: Option<String>,
    #[serde(rename = "message", default)]
    message_lower: Option<String>,
}

/// Map a numeric XSTS `XErr` code to a friendly (French) [`AuthError`].
pub fn map_xsts_error(xerr: u64) -> AuthError {
    match xerr {
        2148916233 => AuthError::NoXboxAccount,
        2148916238 => AuthError::ChildAccount,
        2148916229 => AuthError::AccountBanned,
        other => AuthError::Xsts {
            code: other,
            message: format!("XErr {other}"),
        },
    }
}

/// Parse an XSTS error JSON payload into the mapped [`AuthError`].
pub fn parse_xsts_error_json(s: &str) -> AuthError {
    match serde_json::from_str::<XstsErrorEnvelope>(s) {
        Ok(env) => match env.xerr {
            Some(code) => {
                let mapped = map_xsts_error(code);
                match mapped {
                    AuthError::Xsts { .. } => AuthError::Xsts {
                        code,
                        message: env
                            .message
                            .or(env.message_lower)
                            .unwrap_or_else(|| format!("XErr {code}")),
                    },
                    other => other,
                }
            }
            None => AuthError::Xsts {
                code: 0,
                message: s.to_string(),
            },
        },
        Err(_) => AuthError::Xsts {
            code: 0,
            message: s.to_string(),
        },
    }
}

/// Build the `identityToken` for `login_with_xbox`: `XBL3.0 x=<uhs>;<xsts>`.
pub fn xbox_identity_token(uhs: &str, xsts_token: &str) -> String {
    format!("XBL3.0 x={uhs};{xsts_token}")
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// Build a default HTTP client for the auth flow.
pub fn new_client() -> Client {
    Client::new()
}

// ---------------------------------------------------------------------------
// Steps 1-2: MSA device code
// ---------------------------------------------------------------------------

/// Step 1: request a device code.
///
/// `POST {MS_DEVICE_CODE_URL}` with `client_id` + `scope=XboxLive.signin
/// offline_access`.
pub async fn request_device_code(
    client: &Client,
    config: &AuthConfig,
) -> Result<DeviceCodeResponse, AuthError> {
    config.validate()?;
    let res = client
        .post(MS_DEVICE_CODE_URL)
        .form(&[
            ("client_id", config.client_id.as_str()),
            ("scope", MSA_SCOPE),
        ])
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        let (code, description) = split_oauth_error(&body);
        return Err(AuthError::Msa { code, description });
    }
    Ok(serde_json::from_str(&body)?)
}

fn split_oauth_error(body: &str) -> (String, String) {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let code = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown")
                .to_string();
            let description = v
                .get("error_description")
                .and_then(|e| e.as_str())
                .unwrap_or(body)
                .to_string();
            (code, description)
        }
        Err(_) => ("unknown".to_string(), body.to_string()),
    }
}

/// Raw MSA token-error envelope.
#[derive(Debug, Deserialize)]
struct MsaTokenError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

/// Step 2: poll the token endpoint until the user approves/denies/expires.
///
/// Handles `authorization_pending` (sleep + retry), `slow_down`
/// (back off), `authorization_declined`/`access_denied` (declined) and
/// `expired_token` (expired) using `tokio::time::sleep`.
pub async fn poll_device_token(
    client: &Client,
    config: &AuthConfig,
    device: &DeviceCodeResponse,
) -> Result<MsaToken, AuthError> {
    config.validate()?;
    let deadline = Instant::now() + Duration::from_secs(device.expires_in);
    let mut interval = Duration::from_secs(device.interval.max(1));

    loop {
        if Instant::now() >= deadline {
            return Err(AuthError::DeviceExpired);
        }
        let res = client
            .post(MS_TOKEN_URL)
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
            ])
            .send()
            .await?;
        let status = res.status();
        let body = res.text().await?;
        if status.is_success() {
            return Ok(serde_json::from_str::<MsaToken>(&body)?);
        }
        let err: MsaTokenError = serde_json::from_str(&body).unwrap_or(MsaTokenError {
            error: "unknown".to_string(),
            error_description: body.clone(),
        });
        match err.error.as_str() {
            "authorization_pending" => {
                tokio::time::sleep(interval).await;
            }
            "slow_down" => {
                interval += Duration::from_secs(5);
                tokio::time::sleep(interval).await;
            }
            "authorization_declined" | "access_denied" => {
                return Err(AuthError::DeviceDeclined);
            }
            "expired_token" => {
                return Err(AuthError::DeviceExpired);
            }
            other => {
                return Err(AuthError::Msa {
                    code: other.to_string(),
                    description: err.error_description,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Steps 3-4: Xbox Live (XBL) + XSTS
// ---------------------------------------------------------------------------

/// Step 3: authenticate to Xbox Live with the MSA access token.
///
/// `POST user.auth.xboxlive.com/user/authenticate` with
/// `{ Properties: { AuthMethod: RPS, SiteName: user.auth.xboxlive.com,
/// RpsTicket: d=<msa> }, RelyingParty: http://auth.xboxlive.com,
/// TokenType: JWT }`.
pub async fn authenticate_xbl(
    client: &Client,
    msa_access_token: &str,
) -> Result<XboxAuth, AuthError> {
    let payload = serde_json::json!({
        "Properties": {
            "AuthMethod": "RPS",
            "SiteName": "user.auth.xboxlive.com",
            "RpsTicket": format!("d={msa_access_token}"),
        },
        "RelyingParty": "http://auth.xboxlive.com",
        "TokenType": "JWT",
    });
    let res = client.post(XBL_AUTH_URL).json(&payload).send().await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        return Err(AuthError::Minecraft {
            status: status.as_u16(),
            body: truncate(&body, 500),
        });
    }
    parse_xbox_auth_json(&body)
}

/// Step 4: authorize with XSTS.
///
/// `POST xsts.auth.xboxlive.com/xsts/authorize` with
/// `{ Properties: { SandboxId: RETAIL, UserTokens: [xbl] },
/// RelyingParty: rp://api.minecraftservices.com/, TokenType: JWT }`.
///
/// `XErr` codes 2148916233/2148916238/2148916229 map to friendly French
/// errors (see [`map_xsts_error`]).
pub async fn authorize_xsts(client: &Client, xbl_token: &str) -> Result<XboxAuth, AuthError> {
    let payload = serde_json::json!({
        "Properties": {
            "SandboxId": "RETAIL",
            "UserTokens": [xbl_token],
        },
        "RelyingParty": "rp://api.minecraftservices.com/",
        "TokenType": "JWT",
    });
    let res = client.post(XSTS_AUTH_URL).json(&payload).send().await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        return Err(parse_xsts_error_json(&body));
    }
    parse_xbox_auth_json(&body)
}

// ---------------------------------------------------------------------------
// Steps 5-6: Minecraft Services
// ---------------------------------------------------------------------------

/// Step 5: `POST .../authentication/login_with_xbox` with
/// `{ identityToken: XBL3.0 x=<uhs>;<xsts> }`.
///
/// A 403 ("Invalid app registration") maps to
/// [`AuthError::InvalidAppRegistration`] with guidance to use a custom
/// client id (or the Prism default).
pub async fn login_with_xbox(
    client: &Client,
    uhs: &str,
    xsts_token: &str,
) -> Result<MinecraftToken, AuthError> {
    let payload = serde_json::json!({
        "identityToken": xbox_identity_token(uhs, xsts_token),
    });
    let res = client
        .post(MC_LOGIN_WITH_XBOX_URL)
        .json(&payload)
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await?;
    if status == StatusCode::FORBIDDEN {
        return Err(AuthError::InvalidAppRegistration);
    }
    if !status.is_success() {
        return Err(AuthError::Minecraft {
            status: status.as_u16(),
            body: truncate(&body, 500),
        });
    }
    Ok(serde_json::from_str(&body)?)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Byte slicing can split a multi-byte char: back off to the nearest
        // char boundary so this never panics on non-ASCII bodies.
        let mut end = max.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Raw entitlements envelope.
#[derive(Debug, Deserialize)]
struct EntitlementsRaw {
    #[serde(default)]
    items: Vec<serde_json::Value>,
}

/// Step 6a: check game ownership (`GET entitlements/mcstore` must be non-empty).
pub async fn check_game_ownership(client: &Client, mc_access_token: &str) -> Result<(), AuthError> {
    let res = client
        .get(MC_ENTITLEMENTS_URL)
        .bearer_auth(mc_access_token)
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        return Err(AuthError::Minecraft {
            status: status.as_u16(),
            body: truncate(&body, 500),
        });
    }
    let ent: EntitlementsRaw = serde_json::from_str(&body)?;
    if ent.items.is_empty() {
        return Err(AuthError::GameNotOwned);
    }
    Ok(())
}

/// Step 6b: fetch the Minecraft profile (`GET minecraft/profile`).
pub async fn fetch_minecraft_profile(
    client: &Client,
    mc_access_token: &str,
) -> Result<MinecraftProfile, AuthError> {
    let res = client
        .get(MC_PROFILE_URL)
        .bearer_auth(mc_access_token)
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        return Err(AuthError::Minecraft {
            status: status.as_u16(),
            body: truncate(&body, 500),
        });
    }
    Ok(serde_json::from_str(&body)?)
}

/// Step 6: ownership check + profile fetch.
pub async fn fetch_profile(
    client: &Client,
    mc_access_token: &str,
) -> Result<MinecraftProfile, AuthError> {
    check_game_ownership(client, mc_access_token).await?;
    fetch_minecraft_profile(client, mc_access_token).await
}

// ---------------------------------------------------------------------------
// Step 7: refresh flow
// ---------------------------------------------------------------------------

/// Step 7: refresh a session from an MSA `refresh_token`.
///
/// `POST .../token` with `grant_type=refresh_token`, then the full
/// XBL → XSTS → Minecraft → profile chain. Returns a fresh in-memory
/// [`MinecraftSession`] (new refresh token included).
pub async fn refresh_session(
    config: &AuthConfig,
    refresh_token: &str,
) -> Result<MinecraftSession, AuthError> {
    refresh_session_with_client(&new_client(), config, refresh_token).await
}

/// [`refresh_session`] with an injected client (tests / custom proxies).
pub async fn refresh_session_with_client(
    client: &Client,
    config: &AuthConfig,
    refresh_token: &str,
) -> Result<MinecraftSession, AuthError> {
    config.validate()?;
    let res = client
        .post(MS_TOKEN_URL)
        .form(&[
            ("client_id", config.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", MSA_SCOPE),
        ])
        .send()
        .await?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        let (code, description) = split_oauth_error(&body);
        return Err(AuthError::Msa { code, description });
    }
    let msa: MsaToken = serde_json::from_str(&body)?;
    session_from_msa_token(client, msa).await
}

async fn session_from_msa_token(
    client: &Client,
    msa: MsaToken,
) -> Result<MinecraftSession, AuthError> {
    let xbl = authenticate_xbl(client, &msa.access_token).await?;
    let xsts = authorize_xsts(client, &xbl.token).await?;
    let mc = login_with_xbox(client, &xbl.uhs, &xsts.token).await?;
    let profile = fetch_profile(client, &mc.access_token).await?;
    Ok(MinecraftSession {
        uuid: profile.id,
        name: profile.name,
        mc_token: mc.access_token,
        refresh_token: msa.refresh_token,
        obtained: std::time::Instant::now(),
    })
}

// ---------------------------------------------------------------------------
// Full device-code login
// ---------------------------------------------------------------------------

/// Full device-code login: steps 1–6 in one call.
///
/// The `on_progress` callback receives [`LoginStage`] updates; the
/// [`LoginStage::DeviceCodeReceived`] stage carries the code/URI to display
/// so the user can approve on the Microsoft page. The MSA refresh token is
/// saved to the OS keyring on success (best effort: a keyring failure does
/// not fail the login).
pub async fn full_login_with_device_code<F>(
    config: &AuthConfig,
    on_progress: F,
) -> Result<MinecraftSession, AuthError>
where
    F: Fn(LoginStage),
{
    full_login_with_client(&new_client(), config, on_progress).await
}

/// [`full_login_with_device_code`] with an injected client.
pub async fn full_login_with_client<F>(
    client: &Client,
    config: &AuthConfig,
    on_progress: F,
) -> Result<MinecraftSession, AuthError>
where
    F: Fn(LoginStage),
{
    let device = request_device_code(client, config).await?;
    on_progress(LoginStage::DeviceCodeReceived(device.clone()));
    on_progress(LoginStage::WaitingForUser);
    let msa = poll_device_token(client, config, &device).await?;
    on_progress(LoginStage::MsaDone);
    let session = session_from_msa_token(client, msa).await?;
    on_progress(LoginStage::XboxLiveDone);
    on_progress(LoginStage::XstsDone);
    on_progress(LoginStage::MinecraftDone);
    // Best-effort persistence: only the refresh token touches the disk
    // (keyring); the Minecraft access token stays in memory.
    let _ = save_refresh_token(None, &session.refresh_token);
    on_progress(LoginStage::Finished);
    Ok(session)
}

// ---------------------------------------------------------------------------
// Secure storage (OS keyring; refresh token only)
// ---------------------------------------------------------------------------

/// Account label for the keyring entry.
///
/// `profile_id` scopes multi-account setups (`msa-refresh-token:<id>`);
/// `None` (or empty) maps to `msa-refresh-token:default`.
pub fn refresh_token_account(profile_id: Option<&str>) -> String {
    let id = profile_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    format!("msa-refresh-token:{id}")
}

/// Save the MSA refresh token to the OS keyring.
///
/// Never call this with the Minecraft access token: it stays in memory only.
pub fn save_refresh_token(profile_id: Option<&str>, refresh_token: &str) -> Result<(), AuthError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &refresh_token_account(profile_id))?;
    entry.set_password(refresh_token)?;
    Ok(())
}

/// Load the MSA refresh token from the OS keyring (`None` if absent).
pub fn load_refresh_token(profile_id: Option<&str>) -> Result<Option<String>, AuthError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &refresh_token_account(profile_id))?;
    match entry.get_password() {
        Ok(pw) => Ok(Some(pw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(AuthError::Keyring(e)),
    }
}

/// Delete the MSA refresh token from the OS keyring (ok if absent).
pub fn clear_refresh_token(profile_id: Option<&str>) -> Result<(), AuthError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &refresh_token_account(profile_id))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(AuthError::Keyring(e)),
    }
}

// ---------------------------------------------------------------------------
// Tests (mocked JSON parsing, no live network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_keeps_username() {
        assert_eq!(placeholder_session("Steve").username, "Steve");
    }

    #[test]
    fn device_code_response_parses() {
        let json = r#"{
            "user_code": "AB12-CD34",
            "device_code": "DA-DEVICE-CODE",
            "verification_uri": "https://www.microsoft.com/link",
            "verification_uri_complete": "https://www.microsoft.com/link?otc=AB12-CD34",
            "expires_in": 900,
            "interval": 5,
            "message": "To sign in, use a web browser to open the page https://www.microsoft.com/link and enter the code AB12-CD34 to authenticate."
        }"#;
        let dc = parse_device_code_json(json).expect("device code parses");
        assert_eq!(dc.user_code, "AB12-CD34");
        assert_eq!(dc.device_code, "DA-DEVICE-CODE");
        assert_eq!(dc.verification_uri, "https://www.microsoft.com/link");
        assert_eq!(dc.expires_in, 900);
        assert_eq!(dc.interval, 5);
        assert!(dc.message.contains("AB12-CD34"));
        // Public struct shape required by the spec.
        let _: DeviceCodeResponse = dc;
    }

    #[test]
    fn msa_token_parses() {
        let json =
            r#"{"access_token": "MSA-ACCESS", "refresh_token": "MSA-REFRESH", "expires_in": 3600}"#;
        let t = parse_msa_token_json(json).expect("msa token parses");
        let _: MsaToken = t.clone();
        assert_eq!(t.access_token, "MSA-ACCESS");
        assert_eq!(t.refresh_token, "MSA-REFRESH");
    }

    #[test]
    fn xbl_response_parses_token_and_uhs() {
        let json = r#"{
            "Token": "XBL-JWT",
            "DisplayClaims": { "xui": [{ "uhs": "123456789" }] }
        }"#;
        let x = parse_xbox_auth_json(json).expect("xbl parses");
        let _: XboxAuth = x.clone();
        assert_eq!(x.token, "XBL-JWT");
        assert_eq!(x.uhs, "123456789");
    }

    #[test]
    fn xbl_response_missing_uhs_errors() {
        let json = r#"{"Token": "X", "DisplayClaims": {"xui": []}}"#;
        assert!(parse_xbox_auth_json(json).is_err());
    }

    #[test]
    fn xsts_xerr_codes_map_to_french_errors() {
        assert!(matches!(
            map_xsts_error(2148916233),
            AuthError::NoXboxAccount
        ));
        assert!(matches!(
            map_xsts_error(2148916238),
            AuthError::ChildAccount
        ));
        assert!(matches!(
            map_xsts_error(2148916229),
            AuthError::AccountBanned
        ));
        // Unknown codes stay generic but keep the code.
        match map_xsts_error(2148916235) {
            AuthError::Xsts { code, .. } => assert_eq!(code, 2148916235),
            other => panic!("expected generic Xsts, got {other:?}"),
        }
        // Full envelope parsing maps too.
        let e = parse_xsts_error_json(r#"{"XErr": 2148916233, "Message": "No Xbox account"}"#);
        assert!(matches!(e, AuthError::NoXboxAccount));
        let e = parse_xsts_error_json(r#"{"XErr": 2148916238, "Message": "child"}"#);
        assert!(matches!(e, AuthError::ChildAccount));
        let e = parse_xsts_error_json(r#"{"XErr": 2148916229, "Message": "banned"}"#);
        assert!(matches!(e, AuthError::AccountBanned));
    }

    #[test]
    fn identity_token_format_is_xbl30() {
        assert_eq!(
            xbox_identity_token("123456789", "XSTS-JWT"),
            "XBL3.0 x=123456789;XSTS-JWT"
        );
    }

    #[test]
    fn minecraft_profile_parses() {
        let json = r#"{"id": "0123456789abcdef0123456789abcdef", "name": "Steve"}"#;
        let p: MinecraftProfile = serde_json::from_str(json).expect("profile parses");
        let _: MinecraftProfile = MinecraftProfile {
            id: p.id.clone(),
            name: p.name.clone(),
        };
        assert_eq!(p.name, "Steve");
        // Session shape required by the spec (mc_token in memory only).
        let s = MinecraftSession {
            uuid: p.id,
            name: p.name,
            mc_token: "MC-ACCESS".to_string(),
            refresh_token: "MSA-REFRESH".to_string(),
            obtained: std::time::Instant::now(),
        };
        assert_eq!(s.mc_token, "MC-ACCESS");
    }

    #[test]
    fn invalid_grant_classification() {
        let dead = AuthError::Msa {
            code: "invalid_grant".to_string(),
            description: "x".to_string(),
        };
        assert!(is_invalid_grant(&dead));
        let net = AuthError::Msa {
            code: "temporarily_unavailable".to_string(),
            description: "x".to_string(),
        };
        assert!(!is_invalid_grant(&net));
        assert!(!is_invalid_grant(&AuthError::MissingClientId));
    }

    #[test]
    fn refresh_account_scoping() {
        assert_eq!(refresh_token_account(None), "msa-refresh-token:default");
        assert_eq!(refresh_token_account(Some("")), "msa-refresh-token:default");
        assert_eq!(
            refresh_token_account(Some("550e8400-e29b-41d4-a716-446655440000")),
            "msa-refresh-token:550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn placeholder_client_id_is_rejected() {
        let cfg = AuthConfig::with_client_id(PLACEHOLDER_CLIENT_ID);
        assert!(matches!(cfg.validate(), Err(AuthError::MissingClientId)));
        let cfg = AuthConfig::with_client_id("real-id");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn truncate_never_splits_utf8() {
        // "é" is 2 bytes: cutting at a non-boundary must not panic.
        let s = "é".repeat(400); // 800 bytes of multi-byte chars
        let out = truncate(&s, 500);
        assert!(out.len() < s.len());
        // Short strings pass through untouched.
        assert_eq!(truncate("abc", 500), "abc");
        assert_eq!(truncate("", 500), "");
    }
}
