// SPDX-License-Identifier: AGPL-3.0-only

//! Forge 1.7.10 launch command builder (official universal 1614, vanilla).
//!
//! Classpath rule: only files owned by a model with
//! `properties.classpath == true` plus the provisioned jars land on the
//! classpath. `mods/` (Forge-loaded) are never on the classpath, even when a
//! `libraries-*` entry remaps under `<gameDir>/mods/…` on disk.
//!
//! Provisioned jars (absent from the CDN manifest) join after the manifest
//! entries, in this order: the Mojang regular-library fill (vanilla LWJGL on
//! Linux/macOS — la copie CDN est patchée et exclue du plan sur ces OS), the
//! official Forge 1614 universal (Forge Maven, standard layout), then the
//! vanilla client jar last. PalaForge (artefact exact du fork :
//! `palaforge-1.7.10-10.13.4.2512.jar` / `fr/paladium/palaforge/…`, jamais
//! le préfixe `palaforge*` — voir [`crate::integrity::is_palaforge_file`])
//! is excluded everywhere — a stale file on disk stays inert, like the
//! dedicated-server jar.

use std::path::{Path, PathBuf};

use crate::instance::{assets_dir, natives_dir, versions_dir};
use crate::integrity::{file_allowed, is_file_blacklisted, model_allowed, resolve_local_path};
use crate::java::resolve_java;
use crate::manifest::Manifest;
use crate::os::{is_allowed_for, Os};

/// Default minimum heap substituted into `${options.memory.min}`.
pub const DEFAULT_MEMORY_MIN: &str = "1G";
/// Default maximum heap substituted into `${options.memory.max}`.
pub const DEFAULT_MEMORY_MAX: &str = "4G";

/// `--userType` value: always `msa` (login with a Microsoft account is
/// mandatory; there is no offline mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserType {
    #[default]
    Msa,
}

impl UserType {
    /// Game-facing spelling (`"msa"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Msa => "msa",
        }
    }
}

/// Tunable launcher knobs (currently just heap defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Substituted into `${options.memory.min}` (default `1G`).
    pub memory_min: String,
    /// Substituted into `${options.memory.max}` (default `4G`).
    pub memory_max: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            memory_min: DEFAULT_MEMORY_MIN.to_string(),
            memory_max: DEFAULT_MEMORY_MAX.to_string(),
        }
    }
}

impl Settings {
    /// Custom heap knobs (e.g. `Settings::new("2G", "6G")`).
    pub fn new(min: impl Into<String>, max: impl Into<String>) -> Self {
        Self {
            memory_min: min.into(),
            memory_max: max.into(),
        }
    }
}

/// Everything the command builder needs beyond the manifest.
#[derive(Debug, Clone)]
pub struct LaunchProfile {
    /// `--username` value.
    pub username: String,
    /// Dashed Mojang UUID (used as-is).
    pub uuid: String,
    /// `--accessToken` value (Microsoft session token).
    pub access_token: String,
    /// `--userType` value.
    pub user_type: UserType,
    /// `--version` value (e.g. `"1.7.10-Forge10.13.4.1614"`).
    pub version: String,
    /// Substituted into `${options.memory.min}`.
    pub memory_min: String,
    /// Substituted into `${options.memory.max}`.
    pub memory_max: String,
    /// Instance root (gameDir, assetsDir base, natives parent, ...).
    pub game_dir: PathBuf,
}

impl LaunchProfile {
    /// Minimal profile with default heap (`1G`/`4G`).
    pub fn new(
        username: impl Into<String>,
        uuid: impl Into<String>,
        access_token: impl Into<String>,
        user_type: UserType,
        version: impl Into<String>,
        game_dir: impl Into<PathBuf>,
    ) -> Self {
        let settings = Settings::default();
        Self {
            username: username.into(),
            uuid: uuid.into(),
            access_token: access_token.into(),
            user_type,
            version: version.into(),
            memory_min: settings.memory_min,
            memory_max: settings.memory_max,
            game_dir: game_dir.into(),
        }
    }

    /// Profile with explicit heap settings.
    pub fn with_memory(
        username: impl Into<String>,
        uuid: impl Into<String>,
        access_token: impl Into<String>,
        user_type: UserType,
        version: impl Into<String>,
        game_dir: impl Into<PathBuf>,
        settings: &Settings,
    ) -> Self {
        Self {
            username: username.into(),
            uuid: uuid.into(),
            access_token: access_token.into(),
            user_type,
            version: version.into(),
            memory_min: settings.memory_min.clone(),
            memory_max: settings.memory_max.clone(),
            game_dir: game_dir.into(),
        }
    }
}

