// SPDX-License-Identifier: AGPL-3.0-only

//! File-integrity rules ported from FS-Patches `FileIntegrity.java`.
//!
//! ## Règle OS — référence FS-Patches, leçon du wipe 16:41
//!
//! Référence authoritative : FS-Patches `FileIntegrity.java` ne blackliste
//! que `natives-windows/macos/linux`, `versions`, `java-windows/macos/java-linux`
//! (plus `BLACKLISTED_FILES = palaforge-1.7.10-10.13.4.2512.jar`).
//! Tout le reste — dont `libraries-windows` quel que soit son suffixe —
//! se télécharge cross-platform.
//!
//! Sur CE CDN, `libraries-windows` n'est PAS un payload Windows : c'est le
//! nom du seau unique des 304 bibliothèques (pur Java, indépendant de
//! l'OS). Le boot 15:51 l'a prouvé (menu atteint avec ces jars sur Linux).
//! Le gate OS strict qui excluait chaque entrée `model=libraries-windows,
//! os=[WINDOWS]` sur Linux a donc vidé `libraries/` (16:41 : réinstall
//! qui ne pouvait plus remplir — seuls les 33 jars Mojang restaient) puis
//! tué le spawn (`Could not find or load main class
//! net.minecraft.launchwrapper.Launch`, `latest.log` intact).
//! Ne JAMAIS resserrer cette règle : `libraries-*` reste cross-platform.
//!
//! Règle effective :
//! - `libraries-*` (tout suffixe) est **cross-platform** : inclus quel que
//!   soit le champ `os` du fichier comme du modèle — moins les carve-outs
//!   explicites ci-dessous ;
//! - `natives-*` et `java-*` restent **strict-OS** : le champ `os`
//!   (vide = partagé, sinon OS courant requis) + repli suffixe
//!   `-windows`/`-macos`/`-linux` quand `os` est vide (voir
//!   [`crate::os::Os::from_model_suffix`]) ;
//! - à l'intérieur d'un modèle `libraries-*`, un fichier dont le nom
//!   contient `natives` reste scopé OS (vrai binaire natif, pas du pur
//!   Java) — même règle stricte que `natives-*`.
//!
//! Carve-outs conservés sur tous les OS (voir fonctions dédiées) :
//! 1. **serveur dédié** ([`is_dedicated_server_file`]) : `net/minecraft/server/…`
//!    recouvre 1247 classes clientes partagées (`pu`/`JsonUtils`) et tue le
//!    boot (`NoSuchMethodError func_151218_a`) ;
//! 2. **PalaForge** ([`is_palaforge_file`] + entrée [`BLACKLISTED_FILES`]) :
//!    SEUL l'artefact exact du fork
//!    (`palaforge-1.7.10-10.13.4.2512.jar` / `fr/paladium/palaforge/…`) est
//!    exclu — jamais le préfixe `palaforge*`, qui affamerait le mod
//!    `PalaForgeUtils-6.6.2.jar` du pack (`MissingModsException`
//!    `required-after:palaforge-utils` avalée par FML : zéro phase de
//!    chargement). Les deux saveurs lancent l'universel officiel 1614 depuis
//!    Forge Maven (voir [`crate::mojang`]) ; un fichier stale reste inerte
//!    sur disque ;
//! 3. **LWJGL patché du CDN** ([`is_cdn_patched_lwjgl`]) : `org/lwjgl/…`
//!    (`Sys` → `lwjgl_pala`) exclu sur Linux/macOS — le vanilla Mojang
//!    (remplissage [`crate::mojang`]) gagne ; servi tel quel sur Windows.
//!
//! ## Gate bytecode : avertissement seulement (jamais d'échec)
//!
//! FML lui-même ignore les jars illisibles (`Zip file … failed to read
//! properly, it will be ignored` — le boot 15:51 a CONTINUÉ : 69 mods,
//! init GL, menu). Bloquer l'install ou le lancement est donc strictement
//! pire que l'avertissement : le scanner ([`max_class_version_in_jar`])
//! calcule toujours, mais [`check_jar_class_versions`] ne fait qu'émettre
//! des avertissements français nommant les jars.
//!
//! ## Other rules (faithful ports)
//!
//! - [`BLACKLISTED_FILES`]: exact file names that are never downloaded —
//!   currently empty (kept as a hook).
//! - destination names are the manifest names as-is (`.jar`) : vanilla
//!   Forge découvre les `.jar`, il n'y a plus de mapping d'extension.
//!   (Historique : PalaForge n'ouvrait que les `.pala`, d'où la danse de
//!   renommage `.jar` ↔ `.pala` — supprimée avec PalaForge. Les éventuels
//!   `mods/*.pala` résiduels sont re-migrés vers `.jar`, voir
//!   [`crate::instance::migrate_mods_to_jar`].)
//! - `libraries-*` → `mods/` remap: files owned by a `libraries-*` model whose
//!   `path` already starts with `"mods/"` are written under `<gameDir>/mods/…`
//!   instead of `<gameDir>/libraries/mods/…` (avoids double nesting).
//! - duplicate manifest entries (the CDN lists every file twice) collapse to
//!   one download per destination in [`plan_downloads`].

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::error::CoreError;
use crate::manifest::{Manifest, ManifestFile, Model};
use crate::os::{is_allowed_for, Os};

/// Exact file names that must never be downloaded (matched against
/// [`ManifestFile::name`]; see module docs).
///
/// Miroir de FS-Patches `FileIntegrity.java` (`BLACKLISTED_FILES`) : le
/// Forge custom Paladium ne se télécharge jamais (les deux saveurs lancent
/// l'universel officiel 1614). La garde [`is_palaforge_file`] (nom exact du
/// fork / chemin `fr/paladium/palaforge/…`) double cette entrée ; aucune des
/// deux ne touche au préfixe `palaforge*` (voir `is_palaforge_file`).
pub const BLACKLISTED_FILES: &[&str] = &["palaforge-1.7.10-10.13.4.2512.jar"];

/// Returns `true` when `file_name` is on the never-download list.
pub fn is_file_blacklisted(file_name: &str) -> bool {
    BLACKLISTED_FILES.contains(&file_name)
}

/// `true` for the PalaForge custom Forge, exclu du plan de téléchargement
/// ET du classpath sur tous les OS (les deux saveurs lancent l'universel
/// officiel 1614 depuis Forge Maven).
///
/// Règle EXACTE uniquement : nom `palaforge-1.7.10-10.13.4.2512.jar` ou
/// chemin `fr/paladium/palaforge/…`, insensible à la casse. Le préfixe
/// `palaforge*` est INTERDIT ici : il excluait aussi le mod légitime
/// `PalaForgeUtils-6.6.2.jar` du pack, dont 8 mods exigent
/// `required-after:palaforge-utils` — FML avalait la `MissingModsException`
/// et aucune phase de chargement ne démarrait. Un fichier du fork déjà
/// présent sur disque reste, inerte — comme le jar serveur dédié.
pub fn is_palaforge_file(file: &ManifestFile) -> bool {
    let name = file.name.to_lowercase();
    if name == "palaforge-1.7.10-10.13.4.2512.jar" {
        return true;
    }
    let path = file.path.replace('\\', "/").to_lowercase();
    path.contains("fr/paladium/palaforge")
}

