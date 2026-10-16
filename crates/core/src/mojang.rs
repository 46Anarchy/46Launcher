// SPDX-License-Identifier: AGPL-3.0-only

//! Mojang fallback runtime (vanilla 1.7.10 + per-OS natives + assets).
//!
//! The 46Anarchy CDN ships its 304 libraries under the single
//! `libraries-windows` bucket name (+mods/configs) : ce seau est
//! cross-platform (pur Java — voir [`crate::integrity`], référence
//! FS-Patches + leçon du wipe 16:41), il n'y a pas de modèles
//! `libraries-macos/linux` séparés, pas de fichiers natives pour aucun OS,
//! et pas de jar `versions` côté CDN. Without this module the game cannot
//! launch on Linux/macOS (natives/vanilla manquants) — and on Windows the
//! vanilla client jar would still be missing. Everything here keys off the
//! immutable Mojang 1.7.10 version JSON, cached forever under
//! `<gameDir>/caches/` (frozen versions never change, so no TTL is needed).
//!
//! Fill policy (blast radius kept minimal):
//! - vanilla client jar → always (missing on every OS);
//! - Mojang native classifier jars → every OS (extraction only; tiny);
//!   Windows gets most natives from the CDN, Linux/macOS from Mojang — both
//!   sources are extracted (see below), genuinely OS-specific payloads only ;
//! - Mojang regular libraries → Linux/macOS only, skipping anything the CDN
//!   already provides (exact file name, or same artifact base ignoring the
//!   version, e.g. CDN `guava-17.0` wins over Mojang `guava-15.0` — le plan
//!   CDN étant désormais cross-platform, le skip couvre les 304 libs et le
//!   remplissage ne comble que le vrai manque ; le LWJGL patché CDN étant
//!   exclu du plan sur Linux/macOS, le vanilla Mojang gagne toujours là) ;
//! - assets index + objects → every OS (686 files, ~107 MiB, incremental);
//! - natives extraction runs on every OS (CDN `*-natives-*.jar` + Mojang
//!   classifier jars → `<gameDir>/natives/`, `META-INF/` excluded).
//!
//! ## Log4Shell (CVE-2021-44228) — read this before "upgrading" anything
//!
//! Verified live: Mojang's 1.7.10 JSON still pins
//! `log4j-api/core:2.0-beta9` with byte-identical SHA1 to the CDN copies.
//! **There is no fixed Log4j from Mojang (or the CDN) for 1.7.10.** The
//! launcher therefore mitigates itself, in two layers:
//! 1. [`harden_log4j`] strips `JndiLookup.class` from every provisioned
//!    log4j-core jar and records the patched hash in a sidecar
//!    ([`LOG4J_SIDECAR`]) that [`crate::download::needs_update`] honours, so
//!    patched jars are never "repaired" back to vulnerable;
//! 2. the launch always carries `-Dlog4j2.formatMsgNoLookups=true`
//!    (harmless on beta9, protects any newer log4j that lands on the cp).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::instance::{assets_dir, caches_dir, libraries_dir, natives_dir, versions_dir};
use crate::integrity::PlannedFile;
use crate::manifest::ManifestFile;
use crate::os::Os;

/// Piston version manifest (lists every Minecraft version).
pub const MOJANG_MANIFEST_URL: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
/// The only vanilla version this launcher provisions.
pub const MOJANG_MC_VERSION: &str = "1.7.10";
/// Cached Mojang version JSON (immutable — no TTL).
pub const MOJANG_CACHE_FILE: &str = "mojang-1.7.10.json";
/// Cached Mojang asset index (immutable per asset id — no TTL).
pub const MOJANG_ASSET_INDEX_CACHE_FILE: &str = "mojang-assets-1.7.10.json";
/// Mojang asset objects base (`<base>/<2-char-hash>/<hash>`).
pub const MOJANG_RESOURCES_BASE: &str = "https://resources.download.minecraft.net";
/// Sidecar recording patched log4j-core hashes (see module docs).
pub const LOG4J_SIDECAR: &str = ".log4j-patched.json";
/// Zip entry prefix removed by [`harden_log4j`] (class + `$` companions).
pub const JNDI_LOOKUP_PREFIX: &str = "org/apache/logging/log4j/core/lookup/JndiLookup";

// ---------------------------------------------------------------------------
// Mojang schema (subset of piston-meta, 1.7.10 shape)
// ---------------------------------------------------------------------------

/// One Mojang downloadable artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangArtifact {
    /// Maven-style path (`org/lwjgl/.../lwjgl-2.9.1.jar`). Optional on
    /// purpose: third-party profiles in the lwjgl3ify shape omit it (their
    /// artifacts carry only `sha1`/`size`/`url`) — use
    /// [`artifact_local_path`] to resolve the on-disk layout instead of
    /// reading this field directly.
    #[serde(default)]
    pub path: String,
    /// Lowercase hex SHA1.
    pub sha1: String,
    /// Size in bytes.
    pub size: u64,
    /// HTTPS download URL.
    pub url: String,
}

/// Resolve where an artifact lives under `libraries/`.
///
/// The declared `path` wins when present (piston-meta shape). Otherwise the
/// maven layout is derived from the `group:name:version[:classifier]`
/// coordinate plus the URL file name
/// (`com.google.guava:guava:17.0` + `…/guava-17.0.jar` →
/// `com/google/guava/guava/17.0/guava-17.0.jar`). Returns `None` only when
/// neither source yields a usable relative path (unparseable coordinate and
/// an empty URL file name) — callers skip such entries instead of writing
/// to the libraries root.
pub fn artifact_local_path(lib: &MojangLibrary, artifact: &MojangArtifact) -> Option<String> {
    if !artifact.path.trim().is_empty() {
        return Some(artifact.path.clone());
    }
    let url_file = artifact
        .url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    let parts: Vec<&str> = lib.name.split(':').collect();
    if parts.len() < 3 || parts[..3].iter().any(|p| p.trim().is_empty()) || url_file.is_empty() {
        return None;
    }
    // Extra coordinate segments (Forge-style `:universal` suffixes) ride in
    // the URL file name already — only group/name/version shape the folders.
    let (group, name, version) = (parts[0], parts[1], parts[2]);
    Some(format!(
        "{}/{name}/{version}/{url_file}",
        group.replace('.', "/")
    ))
}

/// `downloads.client` (vanilla game jar).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangClientDownload {
    /// Hex SHA1.
    pub sha1: String,
    /// Size in bytes.
    pub size: u64,
    /// HTTPS download URL.
    pub url: String,
}

/// One allow/disallow rule on a Mojang library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangRule {
    /// `"allow"` or `"disallow"`.
    #[serde(default)]
    pub action: String,
    /// Optional OS selector (`{"name": "windows"|"osx"|"linux", ...}`).
    #[serde(default)]
    pub os: Option<HashMap<String, String>>,
    /// Optional launcher-feature selector (`{"is_demo_user": true, ...}`).
    /// This launcher never enables demo/custom-resolution features, so
    /// feature-gated entries are always skipped (see `modern_lib_allowed`).
    #[serde(default)]
    pub features: Option<HashMap<String, serde_json::Value>>,
}

