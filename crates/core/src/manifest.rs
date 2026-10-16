// SPDX-License-Identifier: AGPL-3.0-only

//! Serde types mirroring `https://cdn.46anarchy.fr/manifest.json`.
//!
//! Shape (observed live):
//! ```json
//! {
//!   "name": "Paladium V11", "root": "paladium", "version": "v1.0.0",
//!   "assetIndex": "https://...", "mc_version": "1.7.10",
//!   "maintenance": {"enabled": false, "message": ""},
//!   "main": "net.minecraft.launchwrapper.Launch",
//!   "arguments": {"game": [...], "jvm": [...]},
//!   "models": [{"name": "mods", "dest": "mods", "properties": {...}, "os": [...]}],
//!   "files": [{"name": "...", "model": "mods", "url": "https://...",
//!              "size": 123, "sha1": "...", "path": "...", "os": [...]}]
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::os::Os;

/// Top-level manifest document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Display name, e.g. `"Paladium V11"`.
    pub name: String,
    /// Instance root folder name, e.g. `"paladium"`.
    pub root: String,
    /// Manifest version, e.g. `"v1.0.0"`. Compared against `cdn_last.json`.
    pub version: String,
    /// Mojang asset-index URL.
    #[serde(rename = "assetIndex")]
    pub asset_index: String,
    /// Minecraft version, e.g. `"1.7.10"`.
    pub mc_version: String,
    /// Maintenance flag + message shown by the launcher.
    pub maintenance: Maintenance,
    /// Main class, e.g. `"net.minecraft.launchwrapper.Launch"`.
    pub main: String,
    /// Launch arguments with `${...}` placeholders.
    pub arguments: Arguments,
    /// Storage models: logical groups mapping to a `dest` folder.
    pub models: Vec<Model>,
    /// Every downloadable file, each pointing at a `model`.
    pub files: Vec<ManifestFile>,
}

/// Maintenance banner state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Maintenance {
    /// When `true` the launcher should refuse to launch and show `message`.
    pub enabled: bool,
    /// Human-readable maintenance message (may be empty).
    pub message: String,
}

/// `arguments.game` / `arguments.jvm` template lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Arguments {
    /// Game arguments (may contain `${username}`, `${gameDir}`, ...).
    #[serde(default)]
    pub game: Vec<String>,
    /// JVM arguments (may contain `${options.library.path}`, ...).
    #[serde(default)]
    pub jvm: Vec<String>,
}

/// Optional per-model flags observed on the CDN.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelProperties {
    /// Whether the model's files belong on the classpath.
    #[serde(default)]
    pub classpath: Option<bool>,
    /// Whether this model is a Java runtime payload.
    #[serde(default)]
    pub java: Option<bool>,
    /// Whether downloads require a token query/headers (kept opaque here).
    #[serde(default)]
    pub download_token: Option<bool>,
    /// Whitelist globs (used by `mods` / `resourcepacks` / `shaderpacks`).
    #[serde(default)]
    pub whitelist: Option<Vec<String>>,
}

/// A logical file group, e.g. `mods` → `dest: "mods"`,
/// `natives-windows` → `dest: "natives"` with `os: ["WINDOWS"]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    /// Model key referenced by `files[].model`.
    pub name: String,
    /// Model version string (informational).
    #[serde(default)]
    pub version: Option<String>,
    /// Human-readable description (informational).
    #[serde(default)]
    pub description: Option<String>,
    /// Destination folder relative to the game dir.
    pub dest: String,
    /// Optional flags (classpath / java / whitelist / ...).
    #[serde(default)]
    pub properties: Option<ModelProperties>,
    /// Empty = cross-platform; otherwise OS-gated.
    #[serde(default)]
    pub os: Vec<Os>,
}

/// One downloadable file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    /// File name (used for exclusion checks, e.g. the dedicated-server jar).
    pub name: String,
    /// Owning model key (`models[].name`).
    pub model: String,
    /// HTTPS download URL.
    pub url: String,
    /// Expected size in bytes.
    pub size: u64,
    /// Expected lowercase hex SHA1.
    pub sha1: String,
    /// Path relative to the model's `dest` folder.
    pub path: String,
    /// Empty = cross-platform; otherwise OS-gated.
    #[serde(default)]
    pub os: Vec<Os>,
}

impl Manifest {
    /// Look up a model by its `name` key.
    pub fn model_by_name(&self, name: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.name == name)
    }
}

/// Parse a manifest from its JSON text (CDN payload or cached `cdn_*.json`).
pub fn parse_manifest(json: &str) -> Result<Manifest> {
    Ok(serde_json::from_str(json)?)
}

/// Fetch + parse the live manifest. Callers surface [`CoreError::CdnFailure`]
/// when the CDN is unreachable so the UI can show the stored `cdn_last.json`.
pub async fn fetch_manifest(client: &reqwest::Client, url: &str) -> Result<Manifest> {
    let res = client
        .get(url)
        .send()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("GET {url}: {e}")))?;
    let status = res.status();
    if !status.is_success() {
        return Err(CoreError::CdnFailure(format!("GET {url}: HTTP {status}")));
    }
    let text = res
        .text()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("read {url}: {e}")))?;
    parse_manifest(&text)
}

/// Default manifest endpoint.
pub const MANIFEST_URL: &str = "https://cdn.46anarchy.fr/manifest.json";

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal offline fixture covering every field the launcher relies on.
    pub(crate) const MINIMAL_MANIFEST_JSON: &str =
        include_str!("../tests/fixtures/minimal_manifest.json");

    #[test]
    fn parses_minimal_manifest_sample() {
        let m = parse_manifest(MINIMAL_MANIFEST_JSON).expect("fixture must parse");
        assert_eq!(m.name, "Paladium V11");
        assert_eq!(m.root, "paladium");
        assert_eq!(m.version, "v1.0.0");
        assert_eq!(m.mc_version, "1.7.10");
        assert_eq!(m.main, "net.minecraft.launchwrapper.Launch");
        assert!(!m.arguments.game.is_empty());
        assert!(!m.arguments.jvm.is_empty());
        assert!(!m.models.is_empty());
        assert!(!m.files.is_empty());
        // Spot-check OS-gated + cross-platform models.
        assert!(m.models.iter().any(|x| x.name == "mods"));
        assert!(m.models.iter().any(|x| x.name == "natives-windows"));
        assert!(m.model_by_name("mods").is_some());
        assert!(m.model_by_name("does-not-exist").is_none());
    }
}