/// `true` for the CDN's patched LWJGL jars (`org/lwjgl/…`), whose `Sys`
/// loads `lwjgl_pala` instead of `lwjgl` (vérifié sur le
/// `lwjgl-2.9.1.jar` du CDN, sha `80ef7a0b…`).
///
/// Ces jars sont exclus de la réutilisation cross-platform sur Linux/macOS :
/// le vanilla Mojang (remplissage [`crate::mojang`]) gagne, et aucun alias
/// `liblwjgl_pala*` n'est requis. Sur Windows le payload CDN reste servi
/// tel quel (modèle `libraries-windows` natif).
pub fn is_cdn_patched_lwjgl(file: &ManifestFile) -> bool {
    let path = file.path.replace('\\', "/").to_lowercase();
    if path.starts_with("org/lwjgl/") {
        return true;
    }
    let name = file.name.to_lowercase();
    // Noms versionnés uniquement (`lwjgl-2.9.1.jar`) : le préfixe nu
    // avalait aussi `lwjgl-tweaker.jar`, un mod légitime.
    ["lwjgl-", "lwjgl_util-"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// Maven layout prefix of the Mojang dedicated-server jar
/// (`net/minecraft/server/1.7.10/server-1.7.10.jar` on the CDN).
pub const DEDICATED_SERVER_PATH_PREFIX: &str = "net/minecraft/server/";

/// `true` for the Mojang dedicated-server jar, which must never be
/// downloaded nor land on the client classpath.
///
/// Cause racine du `NoSuchMethodError
/// JsonUtils.func_151218_a(JsonObject, String, JsonObject)` au boot Forge :
/// ce jar (entrée `libraries-windows` du CDN, conservée sur Linux par la
/// réutilisation cross-platform) précède le jar vanilla (ajouté en dernier
/// par [`crate::launch::build_classpath`]) et recouvre 1247 classes
/// obfusquées partagées — dont `pu` (`JsonUtils` après déobfuscation), dont
/// la version serveur (3424 o, 10 méthodes) ne contient PAS la surcharge
/// `a(JsonObject, String, JsonObject)` (`func_151218_a`) qu'appelle `bqo`
/// (`ResourceIndex.<init>`, `SourceFile:37`) depuis
/// `Minecraft.<init>:319`. Même ombre côté patcher FML (classe
/// `MinecraftServer` reskipée entre deux lancements à jar vanilla
/// identique) et avertissements « expecting empty class data file for
/// DedicatedServer / MinecraftServerGui ».
pub fn is_dedicated_server_file(manifest: &Manifest, file: &ManifestFile) -> bool {
    let rel = file.path.replace('\\', "/");
    if rel.starts_with(DEDICATED_SERVER_PATH_PREFIX) {
        return true;
    }
    // Repli sur le nom seul (si le CDN déplace le chemin un jour) :
    // restreint aux modèles bibliothèques pour ne jamais exclure un mod
    // qui s'appellerait `server-*.jar`.
    if file.name.starts_with("server-") && file.name.ends_with(".jar") {
        return manifest
            .model_by_name(&file.model)
            .is_some_and(|m| m.dest == "libraries" || m.name.starts_with("libraries-"));
    }
    false
}

/// `true` pour le seau cross-platform des bibliothèques : tout modèle
/// `libraries-*`, quel que soit le suffixe (`-windows`, `-macos`, `-linux`,
/// …). Insensible à la casse. Voir la règle OS en tête de module (référence
/// FS-Patches + leçon du wipe 16:41) : ce seau ignore le champ `os`.
pub fn is_libraries_model(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("libraries-")
}

/// Positive OS-gate for a model: empty `os` (shared) or lists `current`.
///
/// Deux exceptions (voir la règle OS en tête de module) :
/// - `libraries-*` est toujours autorisé (seau cross-platform, champ `os`
///   ignoré — leçon du wipe 16:41) ;
/// - sinon, repli suffixe : quand `os` est vide, un nom en `-windows`/`-macos`
///   (`-osx`/`-mac` inclus) /`-linux` vaut gate OS — le manifeste utilise
///   les deux conventions en parallèle. `natives-*` et `java-*` restent donc
///   strict-OS via ce repli ([`crate::os::Os::from_model_suffix`]).
pub fn model_allowed(model: &Model, current: Os) -> bool {
    if is_libraries_model(&model.name) {
        return true;
    }
    if !model.os.is_empty() {
        return is_allowed_for(&model.os, current);
    }
    match Os::from_model_suffix(&model.name) {
        Some(want) => want == current,
        None => true,
    }
}

/// Whether a file should be downloaded on `current` OS.
///
/// Règle OS (plan, vérification, classpath — identique partout ; voir la
/// règle en tête de module, référence FS-Patches + leçon du wipe 16:41) :
/// - `libraries-*` (tout suffixe) est cross-platform : inclus quel que soit
///   le champ `os` du fichier comme du modèle — moins les carve-outs
///   (serveur dédié, PalaForge/blacklist, LWJGL patché CDN sur Linux/macOS
///   où le vanilla Mojang gagne, binaires `*natives*` qui restent scopés OS
///   comme un vrai natif) ;
/// - tout le reste (`natives-*`, `java-*`, `mods`, …) reste strict-OS : le
///   champ `os` du fichier puis celui du modèle doivent autoriser `current`
///   (vide = partagé), avec repli suffixe `-windows`/`-macos`/`-linux`
///   quand l'`os` du modèle est vide.
///
/// Checks, in order:
/// 1. PalaForge exclusion ([`is_palaforge_file`]) → skip on every OS;
/// 2. file-name blacklist ([`BLACKLISTED_FILES`]) → skip;
/// 3. dedicated-server exclusion ([`is_dedicated_server_file`]) → skip;
/// 4. `libraries-*` bucket → include (carve-outs LWJGL/natives ci-dessous),
///    en ignorant les champs `os` ;
/// 5. sinon, file-level `os` gate → skip when non-empty and not containing
///    `current`, puis owning-model `os` gate ([`model_allowed`], repli
///    suffixe inclus). Unknown models: repli suffixe sur `file.model`,
///    sinon partagé (compatibilité avant).
pub fn file_allowed(manifest: &Manifest, file: &ManifestFile, current: Os) -> bool {
    // PalaForge ne joue jamais : ni téléchargé, ni au classpath.
    if is_palaforge_file(file) {
        return false;
    }
    if is_file_blacklisted(&file.name) {
        return false;
    }
    // Le serveur dédié Mojang ne joue jamais (voir `is_dedicated_server_file`) :
    // ni téléchargé, ni au classpath — sur aucun OS.
    if is_dedicated_server_file(manifest, file) {
        return false;
    }
    // Seau `libraries-*` : cross-platform, champs `os` ignorés (FS-Patches +
    // leçon du wipe 16:41 — `libraries-windows` est le nom du seau unique,
    // pas un payload Windows). Le modèle est identifié par `file.model`,
    // confirmé par le modèle résolu quand il existe.
    let in_libraries_bucket = is_libraries_model(&file.model)
        || manifest
            .model_by_name(&file.model)
            .is_some_and(|m| is_libraries_model(&m.name));
    if in_libraries_bucket {
        // Vrai binaire natif glissé dans le seau (`*-natives-*.jar`, …) :
        // scopé OS comme `natives-*`, jamais réutilisé cross-platform.
        // Pas de repli suffixe sur le modèle : le suffixe `libraries-*`
        // est cross-platform — seuls les champs `os` comptent, et un champ
        // vide vaut partagé.
        if file.name.to_ascii_lowercase().contains("natives") {
            if !is_allowed_for(&file.os, current) {
                return false;
            }
            return match manifest.model_by_name(&file.model) {
                Some(model) if !model.os.is_empty() => is_allowed_for(&model.os, current),
                _ => true,
            };
        }
        // LWJGL patché du CDN (`Sys` → `lwjgl_pala`) : exclu sur
        // Linux/macOS où le vanilla Mojang gagne ; servi tel quel sur
        // Windows (payload natif du seau).
        if is_cdn_patched_lwjgl(file) && current != Os::Windows {
            return false;
        }
        return true;
    }
    if !is_allowed_for(&file.os, current) {
        return false;
    }
    match manifest.model_by_name(&file.model) {
        Some(model) => model_allowed(model, current),
        None => match Os::from_model_suffix(&file.model) {
            Some(want) => want == current,
            None => true,
        },
    }
}

/// `true` quand le plan CDN compte des bibliothèques manquantes sur disque :
/// garde-fou fraîcheur contre les `libraries/` vidés (wipe 16:41 — 33 jars
/// Mojang seuls, 304 libs CDN absentes, version `cdn_last.json` pourtant
/// « à jour »).
///
/// Ne couvre que les modèles `libraries-*` et `versions` (lancement
/// critique : launchwrapper + Forge + vanilla — jamais les `mods/`, où un
/// `.jar.disabled` volontaire ne doit pas passer pour du stale). Présence
/// seule (`is_file`), sans hachage : le pipeline de téléchargement
/// re-vérifie (taille + SHA1) de toute façon. Hermétique (aucun réseau).
pub fn cdn_libraries_stale(manifest: &Manifest, game_dir: &Path, current: Os) -> bool {
    plan_downloads(manifest, game_dir, current)
        .iter()
        .filter(|p| {
            is_libraries_model(&p.file.model)
                || manifest
                    .model_by_name(&p.file.model)
                    .is_some_and(|m| m.dest == "versions" || m.name == "versions")
        })
        .any(|p| !p.dest.is_file())
}

/// Current-OS shorthand for [`file_allowed`] using [`Os::current`].
pub fn should_download(manifest: &Manifest, file: &ManifestFile) -> bool {
    file_allowed(manifest, file, Os::current())
}

/// Resolve where `file` must be written under `game_dir`.
///
/// Steps:
/// 1. look up the owning model's `dest` (unknown model → write `path` directly
///    under `game_dir`);
/// 2. apply the `libraries-*` → `mods/` remap: when `file.model` starts with
///    `"libraries-"` and `file.path` starts with `"mods/"`, anchor under
///    `<gameDir>/mods/…` instead of `<gameDir>/libraries/mods/…`.
///
/// Destination names are the manifest names as-is (`.jar`) : vanilla Forge
/// découvre les `.jar`, aucun mapping d'extension n'est appliqué.
///
/// Returns [`CoreError::UnsafePath`] when `file.path` or the model's `dest`
/// is absolute or contains a parent-dir / root / prefix component, or when
/// the joined destination escapes `game_dir`.
pub fn resolve_local_path(
    game_dir: &Path,
    manifest: &Manifest,
    file: &ManifestFile,
) -> Result<PathBuf, CoreError> {
    let dest = manifest
        .model_by_name(&file.model)
        .map(|m| m.dest.as_str())
        .unwrap_or("");

    validate_rel_path(&file.path)?;
    validate_rel_path(dest)?;

    // Normalize path separators (paths use '/' on the CDN on every OS).
    let rel = file.path.replace('\\', "/");

    // libraries-* remap (see module docs).
    let joined: PathBuf = if file.model.starts_with("libraries-") {
        if let Some(stripped) = rel.strip_prefix("mods/") {
            game_dir.join("mods").join(stripped)
        } else if dest.is_empty() {
            game_dir.join(&rel)
        } else {
            game_dir.join(dest).join(&rel)
        }
    } else if dest.is_empty() {
        game_dir.join(&rel)
    } else {
        game_dir.join(dest).join(&rel)
    };

    // Defense-in-depth: the joined destination must stay under `game_dir`.
    // (`ParentDir` payloads are already rejected above; this catches absolute
    // `rel`/`dest` smuggling through `Path::join`, which discards its base.)
    if !joined.starts_with(game_dir) {
        return Err(CoreError::UnsafePath(file.path.clone()));
    }
    Ok(joined)
}

/// A manifest file paired with its resolved on-disk destination.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Original manifest entry (kept for `url` / `size` / `sha1` at download time).
    pub file: ManifestFile,
    /// Absolute destination path under `game_dir`.
    pub dest: PathBuf,
}

/// Reject manifest-provided relative paths that could escape `game_dir`.
///
/// Rejects (OS-independent, so a malicious CDN payload cannot exploit a
/// cross-platform manifest):
/// - absolute paths (`/etc/passwd`, `\Windows\…`, `C:/…`, `C:\…`, UNC `//host/…`);
/// - any `..` (parent-dir), `/` (root) or Windows prefix (`C:`, `\\?\…`)
///   component in `file.path` or a model's `dest`.
///
/// An empty string (unknown model → write directly under `game_dir`) is
/// accepted; `.` (current-dir) components are harmless and accepted.
pub fn validate_rel_path(raw: &str) -> Result<(), CoreError> {
    if raw.is_empty() {
        return Ok(());
    }
    let normalized = raw.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(CoreError::UnsafePath(raw.to_string()));
    }
    // Windows drive letter (`C:/…`, `C:…`) or UNC (`//host/…`).
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Err(CoreError::UnsafePath(raw.to_string()));
    }
    if normalized.starts_with("//") {
        return Err(CoreError::UnsafePath(raw.to_string()));
    }
    let path = PathBuf::from(&normalized);
    if path.is_absolute() {
        return Err(CoreError::UnsafePath(raw.to_string()));
    }
    for comp in path.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CoreError::UnsafePath(raw.to_string()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Clé de déduplication du plan : `(model, path)` identiques, même
/// destination résolue et même `sha1` — un seul téléchargement/vérification
/// /entrée classpath. Des fichiers réellement distincts (autre destination
/// ou autre `sha1`) ne fusionnent jamais.
pub fn plan_dedupe_key(file: &ManifestFile, dest: &Path) -> (String, String, String, String) {
    (
        file.model.clone(),
        file.path.replace('\\', "/"),
        dest.to_string_lossy().replace('\\', "/"),
        file.sha1.trim().to_ascii_lowercase(),
    )
}

/// Filter a manifest down to the files relevant on `current` OS, each paired
/// with its resolved destination. Order is preserved from the manifest.
///
/// Entries whose `path`/`dest` fail [`validate_rel_path`] (or escape
/// `game_dir`) are skipped — a malicious CDN payload must never cause writes
/// outside the instance root. Duplicate entries (le CDN liste chaque fichier
/// deux fois, ex. nashorn-core 15.4) fusionnent via [`plan_dedupe_key`].
pub fn plan_downloads(manifest: &Manifest, game_dir: &Path, current: Os) -> Vec<PlannedFile> {
    let mut seen: HashSet<(String, String, String, String)> = HashSet::new();
    manifest
        .files
        .iter()
        .filter(|f| file_allowed(manifest, f, current))
        .filter_map(|f| {
            resolve_local_path(game_dir, manifest, f)
                .ok()
                .map(|dest| PlannedFile {
                    file: f.clone(),
                    dest,
                })
        })
        .filter(|planned| seen.insert(plan_dedupe_key(&planned.file, &planned.dest)))
        .collect()
}

/// Current-OS shorthand for [`plan_downloads`].
pub fn plan_downloads_for_current_os(manifest: &Manifest, game_dir: &Path) -> Vec<PlannedFile> {
    plan_downloads(manifest, game_dir, Os::current())
}

/// Plafond bytecode du runtime classique (Java 8, FML 1.7.10/ASM 5).
pub const CLASSIC_MAX_CLASS_VERSION: u32 = 52;
/// Plafond bytecode du runtime expérimental (Java 21).
pub const EXPERIMENTAL_MAX_CLASS_VERSION: u32 = 65;

/// Plafond selon la saveur (`false` = classique Java 8, `true` = expérimental Java 21).
pub fn class_version_cap_for(experimental: bool) -> u32 {
    if experimental {
        EXPERIMENTAL_MAX_CLASS_VERSION
    } else {
        CLASSIC_MAX_CLASS_VERSION
    }
}

/// Plus grande version majeure `.class` d'un jar (`None` : illisible,
/// absent, non-zip ou sans aucune classe). Ignore les entrées hors
/// `*.class` et saute `META-INF/` (dont `module-info.class`, dont la
/// seule présence en v53+ tuait FML — la version réelle compte aussi).
/// Lecture pure sur disque, sans réseau.
pub fn max_class_version_in_jar(path: &Path) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let mut max: Option<u32> = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).ok()?;
        let name = entry.name().to_string();
        if name.starts_with("META-INF/") {
            continue;
        }
        if !name.ends_with(".class") {
            continue;
        }
        let mut header = [0u8; 8];
        use std::io::Read;
        let mut filled = 0usize;
        while filled < header.len() {
            match entry.read(&mut header[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => break,
            }
        }
        if filled < 8 {
            continue;
        }
        if header[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            continue;
        }
        let major = u16::from_be_bytes([header[6], header[7]]) as u32;
        max = Some(max.map_or(major, |m: u32| m.max(major)));
    }
    max
}