/// One Mojang library entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangLibrary {
    /// Maven coordinate (`group:name:version`, sometimes with classifier).
    #[serde(default)]
    pub name: String,
    /// Regular jar and/or per-OS native classifier jars.
    #[serde(default)]
    pub downloads: MojangLibraryDownloads,
    /// Empty/absent = always allowed.
    #[serde(default)]
    pub rules: Option<Vec<MojangRule>>,
    /// Natives extraction instructions (`{"exclude": ["META-INF/"]}`).
    #[serde(default)]
    pub extract: Option<HashMap<String, Vec<String>>>,
}

/// Library payloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MojangLibraryDownloads {
    /// Regular (cross-platform) jar, when present.
    #[serde(default)]
    pub artifact: Option<MojangArtifact>,
    /// Native classifier jars by key (`natives-linux`, …), when present.
    #[serde(default)]
    pub classifiers: HashMap<String, MojangArtifact>,
}

/// Mojang version JSON (1.7.10 shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangVersion {
    /// Version id (`"1.7.10"`).
    #[serde(default)]
    pub id: String,
    /// Vanilla libraries (33 for 1.7.10).
    #[serde(default)]
    pub libraries: Vec<MojangLibrary>,
    /// Vanilla client jar.
    pub downloads: MojangVersionDownloads,
    /// Asset index reference.
    #[serde(rename = "assetIndex")]
    pub asset_index: MojangAssetIndexRef,
}

/// Version-level downloads wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangVersionDownloads {
    /// Vanilla game jar.
    pub client: MojangClientDownload,
}

/// Asset index reference (`id`, `sha1`, `size`, `url`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangAssetIndexRef {
    /// Asset id (`"1.7.10"`).
    #[serde(default)]
    pub id: String,
    /// Index JSON SHA1.
    #[serde(default)]
    pub sha1: String,
    /// Index JSON size.
    #[serde(default)]
    pub size: u64,
    /// Index JSON URL.
    #[serde(default)]
    pub url: String,
}

/// One asset object entry (`{hash, size}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MojangAssetObject {
    /// Content SHA1 (also the file name).
    #[serde(default)]
    pub hash: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
}

/// Mojang asset index (`{objects: {logical: {hash, size}}}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MojangAssetIndex {
    /// Objects by logical path.
    #[serde(default)]
    pub objects: HashMap<String, MojangAssetObject>,
}

// ---------------------------------------------------------------------------
// OS mapping + rules
// ---------------------------------------------------------------------------

/// Mojang OS name for our [`Os`] (`osx`, not `macos`).
pub fn mojang_os_name(current: Os) -> &'static str {
    match current {
        Os::Windows => "windows",
        Os::Macos => "osx",
        Os::Linux => "linux",
    }
}

/// Native classifier keys to try, in order (64-bit first on Windows).
pub fn native_classifier_keys(current: Os) -> &'static [&'static str] {
    match current {
        Os::Windows => &["natives-windows-64", "natives-windows"],
        Os::Macos => &["natives-osx", "natives-macos"],
        Os::Linux => &["natives-linux"],
    }
}