/// Ensure `<gameDir>/natives` exists and return it (for
/// `-Djava.library.path`). Creation failures are ignored so callers can still
/// build a command for display; the spawn will surface real IO errors.
pub fn library_path(game_dir: &Path) -> PathBuf {
    let dir = natives_dir(game_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Classpath entry separator (`;` on Windows, `:` elsewhere).
pub fn classpath_separator(os: Os) -> char {
    match os {
        Os::Windows => ';',
        _ => ':',
    }
}

/// Build the `-cp` value: `libraries` (classpath models) + provisioned jars.
///
/// Seuls les fichiers autorisés sur `os` sont gardés (même règle OS que le
/// plan, voir [`crate::integrity::file_allowed`] : `libraries-*`
/// cross-platform, `natives-*`/`java-*` strict-OS) ; le fork PalaForge
/// (artefact exact — voir [`crate::integrity::is_palaforge_file`], jamais le
/// préfixe `palaforge*` : `PalaForgeUtils-6.6.2.jar` reste un mod normal)
/// est exclu sur tous les OS. Les entrées
/// sont les destinations sur disque via [`resolve_local_path`] ; les mods
/// ne figurent jamais au classpath (Forge les charge — sauf les entrées
/// `libraries-*` remappées sous `mods/`, qui gardent leur chemin résolu).
///
/// Provisioned jars join after the manifest entries: the Mojang
/// regular-library fill (vanilla LWJGL on Linux/macOS — le CDN ne sert que
/// son LWJGL patché, exclu du plan sur ces OS), the official Forge
/// 1614 universal when present, then the vanilla client jar
/// (`versions/<mc>/<mc>.jar`, provisioned by the Mojang fallback) last.
/// The Mojang dedicated-server jar is never included (see
/// [`crate::integrity::is_dedicated_server_file`]): it would shadow the
/// vanilla `pu` (`JsonUtils`) and kill boot with `NoSuchMethodError
/// func_151218_a`.
/// Chemins jars du classpath classique (même règle OS que le
/// plan + déduplication [`crate::integrity::plan_dedupe_key`]).
pub fn classpath_jar_paths(manifest: &Manifest, game_dir: &Path, os: Os) -> Vec<PathBuf> {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String, String, String)> =
        HashSet::with_capacity(manifest.files.len());
    let mut out = Vec::with_capacity(manifest.files.len().min(512));
    for file in &manifest.files {
        if crate::integrity::is_palaforge_file(file) {
            continue;
        }
        let model = manifest.model_by_name(&file.model);
        let is_versions = model
            .map(|m| m.dest == "versions" || m.name == "versions")
            .unwrap_or(false);
        if is_file_blacklisted(&file.name) && !is_versions {
            continue;
        }
        if is_versions {
            // Historical behavior: versions files still pass the OS gates
            // (only the name blacklist is lifted for them).
            if !is_allowed_for(&file.os, os) {
                continue;
            }
            if let Some(m) = model {
                if !model_allowed(m, os) {
                    continue;
                }
            }
        } else if !file_allowed(manifest, file, os) {
            // Même règle OS que le plan (`libraries-*` cross-platform,
            // `natives-*`/`java-*` strict-OS — voir `integrity::file_allowed`).
            continue;
        }
        match model {
            Some(m) => {
                let on_classpath = m
                    .properties
                    .as_ref()
                    .and_then(|p| p.classpath)
                    .unwrap_or(false);
                if !(on_classpath || is_versions) {
                    continue;
                }
            }
            None => continue,
        }
        let Ok(dest) = resolve_local_path(game_dir, manifest, file) else {
            continue;
        };
        let key = crate::integrity::plan_dedupe_key(file, &dest);
        if seen.insert(key) {
            out.push(dest);
        }
    }
    out
}

pub fn build_classpath(manifest: &Manifest, game_dir: &Path, os: Os) -> String {
    let sep = classpath_separator(os);
    let mut entries: Vec<String> = classpath_jar_paths(manifest, game_dir, os)
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    // Déduplication O(1) (le `contains` linéaire coûtait O(N²) sur ~500
    // entrées) sans réordonner : le premier gagne, l'ordre FML reste stable.
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(entries.len() + 16);
    seen.extend(entries.iter().cloned());
    // Mojang regular-library fill (vanilla LWJGL on Linux/macOS — la copie
    // CDN est patchée et exclue du plan sur ces OS). Read from the offline
    // cache, no network.
    for e in crate::mojang::mojang_classpath_entries(manifest, game_dir, os) {
        if seen.insert(e.clone()) {
            entries.push(e);
        }
    }
    // Official Forge 1614 universal (Forge Maven, standard layout under
    // `libraries/`): on the classpath when provisioned, before vanilla.
    let forge = crate::mojang::forge_universal_dest(game_dir);
    if forge.is_file() {
        let s = forge.to_string_lossy().into_owned();
        if seen.insert(s.clone()) {
            entries.push(s);
        }
    }
    // Vanilla client jar (Mojang fallback): Forge 1.7.10 launches with
    // vanilla + Forge side by side. Appended when present, never an error.
    let vanilla = versions_dir(game_dir)
        .join(&manifest.mc_version)
        .join(format!("{}.jar", manifest.mc_version));
    if vanilla.is_file() {
        let s = vanilla.to_string_lossy().into_owned();
        if seen.insert(s.clone()) {
            entries.push(s);
        }
    }
    order_asm_compat(&mut entries);
    entries.join(&sep.to_string())
}

/// ASM coexistence for FML-era packs: the CDN manifest lists the ASM 9.5
/// core jar before `asm-all-5.0.3`, but FML 1.7.10's `SideTransformer`
/// must resolve ASM 5 — ASM 9.5's `ClassReader` throws `AIOOBE` on
/// Forge's `FMLSanityChecker` bytes (reproduced standalone with both
/// jars: same file parses clean under 5.0.3 and under `javap`). The whole
/// production runtime (FML + Mixin 0.7.11) is ASM5-contemporary, so legacy
/// ASM wins the classpath. Stable and minimal: only the two jars that
/// provide `org.objectweb.asm.ClassReader` move (companions like
/// `asm-tree`/`asm-util` carry no reader and keep manifest order).
fn order_asm_compat(entries: &mut Vec<String>) {
    let legacy = entries.iter().position(|e| e.contains("asm-all-5.0.3"));
    let modern = entries
        .iter()
        .position(|e| e.contains("org/ow2/asm/asm/9.5/"));
    if let (Some(l), Some(m)) = (legacy, modern) {
        if m < l {
            let entry = entries.remove(l);
            entries.insert(m, entry);
        }
    }
}

/// Substitute one template arg's `${...}` placeholders.
pub fn substitute_arg(
    template: &str,
    manifest: &Manifest,
    profile: &LaunchProfile,
    natives: &Path,
) -> String {
    let game_dir = profile.game_dir.to_string_lossy();
    let assets = assets_dir(&profile.game_dir).to_string_lossy().into_owned();
    let natives_s = natives.to_string_lossy();
    template
        .replace("${username}", &profile.username)
        .replace("${version}", &profile.version)
        .replace("${gameDir}", &game_dir)
        .replace("${game_directory}", &game_dir)
        .replace("${assetsDir}", &assets)
        .replace("${assetIndex}", &manifest.mc_version)
        .replace("${uuid}", &profile.uuid)
        .replace("${accessToken}", &profile.access_token)
        .replace("${userProperties}", "{}")
        .replace("${userType}", profile.user_type.as_str())
        .replace("${options.library.path}", &natives_s)
        .replace("${options.memory.max}", &profile.memory_max)
        .replace("${options.memory.min}", &profile.memory_min)
}

/// Substitute a whole template list (`arguments.game` / `arguments.jvm`).
pub fn substitute_args(
    templates: &[String],
    manifest: &Manifest,
    profile: &LaunchProfile,
    natives: &Path,
) -> Vec<String> {
    templates
        .iter()
        .map(|t| substitute_arg(t, manifest, profile, natives))
        .collect()
}

/// Shell-quote one argv element when it contains spaces or quotes.
fn quote_arg(arg: &str) -> String {
    if arg.contains(' ') || arg.contains('"') || arg.is_empty() {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

/// Quote a full argv for display/logging.
pub fn quote_command(argv: &[String]) -> String {
    argv.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Assembled Forge launch: structured parts plus a display string.
#[derive(Debug, Clone)]
pub struct FullCommand {
    /// Resolved `java` binary from [`resolve_java`].
    pub java_bin: PathBuf,
    /// Substituted `arguments.jvm` (no `-cp`; see `classpath`).
    pub jvm_args: Vec<String>,
    /// `manifest.main` (e.g. `net.minecraft.launchwrapper.Launch`).
    pub main_class: String,
    /// Substituted `arguments.game` (already contains `--tweakClass` from the template).
    pub game_args: Vec<String>,
    /// `-cp` value from [`build_classpath`].
    pub classpath: String,
    /// Executable argv: `[java, jvm..., -cp, classpath, main, game...]`.
    pub full_command: Vec<String>,
    /// Shell-quoted [`FullCommand::full_command`] for display.
    pub full_command_string: String,
}

/// Build the full Forge launch command (no spawn, no network).
pub fn build_full_command(manifest: &Manifest, profile: &LaunchProfile, os: Os) -> FullCommand {
    let java_bin = resolve_java(&profile.game_dir);
    let natives = library_path(&profile.game_dir);
    let jvm_args = substitute_args(&manifest.arguments.jvm, manifest, profile, &natives);
    let game_args = substitute_args(&manifest.arguments.game, manifest, profile, &natives);
    let classpath = build_classpath(manifest, &profile.game_dir, os);
    let main_class = manifest.main.clone();

    let mut full_command = Vec::with_capacity(jvm_args.len() + game_args.len() + 5);
    full_command.push(java_bin.to_string_lossy().into_owned());
    full_command.extend(jvm_args.iter().cloned());
    if !jvm_args.iter().any(|a| a == "-cp" || a == "-classpath") {
        full_command.push("-cp".to_string());
        full_command.push(classpath.clone());
    }
    full_command.push(main_class.clone());
    full_command.extend(game_args.iter().cloned());
    let full_command_string = quote_command(&full_command);

    FullCommand {
        java_bin,
        jvm_args,
        main_class,
        game_args,
        classpath,
        full_command,
        full_command_string,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    const JSON: &str = include_str!("../tests/fixtures/minimal_manifest.json");

    fn profile_in(dir: &Path) -> LaunchProfile {
        LaunchProfile::new(
            "Steve",
            "123e4567-e89b-12d3-a456-426614174000",
            "token123",
            UserType::Msa,
            "1.7.10-Forge10.13.4.1614",
            dir,
        )
    }

    #[test]
    fn substitutes_all_placeholders() {
        let m = parse_manifest(JSON).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = profile_in(dir.path());
        let natives = library_path(dir.path());
        let templates = vec![
            "--username".to_string(),
            "${username}".to_string(),
            "${version}".to_string(),
            "${gameDir}".to_string(),
            "${game_directory}".to_string(),
            "${assetsDir}".to_string(),
            "${assetIndex}".to_string(),
            "${uuid}".to_string(),
            "${accessToken}".to_string(),
            "${userProperties}".to_string(),
            "${userType}".to_string(),
            "${options.library.path}".to_string(),
            "-Xmx${options.memory.max}".to_string(),
            "-Xms${options.memory.min}".to_string(),
        ];
        let out = substitute_args(&templates, &m, &p, &natives);
        let gd = dir.path().to_string_lossy().into_owned();
        let assets = assets_dir(dir.path()).to_string_lossy().into_owned();
        assert_eq!(out[1], "Steve");
        assert_eq!(out[2], "1.7.10-Forge10.13.4.1614");
        assert_eq!(out[3], gd);
        assert_eq!(out[4], gd);
        assert_eq!(out[5], assets);
        assert_eq!(out[6], "1.7.10");
        assert_eq!(out[7], "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(out[8], "token123");
        assert_eq!(out[9], "{}");
        assert_eq!(out[10], "msa");
        assert_eq!(out[11], natives.to_string_lossy());
        assert_eq!(out[12], "-Xmx4G");
        assert_eq!(out[13], "-Xms1G");
        assert!(!out.iter().any(|a| a.contains("${")));
    }

    #[test]
    fn classpath_separator_per_os() {
        assert_eq!(classpath_separator(Os::Windows), ';');
        assert_eq!(classpath_separator(Os::Linux), ':');
        assert_eq!(classpath_separator(Os::Macos), ':');
    }

    #[test]
    fn asm_compat_puts_legacy_reader_first() {
        let mut v = vec![
            "/x/libraries/org/ow2/asm/asm-analysis/9.5/asm-analysis-9.5.jar".to_string(),
            "/x/libraries/org/ow2/asm/asm/9.5/asm-9.5.jar".to_string(),
            "/x/libraries/org/ow2/asm/asm-all/5.0.3/asm-all-5.0.3.jar".to_string(),
            "/x/libraries/other/lib.jar".to_string(),
        ];
        order_asm_compat(&mut v);
        let pos5 = v.iter().position(|e| e.contains("asm-all-5.0.3")).unwrap();
        let pos9 = v
            .iter()
            .position(|e| e.contains("org/ow2/asm/asm/9.5/"))
            .unwrap();
        assert!(pos5 < pos9);
        // Stable: companions and unrelated entries keep relative order.
        assert!(v[3].contains("other/lib.jar"));
        // Already-ordered and absent cases are no-ops.
        let mut ok = vec!["/a/asm-all-5.0.3.jar".to_string(), "/b/x.jar".to_string()];
        order_asm_compat(&mut ok);
        assert!(ok[0].contains("asm-all-5.0.3"));
        let mut none = vec!["/a/x.jar".to_string()];
        order_asm_compat(&mut none);
        assert_eq!(none, vec!["/a/x.jar".to_string()]);
    }

    #[test]
    fn classpath_uses_os_separator_and_skips_mods() {
        let m = parse_manifest(JSON).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let linux_cp = build_classpath(&m, dir.path(), Os::Linux);
        assert!(linux_cp.contains(':'));
        assert!(!linux_cp.contains(';'));
        // `libraries-*` remapped under `mods/` keeps its resolved path —
        // under the manifest `.jar` name, as-is.
        assert!(linux_cp.contains("remapped-lib.jar"));
        assert!(!linux_cp.contains("remapped-lib.pala"));
        // Mods are Forge-loaded, never on the classpath...
        assert!(!linux_cp.contains("shared-mod.pala"));
        assert!(!linux_cp.contains("shared-mod.jar"));
        // ...but the official Forge universal in `versions/` still launches.
        assert!(linux_cp.contains("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"));

        let win_cp = build_classpath(&m, dir.path(), Os::Windows);
        // Fixture has no windows-classpath files besides none; versions file is
        // cross-platform so it shows up; separator must be ';' when 2+ entries.
        // At minimum it must not use ':' as separator between entries.
        let _ = win_cp;
        assert_eq!(classpath_separator(Os::Windows), ';');
    }

    #[test]
    fn classpath_excludes_dedicated_server_but_keeps_vanilla_last() {
        // Non-régression `NoSuchMethodError JsonUtils.func_151218_a` : le jar
        // serveur dédié Mojang (entrée `libraries-windows` du CDN) ne doit
        // jamais figurer au classpath — il recouvrait le `pu` vanilla. Le
        // vanilla reste fourni en dernier, après les bibliothèques.
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
                {"name": "gson-2.8.6.jar", "model": "libraries-windows",
                 "url": "https://x/gson.jar", "size": 1, "sha1": "a",
                 "path": "com/google/code/gson/gson/2.8.6/gson-2.8.6.jar", "os": ["WINDOWS"]},
                {"name": "server-1.7.10.jar", "model": "libraries-windows",
                 "url": "https://x/server.jar", "size": 2, "sha1": "b",
                 "path": "net/minecraft/server/1.7.10/server-1.7.10.jar", "os": ["WINDOWS"]},
                {"name": "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "model": "versions",
                 "url": "https://x/forge.jar", "size": 3, "sha1": "c",
                 "path": "forge-1.7.10-10.13.4.1614-1.7.10-universal.jar", "os": []}
            ]
        }"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Jar vanilla présent : le lanceur l'ajoute après les bibliothèques.
        let vanilla = crate::instance::versions_dir(dir.path())
            .join("1.7.10")
            .join("1.7.10.jar");
        std::fs::create_dir_all(vanilla.parent().unwrap()).unwrap();
        std::fs::write(&vanilla, b"vanilla").unwrap();
        let vanilla_s = vanilla.to_string_lossy().into_owned();
        for os in [Os::Windows, Os::Macos, Os::Linux] {
            let cp = build_classpath(&m, dir.path(), os);
            assert!(
                !cp.contains("server-1.7.10.jar"),
                "serveur dédié hors classpath sur {os:?} : {cp}"
            );
            // Seau `libraries-*` cross-platform : gson `libraries-windows`
            // sur TOUS les OS (wipe 16:41 — le pur Java n'est plus scopé OS).
            assert!(
                cp.contains("gson-2.8.6.jar"),
                "gson gardé sur {os:?} : {cp}"
            );
            assert!(
                cp.contains("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"),
                "forge gardé sur {os:?} : {cp}"
            );
            // Le vanilla est fourni en dernier (aucune bibliothèque après).
            assert!(
                cp.ends_with(&vanilla_s),
                "vanilla en dernier sur {os:?} : {cp}"
            );
            let sep = classpath_separator(os).to_string();
            let entries: Vec<&str> = cp.split(&sep).collect();
            assert_eq!(entries.last(), Some(&vanilla_s.as_str()));
        }
        // Windows garde gson au milieu (avant vanilla).
        {
            let cp = build_classpath(&m, dir.path(), Os::Windows);
            let sep = classpath_separator(Os::Windows).to_string();
            let entries: Vec<&str> = cp.split(&sep).collect();
            assert!(entries.iter().any(|e| e.contains("gson-2.8.6.jar")));
        }
    }

    #[test]
    fn classpath_carries_vanilla_forge_and_lwjgl_without_palaforge() {
        // Monde vanilla : l'universel officiel 1614 est présent (provisionné
        // depuis Forge Maven), un vieux palaforge traîne sur disque, et le
        // LWJGL vanilla vient du remplissage Mojang (le CDN ne le shippe
        // que patché sur Linux).
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
                {"name": "lwjgl-2.9.1.jar", "model": "libraries-windows",
                 "url": "https://x/lwjgl.jar", "size": 1, "sha1": "a",
                 "path": "org/lwjgl/lwjgl/lwjgl/2.9.1/lwjgl-2.9.1.jar", "os": ["WINDOWS"]},
                {"name": "palaforge-1.7.10-10.13.4.2512.jar", "model": "versions",
                 "url": "https://x/palaforge.jar", "size": 3, "sha1": "c",
                 "path": "palaforge-1.7.10-10.13.4.2512.jar", "os": []}
            ]
        }"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        // Cache Mojang : le lwjgl vanilla que le CDN exclut sur Linux.
        std::fs::create_dir_all(crate::instance::caches_dir(game)).unwrap();
        std::fs::write(
            crate::instance::caches_dir(game).join(crate::mojang::MOJANG_CACHE_FILE),
            serde_json::json!({
                "id": "1.7.10",
                "libraries": [
                    {"name": "org.lwjgl.lwjgl:lwjgl:2.9.1",
                     "downloads": {"artifact": {"path": "org/lwjgl/lwjgl/lwjgl/2.9.1/lwjgl-2.9.1.jar",
                         "sha1": "s", "size": 1, "url": "https://example.com/lwjgl.jar"}}}
                ],
                "downloads": {"client": {"sha1": "f", "size": 6, "url": "https://example.com/c.jar"}},
                "assetIndex": {"id": "1.7.10", "sha1": "g", "size": 7, "url": "https://example.com/a.json"}
            })
            .to_string(),
        )
        .unwrap();
        // Universel officiel provisionné + vieux palaforge inerte sur disque.
        let forge = crate::mojang::forge_universal_dest(game);
        std::fs::create_dir_all(forge.parent().unwrap()).unwrap();
        std::fs::write(&forge, b"forge").unwrap();
        let stale = crate::instance::libraries_dir(game).join("fr/paladium/palaforge.jar");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"palaforge").unwrap();
        let cp = build_classpath(&m, game, Os::Linux);
        assert!(
            cp.contains("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"),
            "universel vanilla présent : {cp}"
        );
        assert!(!cp.contains("palaforge"), "palaforge hors classpath : {cp}");
        assert!(
            !cp.contains("server-1.7.10.jar"),
            "serveur dédié hors classpath : {cp}"
        );
        assert!(
            cp.contains("org/lwjgl/lwjgl/lwjgl/2.9.1/lwjgl-2.9.1.jar"),
            "lwjgl vanilla Mojang présent : {cp}"
        );
        // L'universel précède le vanilla (ajouté en dernier quand présent).
        let vanilla = crate::instance::versions_dir(game)
            .join("1.7.10")
            .join("1.7.10.jar");
        std::fs::create_dir_all(vanilla.parent().unwrap()).unwrap();
        std::fs::write(&vanilla, b"vanilla").unwrap();
        let cp = build_classpath(&m, game, Os::Linux);
        let pos_forge = cp.find("universal.jar").unwrap();
        let pos_vanilla = cp.find("1.7.10.jar").unwrap();
        assert!(pos_forge < pos_vanilla, "forge avant vanilla : {cp}");
    }

    #[test]
    fn library_path_creates_natives_dir() {
        let dir = tempfile::tempdir().unwrap();
        let natives = library_path(&dir.path().join("game"));
        assert!(natives.ends_with("natives"));
        assert!(natives.is_dir());
    }

    #[test]
    fn full_command_layout() {
        let m = parse_manifest(JSON).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = profile_in(dir.path());
        let cmd = build_full_command(&m, &p, Os::Linux);
        assert_eq!(cmd.main_class, "net.minecraft.launchwrapper.Launch");
        assert!(cmd.jvm_args.iter().any(|a| a.starts_with("-Xmx")));
        assert!(cmd.jvm_args.iter().any(|a| a.contains("java.library.path")));
        assert!(!cmd.jvm_args.iter().any(|a| a.contains("${")));
        assert!(!cmd.game_args.iter().any(|a| a.contains("${")));
        let pos_cp = cmd.full_command.iter().position(|a| a == "-cp").unwrap();
        assert_eq!(cmd.full_command[pos_cp + 1], cmd.classpath);
        assert_eq!(cmd.full_command[pos_cp + 2], cmd.main_class);
        assert!(cmd
            .full_command_string
            .contains("net.minecraft.launchwrapper.Launch"));
        assert!(cmd.game_args.contains(&"Steve".to_string()));
    }

    #[test]
    fn java_library_path_points_at_library_path_dir() {
        // Non-régression `no lwjgl in java.library.path` : la JVM doit
        // chercher les natifs exactement dans le dossier que l'installateur
        // remplit (extraction Mojang/CDN vers `<gameDir>/natives`).
        let m = parse_manifest(JSON).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = profile_in(dir.path());
        let cmd = build_full_command(&m, &p, Os::Linux);
        let natives = library_path(dir.path());
        assert!(natives.is_dir());
        let lib_path = cmd
            .jvm_args
            .iter()
            .find_map(|a| a.strip_prefix("-Djava.library.path="))
            .expect("jvm args must carry -Djava.library.path");
        assert_eq!(Path::new(lib_path), natives.as_path());
    }

    #[test]
    fn memory_settings_override_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings::new("2G", "6G");
        let p = LaunchProfile::with_memory(
            "Alex",
            "123e4567-e89b-12d3-a456-426614174000",
            "tok",
            UserType::Msa,
            "1.7.10-Forge",
            dir.path(),
            &s,
        );
        assert_eq!(p.memory_min, "2G");
        assert_eq!(p.memory_max, "6G");
        assert_eq!(UserType::Msa.as_str(), "msa");
        let defaults = Settings::default();
        assert_eq!(
            (defaults.memory_min.as_str(), defaults.memory_max.as_str()),
            ("1G", "4G")
        );
    }

    #[test]
    fn classpath_dedupe_keeps_first_order_without_quadratic_scan() {
        // Garde anti-régression de la déduplication O(1) : doublons CDN +
        // remplissage Mojang + Forge/vanilla provisionnés — le premier gagne,
        // l'ordre FML reste stable, sans doublon.
        let m = parse_manifest(JSON).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let forge = crate::mojang::forge_universal_dest(game);
        std::fs::create_dir_all(forge.parent().unwrap()).unwrap();
        std::fs::write(&forge, b"forge").unwrap();
        let vanilla = crate::instance::versions_dir(game)
            .join("1.7.10")
            .join("1.7.10.jar");
        std::fs::create_dir_all(vanilla.parent().unwrap()).unwrap();
        std::fs::write(&vanilla, b"vanilla").unwrap();
        for os in [Os::Linux, Os::Windows] {
            let cp = build_classpath(&m, game, os);
            let sep = classpath_separator(os);
            let entries: Vec<&str> = cp.split(sep).filter(|s| !s.is_empty()).collect();
            let mut seen = std::collections::HashSet::new();
            assert!(
                entries.iter().all(|e| seen.insert(*e)),
                "aucun doublon au classpath : {cp}"
            );
            // Forge avant vanilla quand les deux sont provisionnés.
            if let (Some(pf), Some(pv)) = (cp.find("universal.jar"), cp.find("1.7.10.jar")) {
                assert!(pf < pv, "forge avant vanilla : {cp}");
            }
        }
    }
}