/// Jars déjà signalés dans ce processus : `ensure` (plan) + `launch_argv`
/// scannent le même classpath à chaque lancement, donc chaque jar avertit
/// UNE fois par `(chemin, version, plafond)` — les passages suivants se
/// taisent au lieu d'imprimer la note française en double.
static WARNED_JAR_VERSIONS: OnceLock<Mutex<HashSet<(String, u32, u32)>>> = OnceLock::new();

fn warned_jar_versions() -> &'static Mutex<HashSet<(String, u32, u32)>> {
    WARNED_JAR_VERSIONS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Avertissements bytecode (français, jars nommés) pour les jars existants
/// du classpath qui dépassent `cap`. Les fichiers absents sont ignorés
/// (pas encore téléchargés au moment du plan) ; les non-jars aussi.
///
/// WARN-ONLY, jamais d'échec : FML lui-même ignore les jars illisibles
/// (`Zip file … failed to read properly, it will be ignored` — le boot a
/// CONTINUÉ : 69 mods, init GL, menu), donc bloquer l'install ou le
/// lancement serait strictement pire. Les appelants journalisent le retour
/// (niveau info/debug) et lancent quand même. Même scanner
/// ([`max_class_version_in_jar`]) qu'avant — seul le verdict a changé.
///
/// Dédupliqué par processus (voir `WARNED_JAR_VERSIONS`) : `ensure` puis
/// `launch_argv` scannent le même classpath, chaque jar n'avertit qu'une
/// fois par `(chemin, version, plafond)`.
pub fn check_jar_class_versions(paths: &[PathBuf], cap: u32) -> Vec<String> {
    let mut warnings = Vec::new();
    for path in paths {
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|e| !e.eq_ignore_ascii_case("jar"))
        {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        if let Some(version) = max_class_version_in_jar(path) {
            if version > cap {
                let key = (
                    path.to_string_lossy().replace('\\', "/"),
                    version,
                    cap,
                );
                // Verrou empoisonné : on avertit (fail-open), jamais de
                // panique sur une voie d'avertissement.
                let already = warned_jar_versions()
                    .lock()
                    .map(|mut seen| !seen.insert(key))
                    .unwrap_or(false);
                if already {
                    continue;
                }
                warnings.push(format!(
                    "Bytecode récent ignoré (avertissement) : {} contient des classes en version v{version} (maximum v{cap} pour ce runtime) — FML l'ignorera si illisible, le lancement continue.",
                    path.to_string_lossy()
                ));
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    const JSON: &str = include_str!("../tests/fixtures/minimal_manifest.json");

    fn manifest() -> Manifest {
        parse_manifest(JSON).unwrap()
    }

    #[test]
    fn os_filter_keeps_current_os_only() {
        let m = manifest();
        let linux: Vec<_> = m
            .files
            .iter()
            .filter(|f| file_allowed(&m, f, Os::Linux))
            .map(|f| f.name.as_str())
            .collect();
        // Cross-platform files are kept.
        assert!(linux.contains(&"shared-mod.jar"));
        // Matching-OS files are kept.
        assert!(linux.contains(&"linux-native.so"));
        // Other-OS files are dropped.
        assert!(!linux.contains(&"windows-native.dll"));

        let windows: Vec<_> = m
            .files
            .iter()
            .filter(|f| file_allowed(&m, f, Os::Windows))
            .map(|f| f.name.as_str())
            .collect();
        assert!(windows.contains(&"windows-native.dll"));
        assert!(!windows.contains(&"linux-native.so"));
    }

    /// Manifeste miroir du CDN pour la non-régression serveur dédié :
    /// modèle `libraries-windows` (classpath, WINDOWS) avec le jar serveur
    /// Mojang + une vraie bibliothèque, modèle `versions` avec le Forge,
    /// modèle `mods` avec un faux positif potentiel.
    fn server_manifest() -> Manifest {
        parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "net.minecraft.launchwrapper.Launch",
            "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "libraries-windows", "dest": "libraries",
                 "properties": {"classpath": true}, "os": ["WINDOWS"]},
                {"name": "versions", "dest": "versions"},
                {"name": "mods", "dest": "mods"}
            ],
            "files": [
                {"name": "server-1.7.10.jar", "model": "libraries-windows",
                 "url": "https://x/server-1.7.10.jar", "size": 2, "sha1": "b",
                 "path": "net/minecraft/server/1.7.10/server-1.7.10.jar", "os": ["WINDOWS"]},
                {"name": "gson-2.8.6.jar", "model": "libraries-windows",
                 "url": "https://x/gson.jar", "size": 1, "sha1": "a",
                 "path": "com/google/code/gson/gson/2.8.6/gson-2.8.6.jar", "os": ["WINDOWS"]},
                {"name": "server-1.7.10.jar", "model": "libraries-windows",
                 "url": "https://x/server-1.7.10.jar", "size": 2, "sha1": "b",
                 "path": "miroir/server-1.7.10.jar", "os": ["WINDOWS"]},
                {"name": "server-tools.jar", "model": "mods",
                 "url": "https://x/server-tools.jar", "size": 4, "sha1": "d",
                 "path": "server-tools.jar", "os": []}
            ]
        }"#,
        )
        .unwrap()
    }

    #[test]
    fn dedicated_server_jar_never_downloads_on_any_os() {
        // Non-régression `NoSuchMethodError JsonUtils.func_151218_a` : le jar
        // serveur recouvrait le `pu` vanilla (1247 classes partagées) car il
        // précédait le vanilla au classpath, ajouté en dernier.
        let m = server_manifest();
        let by_path = |path: &str| m.files.iter().find(|f| f.path == path).unwrap().clone();
        let cdn_server = by_path("net/minecraft/server/1.7.10/server-1.7.10.jar");
        let moved_server = by_path("miroir/server-1.7.10.jar");
        let mod_lookalike = by_path("server-tools.jar");
        assert!(is_dedicated_server_file(&m, &cdn_server));
        // Repli par nom seul quand le chemin CDN bouge.
        assert!(is_dedicated_server_file(&m, &moved_server));
        // ... mais jamais un mod.
        assert!(!is_dedicated_server_file(&m, &mod_lookalike));
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            // Sans la garde, la réutilisation cross-platform gardait le jar
            // sur Linux/macOS (nom sans « natives »).
            assert!(
                !file_allowed(&m, &cdn_server, os),
                "serveur dédié exclu sur {os:?}"
            );
            assert!(
                !file_allowed(&m, &moved_server, os),
                "serveur déplacé exclu sur {os:?}"
            );
            assert!(file_allowed(&m, &mod_lookalike, os));
        }
        // Seau `libraries-*` cross-platform (FS-Patches + leçon du wipe
        // 16:41) : le pur Java `libraries-windows` est inclus sur TOUS les
        // OS — seuls serveur dédié, PalaForge/blacklist, LWJGL patché
        // (Linux/macOS) et binaires `*natives*` restent exclus.
        let gson = by_path("com/google/code/gson/gson/2.8.6/gson-2.8.6.jar");
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            assert!(file_allowed(&m, &gson, os), "gson gardé sur {os:?}");
        }
        // Le plan de téléchargement ne contient plus le serveur (ni son
        // doublon déplacé), sans perdre le reste.
        let dir = tempfile::tempdir().unwrap();
        let planned_win = plan_downloads(&m, dir.path(), Os::Windows);
        assert!(
            planned_win
                .iter()
                .all(|p| p.file.name != "server-1.7.10.jar"),
            "plan sans serveur : {:?}",
            planned_win.iter().map(|p| &p.file.name).collect::<Vec<_>>()
        );
        assert!(planned_win.iter().any(|p| p.file.name == "gson-2.8.6.jar"));
        assert!(planned_win
            .iter()
            .any(|p| p.file.name == "server-tools.jar"));
        let planned_linux = plan_downloads(&m, dir.path(), Os::Linux);
        assert!(
            planned_linux
                .iter()
                .all(|p| p.file.name != "server-1.7.10.jar"),
            "plan sans serveur : {:?}",
            planned_linux
                .iter()
                .map(|p| &p.file.name)
                .collect::<Vec<_>>()
        );
        assert!(
            planned_linux
                .iter()
                .any(|p| p.file.name == "gson-2.8.6.jar"),
            "gson windows DANS le plan Linux (seau cross-platform)"
        );
        assert!(planned_linux
            .iter()
            .any(|p| p.file.name == "server-tools.jar"));
    }

    #[test]
    fn palaforge_excluded_from_plan_and_classpath_on_every_os() {
        // PalaForge est remplacé par l'universel officiel 1614 (Forge Maven,
        // voir `crate::mojang`) : ni téléchargé, ni au classpath — sur tous
        // les OS et tous les modèles. Un fichier déjà présent reste, inerte.
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "libraries-windows", "dest": "libraries",
                 "properties": {"classpath": true}, "os": ["WINDOWS"]},
                {"name": "versions", "dest": "versions"}
            ],
            "files": [
                {"name": "palaforge-1.7.10-10.13.4.2512.jar", "model": "libraries-windows",
                 "url": "https://x/palaforge.jar", "size": 3, "sha1": "c",
                 "path": "fr/paladium/palaforge/1.7.10-10.13.4.2512/palaforge-1.7.10-10.13.4.2512.jar", "os": ["WINDOWS"]},
                {"name": "palaforge-1.7.10-10.13.4.2512.jar", "model": "versions",
                 "url": "https://x/palaforge.jar", "size": 3, "sha1": "c",
                 "path": "palaforge-1.7.10-10.13.4.2512.jar", "os": []},
                {"name": "gson-2.8.6.jar", "model": "libraries-windows",
                 "url": "https://x/gson.jar", "size": 1, "sha1": "a",
                 "path": "com/google/code/gson/gson/2.8.6/gson-2.8.6.jar", "os": ["WINDOWS"]}
            ]
        }"#,
        )
        .unwrap();
        for f in m.files.iter().filter(|f| f.name.contains("palaforge")) {
            assert!(is_palaforge_file(f), "reconnu par nom/chemin : {}", f.path);
        }
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            assert!(
                m.files
                    .iter()
                    .filter(|f| file_allowed(&m, f, os))
                    .all(|f| !f.name.contains("palaforge")),
                "palaforge exclu du plan sur {os:?}"
            );
        }
        // ... pendant que les vraies bibliothèques du seau `libraries-*`
        // restent prises sur TOUS les OS (cross-platform — wipe 16:41).
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            assert!(
                m.files
                    .iter()
                    .filter(|f| file_allowed(&m, f, os))
                    .any(|f| f.name == "gson-2.8.6.jar"),
                "gson gardé sur {os:?}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let planned = plan_downloads(&m, dir.path(), Os::Windows);
        assert!(
            planned.iter().all(|p| !is_palaforge_file(&p.file)),
            "plan sans palaforge : {:?}",
            planned.iter().map(|p| &p.file.name).collect::<Vec<_>>()
        );
        assert!(planned.iter().any(|p| p.file.name == "gson-2.8.6.jar"));
    }

    #[test]
    fn palaforge_utils_mod_flows_through_plan_verify_and_classpath() {
        // Non-régression de la mort silencieuse des phases FML
        // (`MissingModsException` pour `required-after:palaforge-utils`,
        // avalée : 8 mods dont PalaMod restaient `Unloaded`) : le mod
        // `PalaForgeUtils-6.6.2.jar` du pack N'EST PAS le fork PalaForge et
        // doit suivre le circuit normal (plan, vérification, classpath,
        // onglet mods) comme tout mod du manifeste. Seul l'artefact exact
        // du fork reste exclu.
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "mods", "dest": "mods"},
                {"name": "versions", "dest": "versions"}
            ],
            "files": [
                {"name": "PalaForgeUtils-6.6.2.jar", "model": "mods",
                 "url": "https://x/PalaForgeUtils-6.6.2.jar", "size": 9, "sha1": "aa",
                 "path": "PalaForgeUtils-6.6.2.jar", "os": []},
                {"name": "palaforge-1.7.10-10.13.4.2512.jar", "model": "versions",
                 "url": "https://x/palaforge.jar", "size": 3, "sha1": "c",
                 "path": "fr/paladium/palaforge/1.7.10-10.13.4.2512/palaforge-1.7.10-10.13.4.2512.jar", "os": []}
            ]
        }"#,
        )
        .unwrap();
        let utils = m
            .files
            .iter()
            .find(|f| f.name == "PalaForgeUtils-6.6.2.jar")
            .unwrap();
        let fork = m
            .files
            .iter()
            .find(|f| f.name == "palaforge-1.7.10-10.13.4.2512.jar")
            .unwrap();
        // Le mod passe la garde exacte, le fork reste exclu — sur tous les OS.
        assert!(!is_palaforge_file(utils), "PalaForgeUtils n'est pas le fork");
        assert!(!is_file_blacklisted(&utils.name));
        assert!(is_palaforge_file(fork), "artefact exact du fork exclu");
        assert!(is_file_blacklisted(&fork.name));
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            assert!(
                file_allowed(&m, utils, os),
                "PalaForgeUtils gardé sur {os:?}"
            );
            assert!(!file_allowed(&m, fork, os), "fork exclu sur {os:?}");
        }
        // Monde ensemencé où seul PalaForgeUtils manque : le plan le
        // contient (re-téléchargement automatique au JOUER/verify suivant)
        // et la vérification le signale stale.
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        for p in plan_downloads(&m, game, Os::Linux) {
            if p.file.name == "PalaForgeUtils-6.6.2.jar" {
                continue;
            }
            if let Some(parent) = p.dest.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p.dest, b"x").unwrap();
        }
        let planned = plan_downloads(&m, game, Os::Linux);
        let target = planned
            .iter()
            .find(|p| p.file.name == "PalaForgeUtils-6.6.2.jar")
            .expect("PalaForgeUtils planifié malgré le fork exclu");
        assert!(
            crate::download::needs_update(&target.dest, target.file.size, &target.file.sha1),
            "PalaForgeUtils absent → stale (JOUER/verify le re-télécharge)"
        );
        assert!(
            planned.iter().all(|p| p.file.name
                != "palaforge-1.7.10-10.13.4.2512.jar"),
            "plan sans le fork : {:?}",
            planned.iter().map(|p| &p.file.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lwjgl_matcher_keeps_legitimate_neighbors() {
        // Audit du carve-out LWJGL : les vraies cibles restent exclues sur
        // Linux/macOS (vanilla Mojang gagne) et servies sur Windows, pendant
        // que les voisins légitimes passent partout — y compris un mod dont
        // le nom ressemble (`lwjgl-…` hors seau `libraries-*` n'est jamais
        // filtré, le carve-out ne s'applique que dans le seau).
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "libraries-windows", "dest": "libraries",
                 "properties": {"classpath": true}, "os": ["WINDOWS"]},
                {"name": "mods", "dest": "mods"}
            ],
            "files": [
                {"name": "lwjgl_util-2.9.1.jar", "model": "libraries-windows",
                 "url": "https://x/lwjgl_util.jar", "size": 1, "sha1": "a",
                 "path": "org/lwjgl/lwjgl/lwjgl_util/2.9.1/lwjgl_util-2.9.1.jar", "os": ["WINDOWS"]},
                {"name": "lwjgl3ify.jar", "model": "mods",
                 "url": "https://x/lwjgl3ify.jar", "size": 2, "sha1": "b",
                 "path": "lwjgl3ify.jar", "os": []},
                {"name": "lwjgl-tweaker.jar", "model": "mods",
                 "url": "https://x/tweaker.jar", "size": 3, "sha1": "c",
                 "path": "lwjgl-tweaker.jar", "os": []},
                {"name": "my-lwjgl-wrapper.jar", "model": "libraries-windows",
                 "url": "https://x/wrapper.jar", "size": 4, "sha1": "d",
                 "path": "com/example/my-lwjgl-wrapper.jar", "os": ["WINDOWS"]}
            ]
        }"#,
        )
        .unwrap();
        let by_name = |name: &str| m.files.iter().find(|f| f.name == name).unwrap().clone();
        let util = by_name("lwjgl_util-2.9.1.jar");
        assert!(is_cdn_patched_lwjgl(&util), "vrai LWJGL patché reconnu");
        assert!(!file_allowed(&m, &util, Os::Linux));
        assert!(!file_allowed(&m, &util, Os::Macos));
        assert!(
            file_allowed(&m, &util, Os::Windows),
            "Windows sert le payload CDN tel quel"
        );
        // Voisins : jamais exclus, sur aucun OS.
        for name in ["lwjgl3ify.jar", "lwjgl-tweaker.jar", "my-lwjgl-wrapper.jar"] {
            let f = by_name(name);
            assert!(!is_cdn_patched_lwjgl(&f), "{name} n'est pas du LWJGL patché");
            for os in [Os::Windows, Os::Macos, Os::Linux] {
                assert!(file_allowed(&m, &f, os), "{name} gardé sur {os:?}");
            }
        }
        // Le plan Linux garde les voisins sans le LWJGL patché.
        let dir = tempfile::tempdir().unwrap();
        let planned = plan_downloads(&m, dir.path(), Os::Linux);
        let names: Vec<_> = planned.iter().map(|p| p.file.name.as_str()).collect();
        assert!(!names.contains(&"lwjgl_util-2.9.1.jar"), "{names:?}");
        for name in ["lwjgl3ify.jar", "lwjgl-tweaker.jar", "my-lwjgl-wrapper.jar"] {
            assert!(names.contains(&name), "{name} planifié : {names:?}");
        }
    }

    #[test]
    fn windows_model_pure_java_included_but_natives_stay_scoped() {
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [{"name": "libraries-windows", "dest": "libraries", "os": ["WINDOWS"]}],
            "files": [
                {"name": "scala.jar", "model": "libraries-windows",
                 "url": "https://x/scala.jar", "size": 1, "sha1": "a",
                 "path": "org/scala.jar", "os": ["WINDOWS"]},
                {"name": "thing-natives-windows.jar", "model": "libraries-windows",
                 "url": "https://x/n.jar", "size": 1, "sha1": "b",
                 "path": "org/n.jar", "os": ["WINDOWS"]},
                {"name": "lwjgl-2.9.1.jar", "model": "libraries-windows",
                 "url": "https://x/lwjgl.jar", "size": 1, "sha1": "c",
                 "path": "org/lwjgl/lwjgl/lwjgl/2.9.1/lwjgl-2.9.1.jar", "os": ["WINDOWS"]}
            ]
        }"#,
        )
        .unwrap();
        // Seau `libraries-*` cross-platform (FS-Patches + wipe 16:41) : le
        // pur Java est INCLUS sur Linux/macOS malgré `os=[WINDOWS]`.
        // Restent exclus : le vrai natif `*natives*` (scopé OS même dans le
        // seau) et le LWJGL patché CDN sur Linux/macOS (vanilla Mojang
        // gagne). Le warning nashorn v53 n'est plus un échec (FML ignore et
        // continue — warn-only).
        for os in [Os::Linux, Os::Macos] {
            let kept: Vec<_> = m
                .files
                .iter()
                .filter(|f| file_allowed(&m, f, os))
                .map(|f| f.name.as_str())
                .collect();
            assert!(
                kept.contains(&"scala.jar"),
                "pur Java libraries-windows INCLUS sur {os:?} : {kept:?}"
            );
            assert!(
                !kept.contains(&"thing-natives-windows.jar"),
                "natif windows toujours exclu sur {os:?} : {kept:?}"
            );
            assert!(
                !kept.contains(&"lwjgl-2.9.1.jar"),
                "lwjgl patché CDN exclu sur {os:?} (vanilla Mojang) : {kept:?}"
            );
        }
        assert!(is_cdn_patched_lwjgl(
            m.files
                .iter()
                .find(|f| f.name == "lwjgl-2.9.1.jar")
                .unwrap()
        ));
        // Windows : tout le seau natif est servi (pur Java + natifs + LWJGL
        // patché CDN tel quel).
        let windows: Vec<_> = m
            .files
            .iter()
            .filter(|f| file_allowed(&m, f, Os::Windows))
            .map(|f| f.name.as_str())
            .collect();
        assert!(windows.contains(&"scala.jar"));
        assert!(windows.contains(&"thing-natives-windows.jar"));
        assert!(windows.contains(&"lwjgl-2.9.1.jar"));
        // Miroir : un natif linux reste exclu sur Windows (scopé OS).
        let m2: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [{"name": "libraries-linux", "dest": "libraries", "os": ["LINUX"]}],
            "files": [
                {"name": "pure.jar", "model": "libraries-linux",
                 "url": "https://x/p.jar", "size": 1, "sha1": "a",
                 "path": "org/p.jar", "os": ["LINUX"]},
                {"name": "thing-natives-linux.jar", "model": "libraries-linux",
                 "url": "https://x/n.jar", "size": 1, "sha1": "b",
                 "path": "org/n.jar", "os": ["LINUX"]}
            ]
        }"#,
        )
        .unwrap();
        let win: Vec<_> = m2
            .files
            .iter()
            .filter(|f| file_allowed(&m2, f, Os::Windows))
            .map(|f| f.name.as_str())
            .collect();
        assert!(
            win.contains(&"pure.jar"),
            "pur Java linux INCLUS sur Windows : {win:?}"
        );
        assert!(
            !win.contains(&"thing-natives-linux.jar"),
            "natif linux exclu sur Windows : {win:?}"
        );
    }

    #[test]
    fn blacklist_file_excluded_on_every_os() {
        // Miroir FS-Patches `BLACKLISTED_FILES` : le Forge custom Paladium
        // (nom exact historique) ne se télécharge jamais, sur aucun OS —
        // même hors motif `palaforge*` (double garde avec `is_palaforge_file`).
        assert!(is_file_blacklisted("palaforge-1.7.10-10.13.4.2512.jar"));
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [{"name": "libraries-windows", "dest": "libraries", "os": ["WINDOWS"]}],
            "files": [
                {"name": "palaforge-1.7.10-10.13.4.2512.jar", "model": "libraries-windows",
                 "url": "https://x/p.jar", "size": 3, "sha1": "c",
                 "path": "quelque/part/palaforge-1.7.10-10.13.4.2512.jar", "os": ["WINDOWS"]},
                {"name": "gson-2.8.6.jar", "model": "libraries-windows",
                 "url": "https://x/g.jar", "size": 1, "sha1": "a",
                 "path": "com/google/gson.jar", "os": ["WINDOWS"]}
            ]
        }"#,
        )
        .unwrap();
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            let kept: Vec<_> = m
                .files
                .iter()
                .filter(|f| file_allowed(&m, f, os))
                .map(|f| f.name.as_str())
                .collect();
            assert!(
                !kept.contains(&"palaforge-1.7.10-10.13.4.2512.jar"),
                "blacklist sur {os:?}"
            );
            assert!(kept.contains(&"gson-2.8.6.jar"), "voisin gardé sur {os:?}");
        }
    }

    #[test]
    fn plan_downloads_dedupes_duplicate_entries() {
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [{"name": "mods", "dest": "mods"}],
            "files": [
                {"name": "a.jar", "model": "mods",
                 "url": "https://x/a.jar", "size": 1, "sha1": "a", "path": "a.jar"},
                {"name": "a.jar", "model": "mods",
                 "url": "https://x/a.jar", "size": 1, "sha1": "a", "path": "a.jar"}
            ]
        }"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let planned = plan_downloads(&m, dir.path(), Os::Linux);
        assert_eq!(planned.len(), 1);
    }

    fn nashorn_manifest() -> Manifest {
        // Miroir de l'incident : nashorn-core 15.4 listé DEUX fois sous
        // `libraries-windows`/`WINDOWS`, plus une entrée universelle.
        parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "libraries-windows", "dest": "libraries",
                 "properties": {"classpath": true}, "os": ["WINDOWS"]},
                {"name": "mods", "dest": "mods"}
            ],
            "files": [
                {"name": "nashorn-core-15.4.jar", "model": "libraries-windows",
                 "url": "https://x/nashorn.jar", "size": 10, "sha1": "aa",
                 "path": "org/openjdk/nashorn/nashorn-core/15.4/nashorn-core-15.4.jar", "os": ["WINDOWS"]},
                {"name": "nashorn-core-15.4.jar", "model": "libraries-windows",
                 "url": "https://x/nashorn.jar", "size": 10, "sha1": "aa",
                 "path": "org/openjdk/nashorn/nashorn-core/15.4/nashorn-core-15.4.jar", "os": ["WINDOWS"]},
                {"name": "shared-mod.jar", "model": "mods",
                 "url": "https://x/shared.jar", "size": 11, "sha1": "bb",
                 "path": "shared-mod.jar", "os": []}
            ]
        }"#,
        )
        .unwrap()
    }

    #[test]
    fn windows_bucket_entry_included_on_every_os_warn_only() {
        // Seau `libraries-*` cross-platform (wipe 16:41) : nashorn-core 15.4
        // (`libraries-windows`/`WINDOWS`) rejoint le plan ET le classpath
        // Linux/macOS comme Windows — le gate bytecode étant warn-only
        // (FML ignore le jar illisible et continue : `it will be ignored`).
        // L'entrée `os: []` reste partout, elle aussi.
        let m = nashorn_manifest();
        let nashorn = m
            .files
            .iter()
            .find(|f| f.name == "nashorn-core-15.4.jar")
            .unwrap();
        let shared = m.files.iter().find(|f| f.name == "shared-mod.jar").unwrap();
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            assert!(file_allowed(&m, nashorn, os), "nashorn gardé sur {os:?}");
            assert!(file_allowed(&m, shared, os), "universel gardé sur {os:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        let linux = plan_downloads(&m, dir.path(), Os::Linux);
        // Doublon CDN fusionné : une seule entrée malgré deux lignes.
        assert_eq!(
            linux
                .iter()
                .filter(|p| p.file.name == "nashorn-core-15.4.jar")
                .count(),
            1,
            "doublon nashorn fusionné sur Linux"
        );
        assert!(linux.iter().any(|p| p.file.name == "shared-mod.jar"));
        let windows = plan_downloads(&m, dir.path(), Os::Windows);
        assert_eq!(
            windows
                .iter()
                .filter(|p| p.file.name == "nashorn-core-15.4.jar")
                .count(),
            1,
            "doublon nashorn fusionné sur Windows"
        );
        assert!(windows.iter().any(|p| p.file.name == "shared-mod.jar"));
        // Classpath miroir du plan (même règle + même déduplication).
        let linux_cp = crate::launch::build_classpath(&m, dir.path(), Os::Linux);
        assert_eq!(
            linux_cp.matches("nashorn-core-15.4.jar").count(),
            1,
            "une seule fois au cp Linux : {linux_cp}"
        );
        let win_cp = crate::launch::build_classpath(&m, dir.path(), Os::Windows);
        assert_eq!(
            win_cp.matches("nashorn-core-15.4.jar").count(),
            1,
            "une seule fois au cp Windows : {win_cp}"
        );
    }

    #[test]
    fn model_suffix_fallback_gates_when_os_empty() {
        // Le manifeste mélange `os: [...]` et suffixes `-windows` : un
        // modèle `natives-linux` SANS `os` reste scopé Linux (strict-OS).
        // (`libraries-*` est cross-platform même sans `os` — voir le test
        // seau dédié — donc ce repli ne s'y applique plus.)
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "natives-linux", "dest": "natives"},
                {"name": "libraries-linux", "dest": "libraries",
                 "properties": {"classpath": true}},
                {"name": "mods", "dest": "mods"}
            ],
            "files": [
                {"name": "lib.so", "model": "natives-linux",
                 "url": "https://x/lib.so", "size": 1, "sha1": "a",
                 "path": "lib.so", "os": []},
                {"name": "lib.jar", "model": "libraries-linux",
                 "url": "https://x/lib.jar", "size": 1, "sha1": "a",
                 "path": "org/lib.jar", "os": []},
                {"name": "u.jar", "model": "mods",
                 "url": "https://x/u.jar", "size": 1, "sha1": "b",
                 "path": "u.jar", "os": []}
            ]
        }"#,
        )
        .unwrap();
        let native = m.files.iter().find(|f| f.name == "lib.so").unwrap();
        assert!(file_allowed(&m, native, Os::Linux));
        assert!(!file_allowed(&m, native, Os::Windows));
        assert!(!file_allowed(&m, native, Os::Macos));
        // Le seau bibliothèques reste cross-platform même sans `os`.
        let lib = m.files.iter().find(|f| f.name == "lib.jar").unwrap();
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            assert!(
                file_allowed(&m, lib, os),
                "seau libraries cross-platform sur {os:?}"
            );
        }
    }

    #[test]
    fn os_field_parses_case_insensitively() {
        // Insensibilité à la casse vérifiée sur un modèle strict-OS
        // (`natives-windows` — `libraries-*` ignore le champ `os`).
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [
                {"name": "natives-windows", "dest": "natives", "os": ["windows"]},
                {"name": "mods", "dest": "mods"}
            ],
            "files": [
                {"name": "w.dll", "model": "natives-windows",
                 "url": "https://x/w.dll", "size": 1, "sha1": "a",
                 "path": "w.dll", "os": ["windows"]}
            ]
        }"#,
        )
        .unwrap();
        let w = &m.files[0];
        assert!(file_allowed(&m, w, Os::Windows));
        assert!(!file_allowed(&m, w, Os::Linux));
    }

    #[test]
    fn dedupe_key_keeps_genuinely_distinct_files() {
        // Même `(model, path)` + `sha1` identiques → un seul ; `sha1` ou
        // destination différents → distincts (jamais fusionnés).
        let m: Manifest = parse_manifest(
            r#"{
            "name": "t", "root": "t", "version": "v", "assetIndex": "",
            "mc_version": "1.7.10",
            "maintenance": {"enabled": false, "message": ""},
            "main": "m", "arguments": {"game": [], "jvm": []},
            "models": [{"name": "mods", "dest": "mods"}],
            "files": [
                {"name": "a.jar", "model": "mods",
                 "url": "https://x/a.jar", "size": 1, "sha1": "aa", "path": "a.jar"},
                {"name": "a.jar", "model": "mods",
                 "url": "https://x/a.jar", "size": 1, "sha1": "aa", "path": "a.jar"},
                {"name": "a.jar", "model": "mods",
                 "url": "https://x/a.jar", "size": 1, "sha1": "bb", "path": "a.jar"},
                {"name": "b.jar", "model": "mods",
                 "url": "https://x/b.jar", "size": 1, "sha1": "aa", "path": "b.jar"}
            ]
        }"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let planned = plan_downloads(&m, dir.path(), Os::Linux);
        // 1 (aa/a.jar fusionné) + 1 (bb/a.jar distinct) + 1 (b.jar) = 3.
        assert_eq!(planned.len(), 3, "clés distinctes gardées : {planned:?}");
    }

    fn craft_jar(
        dir: &std::path::Path,
        name: &str,
        classes: &[(&str, u32)],
        extra: &[(&str, &[u8])],
    ) -> PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let f = std::fs::File::create(&path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (entry, major) in classes {
            w.start_file(*entry, opts).unwrap();
            let mut bytes = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00];
            bytes.extend_from_slice(&(*major as u16).to_be_bytes());
            bytes.extend_from_slice(&[0x00, 0x10, 0x00, 0x00]);
            w.write_all(&bytes).unwrap();
        }
        for (entry, body) in extra {
            w.start_file(*entry, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap();
        path
    }

    #[test]
    fn scanner_reads_major_and_skips_meta_inf_and_noise() {
        let dir = tempfile::tempdir().unwrap();
        // v52 + v53 côte à côte → max v53 ; `module-info` sous META-INF
        // (v55) ignoré ; bruit non-classe ignoré.
        let jar = craft_jar(
            dir.path(),
            "mix.jar",
            &[("a/Old.class", 52), ("b/New.class", 53)],
            &[
                (
                    "META-INF/module-info.class",
                    &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 55],
                ),
                ("readme.txt", b"pas une classe"),
                ("data.bin", &[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 99]),
            ],
        );
        assert_eq!(max_class_version_in_jar(&jar), Some(53));
        // v52 seul → passe.
        let v52 = craft_jar(dir.path(), "v52.jar", &[("a/A.class", 52)], &[]);
        assert_eq!(max_class_version_in_jar(&v52), Some(52));
        // Sans classes → None ; absent → None.
        let empty = craft_jar(dir.path(), "empty.jar", &[], &[("readme.txt", b"x")]);
        assert_eq!(max_class_version_in_jar(&empty), None);
        assert_eq!(
            max_class_version_in_jar(&dir.path().join("absent.jar")),
            None
        );
    }

    #[test]
    fn classic_cap_warns_v53_without_failing_and_experimental_stays_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let v53 = craft_jar(dir.path(), "nashorn-like.jar", &[("a/N.class", 53)], &[]);
        let v52 = craft_jar(dir.path(), "ok.jar", &[("a/O.class", 52)], &[]);
        assert_eq!(class_version_cap_for(false), 52);
        assert_eq!(class_version_cap_for(true), 65);
        // Classique : v53 = avertissement français, jar nommé — PAS d'erreur
        // (FML ignore et continue : `it will be ignored`). Le plan/lancement
        // ne doit jamais échouer là-dessus (nashorn 15.4 v53).
        let warnings =
            check_jar_class_versions(std::slice::from_ref(&v53), CLASSIC_MAX_CLASS_VERSION);
        assert_eq!(warnings.len(), 1, "un avertissement, pas d'échec");
        assert!(
            warnings[0].contains("nashorn-like.jar"),
            "jar nommé : {:?}",
            warnings
        );
        assert!(
            warnings[0].contains("v53"),
            "version nommée : {:?}",
            warnings
        );
        // v52 : silence.
        assert!(
            check_jar_class_versions(std::slice::from_ref(&v52), CLASSIC_MAX_CLASS_VERSION)
                .is_empty()
        );
        // Absents et non-jars : silence aussi (pas encore téléchargés).
        assert!(check_jar_class_versions(
            &[dir.path().join("absent.jar"), dir.path().join("notes.txt")],
            CLASSIC_MAX_CLASS_VERSION,
        )
        .is_empty());
        // Expérimental : v53 sous le plafond v65 → silence.
        assert!(check_jar_class_versions(&[v53, v52], EXPERIMENTAL_MAX_CLASS_VERSION).is_empty());
        // ... mais v66 reste signalé même en expérimental (toujours sans échec).
        let v66 = craft_jar(dir.path(), "trop-recent.jar", &[("a/F.class", 66)], &[]);
        let warnings =
            check_jar_class_versions(std::slice::from_ref(&v66), EXPERIMENTAL_MAX_CLASS_VERSION);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("trop-recent.jar"));
    }

    #[test]
    fn bytecode_warnings_dedupe_per_jar_per_process() {
        // `ensure` (plan) + `launch_argv` scannent le même classpath à
        // chaque lancement : chaque jar n'avertit qu'une fois par
        // `(chemin, version, plafond)`, les passages suivants se taisent.
        let dir = tempfile::tempdir().unwrap();
        let v53 = craft_jar(dir.path(), "double.jar", &[("a/N.class", 53)], &[]);
        let first =
            check_jar_class_versions(std::slice::from_ref(&v53), CLASSIC_MAX_CLASS_VERSION);
        assert_eq!(first.len(), 1, "premier passage avertit");
        assert!(first[0].contains("double.jar"));
        let second =
            check_jar_class_versions(std::slice::from_ref(&v53), CLASSIC_MAX_CLASS_VERSION);
        assert!(
            second.is_empty(),
            "second passage silencieux : {second:?}"
        );
        // Un autre jar avertit toujours (déduplication par jar, pas
        // globale).
        let other = craft_jar(dir.path(), "autre.jar", &[("a/N.class", 53)], &[]);
        assert_eq!(
            check_jar_class_versions(std::slice::from_ref(&other), CLASSIC_MAX_CLASS_VERSION).len(),
            1,
            "voisin non averti"
        );
        // Le plafond fait partie de la clé : un jar au-dessus des deux
        // plafonds avertit une fois par plafond (messages différents).
        let v66 = craft_jar(dir.path(), "biplafond.jar", &[("a/F.class", 66)], &[]);
        assert_eq!(
            check_jar_class_versions(std::slice::from_ref(&v66), CLASSIC_MAX_CLASS_VERSION).len(),
            1,
            "premier plafond avertit"
        );
        assert_eq!(
            check_jar_class_versions(std::slice::from_ref(&v66), EXPERIMENTAL_MAX_CLASS_VERSION)
                .len(),
            1,
            "second plafond avertit aussi (clé distincte)"
        );
        assert!(
            check_jar_class_versions(std::slice::from_ref(&v66), EXPERIMENTAL_MAX_CLASS_VERSION)
                .is_empty(),
            "puis silence"
        );
    }

    #[test]
    fn stale_check_catches_wiped_libraries_but_passes_after_fill() {
        // Non-régression wipe 16:41 : `libraries/` avec le seul remplissage
        // Mojang (présence vanilla/forge/natives OK) mais sans les libs CDN
        // planifiées (launchwrapper, …) → stale=true ; après remplissage
        // simulé → fresh. Le JOUER suivant re-télécharge automatiquement.
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
                {"name": "gson-2.8.6.jar", "model": "libraries-windows",
                 "url": "https://x/gson.jar", "size": 1, "sha1": "b",
                 "path": "com/google/code/gson/gson/2.8.6/gson-2.8.6.jar", "os": ["WINDOWS"]},
                {"name": "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "model": "versions",
                 "url": "https://x/forge.jar", "size": 5, "sha1": "c",
                 "path": "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "os": []}
            ]
        }"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        // Install « wiped » : rien sur disque → stale (launchwrapper manque).
        assert!(
            cdn_libraries_stale(&m, game, Os::Linux),
            "libs CDN absentes → stale"
        );
        // Remplissage Mojang seul (vanilla/forge présents, libs CDN toujours
        // absentes) → toujours stale : la présence du vanilla ne masque pas
        // le launchwrapper manquant.
        let vanilla = game.join("versions/1.7.10/1.7.10.jar");
        std::fs::create_dir_all(vanilla.parent().unwrap()).unwrap();
        std::fs::write(&vanilla, b"vanilla").unwrap();
        assert!(
            cdn_libraries_stale(&m, game, Os::Linux),
            "launchwrapper CDN toujours absent → stale"
        );
        // Remplissage simulé : chaque dest planifiée existe → fresh.
        for p in plan_downloads(&m, game, Os::Linux) {
            if let Some(parent) = p.dest.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p.dest, b"x").unwrap();
        }
        assert!(
            !cdn_libraries_stale(&m, game, Os::Linux),
            "après remplissage simulé → fresh"
        );
        // Un `.disabled` côté mods ne rend jamais stale (hors périmètre
        // libraries/versions) : seul un jar bibliothèque manquant compte.
    }

    #[test]
    fn download_dests_are_manifest_names_as_is() {
        // Vanilla Forge découvre les `.jar` : aucun mapping d'extension —
        // la destination est le nom du manifeste tel quel (l'épisode
        // PalaForge `.jar` ↔ `.pala` est terminé).
        let m = manifest();
        let game_dir = Path::new("/game");
        let shared = m.files.iter().find(|f| f.name == "shared-mod.jar").unwrap();
        let dest = resolve_local_path(game_dir, &m, shared).unwrap();
        assert_eq!(dest, Path::new("/game/mods/shared-mod.jar"));
        // Un nom non-`.jar` passe tel quel lui aussi (aucune réécriture).
        let pala = m.files.iter().find(|f| f.name == "legacy.pala").unwrap();
        let dest = resolve_local_path(game_dir, &m, pala).unwrap();
        assert_eq!(dest, Path::new("/game/mods/legacy.pala"));
        let mut m2 = m.clone();
        m2.models.push(crate::manifest::Model {
            name: "extra-libs".to_string(),
            version: None,
            description: None,
            dest: "libraries".to_string(),
            properties: None,
            os: vec![],
        });
        let mut f = pala.clone();
        f.model = "extra-libs".to_string();
        let dest = resolve_local_path(game_dir, &m2, &f).unwrap();
        assert_eq!(dest, Path::new("/game/libraries/legacy.pala"));
    }

    #[test]
    fn libraries_model_mods_path_is_remapped() {
        let m = manifest();
        let game_dir = Path::new("/game");
        let f = m
            .files
            .iter()
            .find(|x| x.name == "remapped-lib.jar")
            .unwrap();
        assert!(f.model.starts_with("libraries-"));
        let dest = resolve_local_path(game_dir, &m, f).unwrap();
        // Must NOT nest under libraries/mods/.
        assert_eq!(dest, Path::new("/game/mods/nested/remapped-lib.jar"));
    }

    #[test]
    fn traversal_paths_are_rejected() {
        let m = manifest();
        let game_dir = Path::new("/game");
        let base = m
            .files
            .iter()
            .find(|f| f.name == "shared-mod.jar")
            .unwrap()
            .clone();
        for evil in [
            "../evil.jar",
            "mods/../../evil.jar",
            "/etc/passwd",
            "C:/Windows/evil.dll",
            "C:\\Windows\\evil.dll",
            "..\\evil.jar",
            "//host/share/evil.jar",
        ] {
            let mut f = base.clone();
            f.path = evil.to_string();
            assert!(
                resolve_local_path(game_dir, &m, &f).is_err(),
                "path must be rejected: {evil}"
            );
            assert!(
                validate_rel_path(evil).is_err(),
                "validate must reject: {evil}"
            );
        }
        // Legit paths still resolve under the game dir.
        let mut ok = base.clone();
        ok.path = "mods/ok.jar".to_string();
        let dest = resolve_local_path(game_dir, &m, &ok).unwrap();
        assert!(dest.starts_with(game_dir));

        // Evil model dest is rejected too.
        let mut m2 = m.clone();
        m2.models.push(crate::manifest::Model {
            name: "evil-model".to_string(),
            version: None,
            description: None,
            dest: "../../outside".to_string(),
            properties: None,
            os: vec![],
        });
        let mut f2 = base.clone();
        f2.model = "evil-model".to_string();
        f2.path = "payload.jar".to_string();
        assert!(resolve_local_path(game_dir, &m2, &f2).is_err());
        assert!(validate_rel_path("../../outside").is_err());
    }
}