/// Vanilla-launcher rule semantics: no rules → allowed; otherwise the last
/// matching rule wins (a rule matches with no `os` selector or when the
/// selector names the current OS).
pub fn mojang_lib_allowed(lib: &MojangLibrary, current: Os) -> bool {
    let Some(rules) = lib.rules.as_ref() else {
        return true;
    };
    let mut allowed = false;
    for rule in rules {
        let matches = match rule.os.as_ref().and_then(|os| os.get("name")) {
            None => true,
            Some(name) => name == mojang_os_name(current),
        };
        if matches {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

/// Pick the native classifier artifact for `current` OS, if any.
pub fn select_native_classifier(lib: &MojangLibrary, current: Os) -> Option<&MojangArtifact> {
    native_classifier_keys(current)
        .iter()
        .filter_map(|key| lib.downloads.classifiers.get(*key))
        .next()
}

/// Artifact base for cross-source dedupe: jar stem minus a trailing
/// `-<version>` segment (`guava-17.0` → `guava`, `codecjorbis-20101023` →
/// `codecjorbis`, `scala-library_2.11-1.0.1` → `scala-library_2.11`).
pub fn artifact_base(jar_name: &str) -> String {
    let stem = jar_name.strip_suffix(".jar").unwrap_or(jar_name);
    match stem.rfind('-') {
        Some(i) if stem[i + 1..].starts_with(|c: char| c.is_ascii_digit()) => stem[..i].to_string(),
        _ => stem.to_string(),
    }
}

/// Maven coordinate base (`group:name:version` → `name`).
pub fn coordinate_base(coordinate: &str) -> &str {
    let mut parts = coordinate.split(':');
    parts.next();
    parts.next().unwrap_or("")
}

// ---------------------------------------------------------------------------
// Fetch + cache (immutable payloads — cache forever)
// ---------------------------------------------------------------------------

/// Fetch the Mojang version JSON for [`MOJANG_MC_VERSION`].
pub async fn fetch_mojang_version(client: &reqwest::Client) -> Result<MojangVersion> {
    let manifest: serde_json::Value = client
        .get(MOJANG_MANIFEST_URL)
        .send()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Mojang manifest : {e}")))?
        .error_for_status()
        .map_err(|e| CoreError::CdnFailure(format!("Mojang manifest : {e}")))?
        .json()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Mojang manifest : {e}")))?;
    let entry = manifest
        .get("versions")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .find(|v| v.get("id").and_then(|id| id.as_str()) == Some(MOJANG_MC_VERSION))
        .ok_or_else(|| CoreError::CdnFailure("Mojang : version 1.7.10 introuvable.".to_string()))?;
    let url = entry
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or_else(|| CoreError::CdnFailure("Mojang : URL 1.7.10 manquante.".to_string()))?;
    client
        .get(url)
        .send()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Mojang 1.7.10 : {e}")))?
        .error_for_status()
        .map_err(|e| CoreError::CdnFailure(format!("Mojang 1.7.10 : {e}")))?
        .json()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Mojang 1.7.10 : {e}")))
}

/// Cached path for the Mojang version JSON.
pub fn mojang_cache_path(game_dir: &Path) -> PathBuf {
    caches_dir(game_dir).join(MOJANG_CACHE_FILE)
}

/// Load the cached Mojang version JSON, or fetch + cache it (fetch failure
/// with a corrupt cache surfaces the fetch error).
pub async fn load_or_fetch_mojang_version(
    client: &reqwest::Client,
    game_dir: &Path,
) -> Result<MojangVersion> {
    let path = mojang_cache_path(game_dir);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(cached) = serde_json::from_slice::<MojangVersion>(&bytes) {
            return Ok(cached);
        }
    }
    let fresh = fetch_mojang_version(client).await?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&fresh) {
        let _ = std::fs::write(&path, text);
    }
    Ok(fresh)
}

/// Read the cached Mojang version JSON without network (verify path).
pub fn load_cached_mojang_version(game_dir: &Path) -> Option<MojangVersion> {
    std::fs::read(mojang_cache_path(game_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Fetch the Mojang asset index (cached next to the version JSON).
pub async fn load_or_fetch_asset_index(
    client: &reqwest::Client,
    game_dir: &Path,
    version: &MojangVersion,
) -> Result<MojangAssetIndex> {
    let path = caches_dir(game_dir).join(MOJANG_ASSET_INDEX_CACHE_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(cached) = serde_json::from_slice::<MojangAssetIndex>(&bytes) {
            return Ok(cached);
        }
    }
    let index: MojangAssetIndex = client
        .get(&version.asset_index.url)
        .send()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Mojang assets : {e}")))?
        .error_for_status()
        .map_err(|e| CoreError::CdnFailure(format!("Mojang assets : {e}")))?
        .json()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Mojang assets : {e}")))?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&index) {
        let _ = std::fs::write(&path, text);
    }
    Ok(index)
}

/// Read the cached asset index without network (verify path).
pub fn load_cached_asset_index(game_dir: &Path) -> Option<MojangAssetIndex> {
    std::fs::read(caches_dir(game_dir).join(MOJANG_ASSET_INDEX_CACHE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

// ---------------------------------------------------------------------------
// Planning (all outputs are [`PlannedFile`] → shared download pipeline)
// ---------------------------------------------------------------------------

fn mojang_file(name: String, url: String, size: u64, sha1: String, path: String) -> ManifestFile {
    ManifestFile {
        name,
        model: "mojang".to_string(),
        url,
        size,
        sha1,
        path,
        os: Vec::new(),
    }
}

/// Vanilla client jar → `versions/1.7.10/1.7.10.jar` (Mojang layout).
pub fn plan_vanilla_jar(version: &MojangVersion, game_dir: &Path) -> PlannedFile {
    let id = if version.id.is_empty() {
        MOJANG_MC_VERSION.to_string()
    } else {
        version.id.clone()
    };
    let name = format!("{id}.jar");
    PlannedFile {
        dest: versions_dir(game_dir).join(&id).join(&name),
        file: mojang_file(
            name,
            version.downloads.client.url.clone(),
            version.downloads.client.size,
            version.downloads.client.sha1.clone(),
            format!("{id}/{id}.jar"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Forge universel officiel 10.13.4.1614 (vanilla — remplace PalaForge)
// ---------------------------------------------------------------------------

/// Version Forge officielle provisionnée (les deux saveurs : le classique
/// comme l'expérimental ciblent vanilla Forge 1.7.10 — Beddium/lwjgl3ify
/// exigent l'universel officiel).
pub const FORGE_VERSION: &str = "1.7.10-10.13.4.1614-1.7.10";
/// Nom du jar universel officiel.
pub const FORGE_UNIVERSAL_FILE: &str = "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar";
/// Chemin Maven standard sous `libraries/` (référence Prism : même layout).
pub const FORGE_UNIVERSAL_PATH: &str =
    "net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10-universal.jar";
/// Taille épinglée de l'universel 1614 (artefact immuable de 2015).
pub const FORGE_UNIVERSAL_SIZE: u64 = 3_018_833;
/// SHA1 épinglé de l'universel 1614 (recoupe le profil lwjgl3ify amont).
pub const FORGE_UNIVERSAL_SHA1: &str = "25fd97f72beca728112256938e03e8105b1b78cc";
/// Forge Maven, ordre d'essai : primaire puis repli (mêmes octets).
pub const FORGE_MAVEN_PRIMARY: &str = "https://maven.minecraftforge.net";
/// Repli historique (redirige vers les releases Forge).
pub const FORGE_MAVEN_FALLBACK: &str = "https://files.minecraftforge.net/maven";

/// URL Forge Maven complète pour un artefact relatif.
fn forge_maven_url(base: &str) -> String {
    format!("{base}/net/minecraftforge/forge/{FORGE_VERSION}/{FORGE_UNIVERSAL_FILE}")
}

/// Destination sur disque de l'universel officiel (layout Maven standard).
pub fn forge_universal_dest(game_dir: &Path) -> PathBuf {
    libraries_dir(game_dir).join(FORGE_UNIVERSAL_PATH)
}

/// Plan pinned de l'universel officiel (URL primaire ; voir
/// [`forge_universal_attempts`] pour le repli).
pub fn plan_forge_universal(game_dir: &Path) -> PlannedFile {
    PlannedFile {
        dest: forge_universal_dest(game_dir),
        file: mojang_file(
            FORGE_UNIVERSAL_FILE.to_string(),
            forge_maven_url(FORGE_MAVEN_PRIMARY),
            FORGE_UNIVERSAL_SIZE,
            FORGE_UNIVERSAL_SHA1.to_string(),
            FORGE_UNIVERSAL_PATH.to_string(),
        ),
    }
}

/// Tentatives de téléchargement dans l'ordre : primaire puis repli (même
/// destination, mêmes taille/SHA1 épinglés — seule l'URL change).
pub fn forge_universal_attempts(game_dir: &Path) -> Vec<PlannedFile> {
    [FORGE_MAVEN_PRIMARY, FORGE_MAVEN_FALLBACK]
        .into_iter()
        .map(|base| PlannedFile {
            dest: forge_universal_dest(game_dir),
            file: mojang_file(
                FORGE_UNIVERSAL_FILE.to_string(),
                forge_maven_url(base),
                FORGE_UNIVERSAL_SIZE,
                FORGE_UNIVERSAL_SHA1.to_string(),
                FORGE_UNIVERSAL_PATH.to_string(),
            ),
        })
        .collect()
}

/// `true` quand le jar téléchargé est un zip non vide (garde-fou après
/// téléchargement : la fraîcheur SHA1 couvre déjà l'intégrité).
pub fn forge_zip_ok(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    match zip::ZipArchive::new(file) {
        Ok(archive) => !archive.is_empty(),
        Err(_) => false,
    }
}

/// Entrées classpath issues du remplissage Mojang (Linux/macOS uniquement) :
/// les jars réguliers que le plan CDN cross-platform ne couvre pas — en
/// pratique le LWJGL vanilla, dont le CDN ne sert qu'une copie patchée,
/// exclue du plan sur ces OS (voir
/// [`crate::integrity::is_cdn_patched_lwjgl`]).
///
/// Lecture seule du cache (`load_cached_mojang_version`, aucun réseau) avec
/// exactement les mêmes règles d'exclusion que l'installation, donc le
/// classpath ne peut pas diverger du provisionnement. Les jars de natifs
/// (classifiers) n'en font jamais partie (extraits, pas chargés).
pub fn mojang_classpath_entries(
    manifest: &crate::manifest::Manifest,
    game_dir: &Path,
    current: Os,
) -> Vec<String> {
    use std::collections::HashSet;

    let Some(version) = load_cached_mojang_version(game_dir) else {
        return Vec::new();
    };
    let cdn_planned = crate::integrity::plan_downloads(manifest, game_dir, current);
    let mut skip_names: HashSet<String> = HashSet::with_capacity(cdn_planned.len());
    skip_names.extend(cdn_planned.iter().map(|p| p.file.name.clone()));
    let mut skip_bases: HashSet<String> = HashSet::with_capacity(cdn_planned.len());
    skip_bases.extend(cdn_planned.iter().map(|p| artifact_base(&p.file.name)));
    let (regular, native_jars) = plan_mojang_libraries(
        &version,
        game_dir,
        current,
        &skip_names,
        &skip_bases,
        current != Os::Windows,
    );
    let natives: HashSet<PathBuf> = native_jars.into_iter().collect();
    regular
        .into_iter()
        .filter(|p| !natives.contains(&p.dest))
        .map(|p| p.dest.to_string_lossy().into_owned())
        .collect()
}

/// Mojang libraries for `current` OS.
///
/// Native classifier jars are always planned (all OSes; tiny, needed for
/// extraction), except when `skip_names` already has the file (CDN covers
/// it — e.g. Windows natives). Regular jars are planned only when
/// `fill_regular` (Linux/macOS : bouche-trous pour ce que le plan CDN
/// cross-platform ne couvre pas — le seau `libraries-*` couvre les 304
/// libs, seul le vrai manque reste, ex. le LWJGL vanilla dont la copie CDN
/// est patchée et exclue) and neither the exact file name nor the artifact
/// base is covered by `skip_names`/`skip_bases` (avoids e.g. Mojang
/// `guava-15.0` shadowing CDN `guava-17.0` on the classpath).
///
/// Returns regular libraries plus native jar destinations; the latter double
/// as the extraction input (see [`extract_all_natives`]).
#[allow(clippy::too_many_arguments)]
pub fn plan_mojang_libraries(
    version: &MojangVersion,
    game_dir: &Path,
    current: Os,
    skip_names: &HashSet<String>,
    skip_bases: &HashSet<String>,
    fill_regular: bool,
) -> (Vec<PlannedFile>, Vec<PathBuf>) {
    let mut regular = Vec::new();
    let mut native_jars = Vec::new();
    for lib in &version.libraries {
        if !mojang_lib_allowed(lib, current) {
            continue;
        }
        if let Some(classifier) = select_native_classifier(lib, current) {
            let file_name = classifier
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&classifier.path)
                .to_string();
            if !skip_names.contains(&file_name) {
                let dest = libraries_dir(game_dir).join(&classifier.path);
                native_jars.push(dest.clone());
                regular.push(PlannedFile {
                    dest,
                    file: mojang_file(
                        file_name,
                        classifier.url.clone(),
                        classifier.size,
                        classifier.sha1.clone(),
                        classifier.path.clone(),
                    ),
                });
            }
        }
        if !fill_regular {
            continue;
        }
        if let Some(artifact) = lib.downloads.artifact.as_ref() {
            let file_name = artifact
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&artifact.path)
                .to_string();
            if skip_names.contains(&file_name) {
                continue;
            }
            if skip_bases.contains(&artifact_base(&file_name))
                || skip_bases.contains(coordinate_base(&lib.name))
            {
                continue;
            }
            regular.push(PlannedFile {
                dest: libraries_dir(game_dir).join(&artifact.path),
                file: mojang_file(
                    file_name,
                    artifact.url.clone(),
                    artifact.size,
                    artifact.sha1.clone(),
                    artifact.path.clone(),
                ),
            });
        }
    }
    (regular, native_jars)
}

/// Mojang asset objects → `assets/objects/<hh>/<hash>` (vanilla layout).
pub fn plan_mojang_assets(index: &MojangAssetIndex, game_dir: &Path) -> Vec<PlannedFile> {
    let mut out = Vec::with_capacity(index.objects.len());
    for object in index.objects.values() {
        if object.hash.len() < 2 {
            continue;
        }
        let name = object.hash.clone();
        out.push(PlannedFile {
            dest: assets_dir(game_dir)
                .join("objects")
                .join(&object.hash[..2])
                .join(&object.hash),
            file: mojang_file(
                name.clone(),
                format!(
                    "{MOJANG_RESOURCES_BASE}/{}/{}",
                    &object.hash[..2],
                    object.hash
                ),
                object.size,
                object.hash.clone(),
                format!("objects/{}/{}", &object.hash[..2], object.hash),
            ),
        });
    }
    out.sort_by(|a, b| a.dest.cmp(&b.dest));
    out
}

// ---------------------------------------------------------------------------
// Natives extraction
// ---------------------------------------------------------------------------

/// Extract one natives jar into `natives_dir` (flattened, `META-INF/`
/// excluded, traversal-safe). Returns the extracted file count.
pub fn extract_natives(jar_path: &Path, natives_dir: &Path) -> Result<usize> {
    let file = std::fs::File::open(jar_path).map_err(CoreError::Io)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| CoreError::CdnFailure(format!("natives {} : {e}", jar_path.display())))?;
    std::fs::create_dir_all(natives_dir).map_err(CoreError::Io)?;
    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| CoreError::CdnFailure(format!("natives {} : {e}", jar_path.display())))?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let rel = name.to_string_lossy();
        if rel.starts_with("META-INF/") {
            continue;
        }
        // Flatten: Mojang natives jars keep payloads at the root; any
        // nested layout collapses to file names (collisions overwrite).
        let Some(base) = name.file_name() else {
            continue;
        };
        let dest = natives_dir.join(base);
        if !dest.starts_with(natives_dir) {
            continue;
        }
        let mut out = std::fs::File::create(&dest).map_err(CoreError::Io)?;
        std::io::copy(&mut entry, &mut out).map_err(CoreError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode));
            } else if dest.extension().is_some_and(|e| e == "so" || e == "dylib") {
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
            }
        }
        count += 1;
    }
    Ok(count)
}

/// Extract every natives jar: `*-natives-*.jar` found under `libraries/`
/// (CDN Windows jars included — same bytes, idempotent) plus `extra_jars`
/// (fresh Mojang classifier downloads). Returns the extracted file count.
///
/// Le LWJGL servi est le vanilla Mojang (`liblwjgl*`) : le `Sys` vanilla
/// charge `lwjgl`, aucun alias n'est requis (l'épisode `lwjgl_pala` est
/// parti avec PalaForge — les fichiers alias éventuellement présents sur
/// disque restent, inertes).
pub fn extract_all_natives(game_dir: &Path, extra_jars: &[PathBuf]) -> Result<usize> {
    let natives = natives_dir(game_dir);
    let mut jars: Vec<PathBuf> = extra_jars.to_vec();
    jars.extend(find_natives_jars(&libraries_dir(game_dir)));
    jars.sort();
    jars.dedup();
    let mut total = 0usize;
    for jar in &jars {
        // A jar may be mid-download from a previous aborted run — skip what
        // does not parse instead of failing the whole install.
        match extract_natives(jar, &natives) {
            Ok(n) => total += n,
            Err(_) => continue,
        }
    }
    Ok(total)
}

/// Recursively find `*-natives-*.jar` files (case-insensitive).
fn find_natives_jars(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                let lower = n.to_lowercase();
                lower.contains("natives") && lower.ends_with(".jar")
            }) {
                out.push(path);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Log4Shell hardening (JndiLookup strip + sidecar)
// ---------------------------------------------------------------------------

/// Read the patched hash for a log4j-core destination, if recorded.
///
/// Only `log4j-core*.jar` paths consult the sidecar (one ancestor walk, no
/// overhead for every other file); any IO/parse failure means "no record".
pub fn log4j_patched_sha(dest: &Path) -> Option<String> {
    let name = dest.file_name()?.to_str()?;
    if !(name.starts_with("log4j-core") && name.ends_with(".jar")) {
        return None;
    }
    let mut dir = dest.parent();
    for _ in 0..8 {
        let Some(d) = dir else { break };
        let sidecar = d.join(LOG4J_SIDECAR);
        if let Ok(bytes) = std::fs::read(&sidecar) {
            if let Ok(map) = serde_json::from_slice::<HashMap<String, String>>(&bytes) {
                if let Ok(rel) = dest.strip_prefix(d) {
                    if let Some(sha) = map.get(&rel.to_string_lossy().replace('\\', "/")) {
                        return Some(sha.clone());
                    }
                }
            }
            return None;
        }
        dir = d.parent();
    }
    None
}

/// Hardening report for the Console log.
#[derive(Debug, Clone, Default)]
pub struct Log4jReport {
    /// Jars found under `libraries/`.
    pub inspected: usize,
    /// Jars rewritten (JndiLookup removed).
    pub patched: usize,
    /// Jars already clean (recorded as-is for convergence).
    pub already_clean: usize,
}

/// Strip `JndiLookup.class` from every `log4j-core*.jar` under `libraries/`
/// (see module docs) and record the resulting hashes in the sidecar.
///
/// Idempotent: already-clean jars (including future fixed upstream builds)
/// are only recorded, never rewritten. Returns per-jar counts.
pub fn harden_log4j(game_dir: &Path) -> Result<Log4jReport> {
    let libs = libraries_dir(game_dir);
    let mut report = Log4jReport::default();
    let mut map = read_log4j_sidecar(&libs);
    let mut changed = false;
    for jar in find_log4j_core_jars(&libs) {
        report.inspected += 1;
        let rel = jar
            .strip_prefix(&libs)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| jar.to_string_lossy().into_owned());
        match strip_jndi_lookup(&jar)? {
            Some(patched_sha) => {
                report.patched += 1;
                map.insert(rel, patched_sha);
                changed = true;
            }
            None => {
                use std::collections::hash_map::Entry;
                if let Entry::Vacant(slot) = map.entry(rel) {
                    let bytes = std::fs::read(&jar).map_err(CoreError::Io)?;
                    slot.insert(crate::hash::sha1_hex(&bytes));
                    changed = true;
                }
                report.already_clean += 1;
            }
        }
    }
    if changed {
        write_log4j_sidecar(&libs, &map)?;
    }
    Ok(report)
}

fn read_log4j_sidecar(libs: &Path) -> HashMap<String, String> {
    std::fs::read(libs.join(LOG4J_SIDECAR))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_log4j_sidecar(libs: &Path, map: &HashMap<String, String>) -> Result<()> {
    let _ = std::fs::create_dir_all(libs);
    let text = serde_json::to_string_pretty(map).map_err(CoreError::Json)?;
    let tmp = libs.join(format!("{LOG4J_SIDECAR}.tmp"));
    std::fs::write(&tmp, text).map_err(CoreError::Io)?;
    std::fs::rename(&tmp, libs.join(LOG4J_SIDECAR)).map_err(CoreError::Io)?;
    Ok(())
}

fn find_log4j_core_jars(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("log4j-core") && n.ends_with(".jar"))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Remove `JndiLookup` classes from one jar (atomic `.tmp` + rename).
/// Returns `Some(new_sha1)` when rewritten, `None` when already clean.
fn strip_jndi_lookup(jar: &Path) -> Result<Option<String>> {
    let file = std::fs::File::open(jar).map_err(CoreError::Io)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| CoreError::CdnFailure(format!("{} : {e}", jar.display())))?;
    let mut targets = Vec::new();
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| CoreError::CdnFailure(format!("{} : {e}", jar.display())))?;
        let name = entry.name().to_string();
        if name.starts_with(JNDI_LOOKUP_PREFIX) && (name.ends_with(".class") || name.contains('$'))
        {
            targets.push(name);
        }
    }
    if targets.is_empty() {
        return Ok(None);
    }
    let tmp = {
        let mut os = jar.as_os_str().to_owned();
        os.push(".tmp");
        PathBuf::from(os)
    };
    {
        let out = std::fs::File::create(&tmp).map_err(CoreError::Io)?;
        let mut writer = zip::ZipWriter::new(out);
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| CoreError::CdnFailure(format!("{} : {e}", jar.display())))?;
            let name = entry.name().to_string();
            if targets.iter().any(|t| t == &name) {
                continue;
            }
            let method = entry.compression();
            let mut options = zip::write::SimpleFileOptions::default().compression_method(method);
            if let Some(mode) = entry.unix_mode() {
                options = options.unix_permissions(mode);
            }
            writer
                .start_file(name, options)
                .map_err(|e| CoreError::CdnFailure(format!("{} : {e}", jar.display())))?;
            std::io::copy(&mut entry, &mut writer).map_err(CoreError::Io)?;
        }
        writer
            .finish()
            .map_err(|e| CoreError::CdnFailure(format!("{} : {e}", jar.display())))?;
    }
    std::fs::rename(&tmp, jar).map_err(CoreError::Io)?;
    let sha = crate::hash::sha1_hex(&std::fs::read(jar).map_err(CoreError::Io)?);
    Ok(Some(sha))
}

// ---------------------------------------------------------------------------
// Freshness helpers (cheap disk checks, no network)
// ---------------------------------------------------------------------------

/// `true` when the Mojang runtime needs (re)provisioning: no cached version
/// JSON, no vanilla jar, empty natives dir, or no official Forge universal
/// on disk.
pub fn mojang_runtime_stale(game_dir: &Path) -> bool {
    if load_cached_mojang_version(game_dir).is_none() {
        return true;
    }
    let cache = load_cached_mojang_version(game_dir);
    let id = cache
        .as_ref()
        .map(|v| v.id.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(MOJANG_MC_VERSION);
    if !versions_dir(game_dir)
        .join(id)
        .join(format!("{id}.jar"))
        .is_file()
    {
        return true;
    }
    let natives = natives_dir(game_dir);
    match std::fs::read_dir(&natives) {
        Ok(mut entries) => {
            if entries.next().is_none() {
                return true;
            }
        }
        Err(_) => return true,
    }
    if !forge_jar_present(game_dir) {
        return true;
    }
    false
}

/// `true` when the official Forge 1614 universal sits at its standard Maven
/// layout under `libraries/` (ou sous un nom identique ailleurs dans
/// `libraries/` pour les placements manuels).
///
/// L'ancien `palaforge*.jar` ne compte plus : le fichier éventuellement
/// présent sur disque reste, inerte (ni téléchargé, ni au classpath).
pub fn forge_jar_present(game_dir: &Path) -> bool {
    if forge_universal_dest(game_dir).is_file() {
        return true;
    }
    let mut stack = vec![libraries_dir(game_dir)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == FORGE_UNIVERSAL_FILE)
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal Mojang-shaped fixture (no network): one plain lib, one with a
    /// Linux-disallow rule set, one classifier lib, plus client + index refs.
    fn fixture() -> MojangVersion {
        serde_json::from_value(serde_json::json!({
            "id": "1.7.10",
            "libraries": [
                {"name": "com.example:plain:1.0",
                 "downloads": {"artifact": {"path": "com/example/plain/1.0/plain-1.0.jar",
                     "sha1": "a", "size": 1, "url": "https://example.com/p.jar"}}},
                {"name": "com.example:gated:2.0",
                 "rules": [{"action": "allow"}, {"action": "disallow", "os": {"name": "linux"}}],
                 "downloads": {"artifact": {"path": "com/example/gated/2.0/gated-2.0.jar",
                     "sha1": "b", "size": 2, "url": "https://example.com/g.jar"}}},
                {"name": "com.example:nativelib:3.0",
                 "downloads": {
                     "artifact": {"path": "com/example/nativelib/3.0/nativelib-3.0.jar",
                         "sha1": "c", "size": 3, "url": "https://example.com/n.jar"},
                     "classifiers": {
                         "natives-linux": {"path": "com/example/nativelib/3.0/nativelib-3.0-natives-linux.jar",
                             "sha1": "d", "size": 4, "url": "https://example.com/nl.jar"},
                         "natives-windows": {"path": "com/example/nativelib/3.0/nativelib-3.0-natives-windows.jar",
                             "sha1": "e", "size": 5, "url": "https://example.com/nw.jar"}}},
                 "extract": {"exclude": ["META-INF/"]}}
            ],
            "downloads": {"client": {"sha1": "f", "size": 6, "url": "https://example.com/client.jar"}},
            "assetIndex": {"id": "1.7.10", "sha1": "g", "size": 7, "url": "https://example.com/assets.json"}
        }))
        .unwrap()
    }

    #[test]
    fn rules_allow_except_disallowed_os() {
        let v = fixture();
        assert!(mojang_lib_allowed(&v.libraries[0], Os::Linux));
        assert!(mojang_lib_allowed(&v.libraries[1], Os::Windows));
        assert!(!mojang_lib_allowed(&v.libraries[1], Os::Linux));
        assert!(mojang_lib_allowed(&v.libraries[2], Os::Macos));
    }

    #[test]
    fn classifiers_select_per_os() {
        let v = fixture();
        assert_eq!(
            select_native_classifier(&v.libraries[2], Os::Linux)
                .unwrap()
                .sha1,
            "d"
        );
        assert_eq!(
            select_native_classifier(&v.libraries[2], Os::Windows)
                .unwrap()
                .sha1,
            "e"
        );
        assert!(select_native_classifier(&v.libraries[0], Os::Linux).is_none());
    }

    #[test]
    fn artifact_local_path_prefers_declared_and_derives_maven_layout() {
        let with_path: MojangLibrary = serde_json::from_value(serde_json::json!({
            "name": "com.example:plain:1.0",
            "downloads": {"artifact": {"path": "com/example/plain/1.0/plain-1.0.jar",
                "sha1": "a", "size": 1, "url": "https://example.com/p.jar"}}
        }))
        .unwrap();
        let art = with_path.downloads.artifact.as_ref().unwrap();
        assert_eq!(
            artifact_local_path(&with_path, art).as_deref(),
            Some("com/example/plain/1.0/plain-1.0.jar")
        );
        // Live lwjgl3ify shape: no `path` — derived from coordinate + URL.
        let no_path: MojangLibrary = serde_json::from_value(serde_json::json!({
            "name": "com.google.guava:guava:17.0",
            "downloads": {"artifact": {"sha1": "9c6e", "size": 2,
                "url": "https://libraries.minecraft.net/com/google/guava/guava/17.0/guava-17.0.jar"}}
        }))
        .unwrap();
        let art = no_path.downloads.artifact.as_ref().unwrap();
        assert_eq!(
            artifact_local_path(&no_path, art).as_deref(),
            Some("com/google/guava/guava/17.0/guava-17.0.jar")
        );
        // Classifier coordinates keep the URL file name (Forge universal).
        let forge: MojangLibrary = serde_json::from_value(serde_json::json!({
            "name": "net.minecraftforge:forge:1.7.10-10.13.4.1614-1.7.10:universal",
            "downloads": {"artifact": {"sha1": "s", "size": 3,
                "url": "https://maven.minecraftforge.net/net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"}}
        }))
        .unwrap();
        let art = forge.downloads.artifact.as_ref().unwrap();
        assert_eq!(
            artifact_local_path(&forge, art).as_deref(),
            Some("net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10-universal.jar")
        );
        // Underivable entries (no coordinate segments, no URL file) → None.
        let bad: MojangLibrary = serde_json::from_value(serde_json::json!({
            "name": "broken",
            "downloads": {"artifact": {"sha1": "s", "size": 0, "url": ""}}
        }))
        .unwrap();
        let art = bad.downloads.artifact.as_ref().unwrap();
        assert_eq!(artifact_local_path(&bad, art), None);
    }

    #[test]
    fn artifact_base_strips_versions() {
        assert_eq!(artifact_base("guava-17.0.jar"), "guava");
        assert_eq!(artifact_base("codecjorbis-20101023.jar"), "codecjorbis");
        assert_eq!(
            artifact_base("scala-library_2.11-1.0.1.jar"),
            "scala-library_2.11"
        );
        assert_eq!(artifact_base("lwjgl_util-2.9.1.jar"), "lwjgl_util");
        assert_eq!(artifact_base("plain.jar"), "plain");
        assert_eq!(coordinate_base("com.google.guava:guava:15.0"), "guava");
    }

    #[test]
    fn library_plan_dedupes_against_cdn() {
        let v = fixture();
        let dir = tempfile::tempdir().unwrap();
        let names: HashSet<String> = ["gated-2.0.jar".to_string()].into_iter().collect();
        let bases: HashSet<String> = ["plain".to_string()].into_iter().collect();
        // Linux: gated disallowed by rules; plain skipped by base;
        // nativelib regular skipped? No — only native classifier planned
        // (regular artifact present too: skipped? name+base absent → kept).
        let (regular, natives) =
            plan_mojang_libraries(&v, dir.path(), Os::Linux, &names, &bases, true);
        let planned: Vec<_> = regular.iter().map(|p| p.file.name.as_str()).collect();
        assert!(!planned.contains(&"gated-2.0.jar"));
        assert!(!planned.contains(&"plain-1.0.jar"));
        assert!(planned.contains(&"nativelib-3.0.jar"));
        assert_eq!(natives.len(), 1);
        assert!(natives[0].ends_with("nativelib-3.0-natives-linux.jar"));
        // Windows without regular fill: only the classifier.
        let (regular, _) = plan_mojang_libraries(
            &v,
            dir.path(),
            Os::Windows,
            &HashSet::new(),
            &HashSet::new(),
            false,
        );
        assert_eq!(regular.len(), 1);
    }

    #[test]
    fn vanilla_jar_targets_versions_layout() {
        let dir = tempfile::tempdir().unwrap();
        let planned = plan_vanilla_jar(&fixture(), dir.path());
        assert!(planned.dest.ends_with("versions/1.7.10/1.7.10.jar"));
        assert_eq!(planned.file.sha1, "f");
    }

    #[test]
    fn natives_extraction_skips_meta_inf_and_traversal() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let jar_path = dir.path().join("test-natives-linux.jar");
        {
            let f = std::fs::File::create(&jar_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            w.write_all(b"manifest").unwrap();
            w.start_file("libtest.so", opts).unwrap();
            w.write_all(b"elf").unwrap();
            w.start_file("../evil.so", opts).unwrap();
            w.write_all(b"evil").unwrap();
            w.finish().unwrap();
        }
        let natives = dir.path().join("natives");
        let n = extract_natives(&jar_path, &natives).unwrap();
        assert_eq!(n, 1);
        assert!(natives.join("libtest.so").is_file());
        assert!(!natives.join("evil.so").exists());
        assert!(!dir.path().join("evil.so").exists());
    }

    #[test]
    fn extract_all_natives_serves_vanilla_lwjgl_without_aliases() {
        use std::io::Write;
        // Le LWJGL servi est le vanilla Mojang (`liblwjgl*`) : le `Sys`
        // vanilla charge `lwjgl`, aucun alias `lwjgl_pala*` n'est créé.
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let jar_dir = game.join("libraries/org/lwjgl/lwjgl/lwjgl-platform/2.9.1");
        std::fs::create_dir_all(&jar_dir).unwrap();
        let jar_path = jar_dir.join("lwjgl-platform-2.9.1-natives-linux.jar");
        {
            let f = std::fs::File::create(&jar_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            w.write_all(b"manifest").unwrap();
            w.start_file("liblwjgl.so", opts).unwrap();
            w.write_all(b"elf32").unwrap();
            w.start_file("liblwjgl64.so", opts).unwrap();
            w.write_all(b"elf64").unwrap();
            w.finish().unwrap();
        }
        let total = extract_all_natives(game, &[]).unwrap();
        let natives = natives_dir(game);
        assert!(natives.join("liblwjgl.so").is_file());
        assert!(natives.join("liblwjgl64.so").is_file());
        // Aucun alias hérité : le runtime vanilla n'en a pas besoin.
        assert!(!natives.join("liblwjgl_pala.so").exists());
        assert!(!natives.join("liblwjgl_pala64.so").exists());
        assert_eq!(
            std::fs::read(natives.join("liblwjgl.so")).unwrap(),
            b"elf32"
        );
        // Extraction seule (2 .so, META-INF ignoré).
        assert_eq!(total, 2);
        // Idempotent : un second passage ré-extrait (2) sans rien créer.
        let again = extract_all_natives(game, &[]).unwrap();
        assert_eq!(again, 2);
    }

    #[test]
    fn extract_all_natives_without_jars_extracts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let total = extract_all_natives(dir.path(), &[]).unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn mojang_runtime_stale_without_forge_universal() {
        // Sans universel officiel sur disque, le runtime reste périmé :
        // l'ancien `palaforge*.jar` ne compte plus (inerte).
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        assert!(mojang_runtime_stale(game));
        // Cache + jar vanilla + natives non vides, sans Forge.
        let caches = caches_dir(game);
        std::fs::create_dir_all(&caches).unwrap();
        std::fs::write(
            caches.join(MOJANG_CACHE_FILE),
            serde_json::json!({
                "id": "1.7.10", "libraries": [],
                "downloads": {"client": {"sha1": "f", "size": 6, "url": "https://example.com/c.jar"}},
                "assetIndex": {"id": "1.7.10", "sha1": "g", "size": 7, "url": "https://example.com/a.json"}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(versions_dir(game).join("1.7.10")).unwrap();
        std::fs::write(versions_dir(game).join("1.7.10/1.7.10.jar"), b"vanilla").unwrap();
        let natives = natives_dir(game);
        std::fs::create_dir_all(&natives).unwrap();
        std::fs::write(natives.join("liblwjgl.so"), b"elf").unwrap();
        // Un vieux palaforge ne rend pas le runtime frais.
        let libs = libraries_dir(game).join("fr/paladium");
        std::fs::create_dir_all(&libs).unwrap();
        std::fs::write(libs.join("palaforge-1.7.10-10.13.4.2512.jar"), b"x").unwrap();
        assert!(
            mojang_runtime_stale(game),
            "sans universel officiel le runtime reste périmé"
        );
        // L'universel au layout Maven guérit l'état.
        let dest = forge_universal_dest(game);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"forge").unwrap();
        assert!(!mojang_runtime_stale(game));
    }

    #[test]
    fn harden_strips_jndi_and_records_sidecar() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let libs = dir.path().join("libraries");
        std::fs::create_dir_all(libs.join("org/example")).unwrap();
        let jar = libs.join("org/example/log4j-core-2.0-test.jar");
        {
            let f = std::fs::File::create(&jar).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file(
                "org/apache/logging/log4j/core/lookup/JndiLookup.class",
                opts,
            )
            .unwrap();
            w.write_all(b"jndi").unwrap();
            w.start_file("org/example/Foo.class", opts).unwrap();
            w.write_all(b"foo").unwrap();
            w.finish().unwrap();
        }
        let report = harden_log4j(dir.path()).unwrap();
        assert_eq!((report.inspected, report.patched), (1, 1));
        // Rewritten jar: JndiLookup gone, Foo intact (read back as a zip —
        // entries are deflated, so raw-byte greps would lie).
        let reopened = std::fs::File::open(&jar).unwrap();
        let mut archive = zip::ZipArchive::new(reopened).unwrap();
        let mut names = Vec::new();
        for i in 0..archive.len() {
            names.push(archive.by_index(i).unwrap().name().to_string());
        }
        assert!(!names.iter().any(|n| n.contains("JndiLookup")), "{names:?}");
        assert!(names.contains(&"org/example/Foo.class".to_string()));
        // Sidecar recorded → freshness check passes without re-download.
        assert!(log4j_patched_sha(&jar).is_some());
        assert!(!crate::download::needs_update(&jar, 0, "deadbeef"));
    }

    #[test]
    fn mojang_runtime_stale_without_cache_or_jars() {
        let dir = tempfile::tempdir().unwrap();
        assert!(mojang_runtime_stale(dir.path()));
    }

    #[test]
    fn forge_universal_source_shape_and_layout() {
        // L'universel officiel 1614 vient de Forge Maven (primaire puis
        // repli), au layout Maven standard sous `libraries/`.
        assert_eq!(
            FORGE_UNIVERSAL_PATH,
            "net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"
        );
        let dir = tempfile::tempdir().unwrap();
        let planned = plan_forge_universal(dir.path());
        assert_eq!(planned.dest, forge_universal_dest(dir.path()));
        assert!(
            planned
                .dest
                .starts_with(libraries_dir(dir.path()).join("net/minecraftforge")),
            "layout maven sous libraries/, obtenu : {}",
            planned.dest.display()
        );
        assert_eq!(planned.file.name, FORGE_UNIVERSAL_FILE);
        assert_eq!(planned.file.size, FORGE_UNIVERSAL_SIZE);
        assert_eq!(planned.file.sha1, FORGE_UNIVERSAL_SHA1);
        let attempts = forge_universal_attempts(dir.path());
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().all(|p| p.dest == planned.dest));
        let urls: Vec<_> = attempts.iter().map(|p| p.file.url.as_str()).collect();
        assert_eq!(
            urls[0],
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"
        );
        assert_eq!(
            urls[1],
            "https://files.minecraftforge.net/maven/net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"
        );
    }

    #[test]
    fn forge_zip_ok_accepts_only_non_empty_zips() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.jar");
        {
            let f = std::fs::File::create(&good).unwrap();
            let mut w = zip::ZipWriter::new(f);
            w.start_file("a.class", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"bytes").unwrap();
            w.finish().unwrap();
        }
        assert!(forge_zip_ok(&good));
        let garbage = dir.path().join("bad.jar");
        std::fs::write(&garbage, b"pas un zip").unwrap();
        assert!(!forge_zip_ok(&garbage));
        assert!(!forge_zip_ok(&dir.path().join("absent.jar")));
    }

    #[test]
    fn forge_jar_detected_at_maven_layout_only() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!forge_jar_present(dir.path()));
        // L'ancien palaforge ne compte plus (inerte sur disque).
        let libs = dir.path().join("libraries").join("fr/paladium");
        std::fs::create_dir_all(&libs).unwrap();
        std::fs::write(libs.join("palaforge-1.7.10-10.13.4.2512.jar"), b"x").unwrap();
        assert!(!forge_jar_present(dir.path()));
        // L'universel officiel au layout Maven compte.
        let dest = forge_universal_dest(dir.path());
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"forge").unwrap();
        assert!(forge_jar_present(dir.path()));
    }

    #[test]
    fn mojang_classpath_fill_serves_vanilla_lwjgl_on_linux() {
        // Seau CDN cross-platform : le LWJGL patché (`Sys` → `lwjgl_pala`,
        // sha `80ef7a…`) reste exclu du plan sur Linux — le vanilla Mojang
        // prend le relais au classpath — pendant que le pur Java CDN (guava)
        // est désormais servi par le CDN lui-même, pas par Mojang.
        let version: MojangVersion = serde_json::from_value(serde_json::json!({
            "id": "1.7.10",
            "libraries": [
                {"name": "org.lwjgl.lwjgl:lwjgl:2.9.1",
                 "downloads": {"artifact": {"path": "org/lwjgl/lwjgl/lwjgl/2.9.1/lwjgl-2.9.1.jar",
                     "sha1": "s", "size": 1, "url": "https://example.com/lwjgl.jar"}}},
                {"name": "com.google.guava:guava:15.0",
                 "downloads": {"artifact": {"path": "com/google/guava/guava/15.0/guava-15.0.jar",
                     "sha1": "s", "size": 1, "url": "https://example.com/guava.jar"}}}
            ],
            "downloads": {"client": {"sha1": "f", "size": 6, "url": "https://example.com/c.jar"}},
            "assetIndex": {"id": "1.7.10", "sha1": "g", "size": 7, "url": "https://example.com/a.json"}
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        std::fs::create_dir_all(crate::instance::caches_dir(game)).unwrap();
        std::fs::write(
            mojang_cache_path(game),
            serde_json::to_string(&version).unwrap(),
        )
        .unwrap();
        // Manifeste CDN : lwjgl patché (exclu du plan Linux — vanilla Mojang
        // gagne) + guava pur Java (INCLUS : seau `libraries-*`
        // cross-platform, le CDN couvre guava sur Linux).
        let manifest: crate::manifest::Manifest = crate::manifest::parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [{"name": "libraries-windows", "dest": "libraries",
                        "properties": {"classpath": true}, "os": ["WINDOWS"]}],
            "files": [
                {"name": "lwjgl-2.9.1.jar", "model": "libraries-windows",
                 "url": "https://x/lwjgl.jar", "size": 1, "sha1": "a",
                 "path": "org/lwjgl/lwjgl/lwjgl/2.9.1/lwjgl-2.9.1.jar", "os": ["WINDOWS"]},
                {"name": "guava-15.0.jar", "model": "libraries-windows",
                 "url": "https://x/guava.jar", "size": 1, "sha1": "b",
                 "path": "com/google/guava/guava/15.0/guava-15.0.jar", "os": ["WINDOWS"]}
            ]
        }"#,
        )
        .unwrap();
        let entries = mojang_classpath_entries(&manifest, game, Os::Linux);
        assert!(
            entries
                .iter()
                .any(|e| e.ends_with("org/lwjgl/lwjgl/lwjgl/2.9.1/lwjgl-2.9.1.jar")),
            "le vanilla Mojang lwjgl doit gagner sur Linux : {entries:?}"
        );
        // Seau CDN cross-platform : le plan Linux couvre désormais guava
        // (pur Java `libraries-windows`) — le CDN gagne, le remplissage
        // Mojang ne sert donc PAS guava (dedupe nom/base). Seul le LWJGL
        // reste servi : sa copie CDN est patchée, exclue du plan sur Linux.
        assert!(
            !entries.iter().any(|e| e.contains("guava")),
            "guava CDN gagne sur Linux (Mojang skippé) : {entries:?}"
        );
        // Sans cache Mojang : rien (aucun réseau dans le builder).
        let bare = tempfile::tempdir().unwrap();
        assert!(mojang_classpath_entries(&manifest, bare.path(), Os::Linux).is_empty());
    }
}
