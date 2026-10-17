// SPDX-License-Identifier: AGPL-3.0-only

//! Experimental LWJGL3 + Java 21 runtime (Phase 1, default OFF).
//!
//! Upstream spec is GTNewHorizons/lwjgl3ify's published `version.json`
//! (profile id `1.7.10-Forge10.13.4.1614-1.7.10-lwjgl3ify-3.0.38`): vanilla
//! 1.7.10 set with gson 2.10.1, full LWJGL **3.4.3** matrix incl.
//! per-OS/arch natives, `forgePatches`, official Forge 10.13.4.1614
//! universal), RetroFuturaBootstrap main class, and the `--add-opens`
//! wall. This module pins that profile and provisions it through the same
//! download pipeline as everything else.
//!
//! Deliberate divergences from the Java 8 path (all gated behind the
//! `experimental_runtime` setting — the default game is untouched):
//! - library set comes **only** from the upstream profile plus a pinned
//!   allowlist of CDN libraries the profile omits but pack mods require
//!   (slf4j, MongoDB, molang, mclib, guava-17.0 — see
//!   [`MODERN_CDN_LIBS`]); the rest of CDN `libraries-windows` and our
//!   Mojang regular-lib fill are skipped — mixing e.g. CDN gson-2.8.6 with
//!   upstream gson-2.10.1 would shadow the classpath;
//! - duplicate library versions resolve **newest-wins** (the profile pulls
//!   guava 15.0 for vanilla and 17.0 for Forge — 15.0 first breaks FML's
//!   AccessTransformers and the boot dies at `freezeData`);
//! - LWJGL3 natives stay as classpath jars (modern format — **no**
//!   extraction; the lwjgl2 natives extraction is skipped too);
//! - `palaforge` (Paladium's custom Forge) is never used: both flavors
//!   launch the official 1614 universal — the upstream profile pins it for
//!   experimental, Forge Maven provisions it for classic. If Paladium mods
//!   depended on palaforge-only behavior the experiment fails loudly,
//!   which is the point of gating it;
//! - Java 21 is **GraalVM CE 21** in `<gameDir>/java21/` (never clobbers
//!   `java/`), tuned G1 on top of the RFB wall;
//! - coremods (`lwjgl3ify`, UniMixins-all) land in `mods/` like Loki
//!   (size-checked — GitHub releases publish no hashes). UniMixins installs
//!   as **`Aunimixins-all-*.jar`**: first-discovered `MixinTweaker` wins,
//!   and the old Mixin copies shaded inside pack jars abort the boot
//!   otherwise (`!`/`0`-prefixed names are invisible to FML discovery);
//! - optimization mods (GTNHLib, Hodgepodge, ArchaicFix, FalseTweaks +
//!   FalsePatternLib) are a separate opt-in (`experimental_optimizations`)
//!   installed the same size-checked way; standalone EntityCulling has no
//!   1.7.10 build and is out (FalseTweaks covers culling);
//! - `-Dforge.forceDisplayStencil=true` always rides along (Zephyr-UI
//!   refuses to boot without a stencil buffer; lwjgl3ify honors it);
//! - default configs are seeded when absent (`falsetweaks.cfg` with
//!   `textureOptimizations=false`, `hodgepodge.cfg` with
//!   `speedupChunkProviderClient=false`, `computium.cfg` with
//!   validation+generation on) — the first two defaults crash this stack,
//!   the third arms the GPU chunk path.
//!
//! What this buys (honest): modern GPU/driver stability, fixed
//! input/windowing, instant texture stitching — NOT raw FPS (that would be
//! Angelica, which permanently conflicts with OptiFine and is out of
//! scope here).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::instance::{caches_dir, libraries_dir, mods_dir};
use crate::integrity::PlannedFile;
use crate::java::temurin_os_for;
use crate::manifest::ManifestFile;
use crate::mojang::MojangLibrary;
use crate::os::Os;

/// Pinned upstream profile (bump deliberately — URLs + SHAs change).
pub const MODERN_PROFILE_VERSION: &str = "3.0.38";
/// Upstream launcher metadata (full library set + JVM wall source).
pub const MODERN_PROFILE_URL: &str =
    "https://github.com/GTNewHorizons/lwjgl3ify/releases/download/3.0.38/version.json";
/// Cached upstream profile (pinned version in the name).
pub const MODERN_CACHE_FILE: &str = "modern-lwjgl3ify-3.0.38.json";
/// Installed lwjgl3ify coremod file name (the release asset renames to this
/// on install — [`plan_modern_coremods`] and [`MODERN_STACK_FILES`] agree).
pub const MODERN_LWJGL3IFY_FILE: &str = "lwjgl3ify.jar";
/// lwjgl3ify coremod (mods folder, size-checked — no published hash).
pub const MODERN_COREMOD_URL: &str =
    "https://github.com/GTNewHorizons/lwjgl3ify/releases/download/3.0.38/lwjgl3ify-3.0.38.jar";
/// UniMixins all-modules coremod (mods folder, size-checked). Installed as
/// `Aunimixins-all-*.jar` (see [`MODERN_UNIMIXINS_FILE`]).
pub const MODERN_UNIMIXINS_URL: &str =
    "https://github.com/LegacyModdingMC/UniMixins/releases/download/0.3.2/%2Bunimixins-all-1.7.10-0.3.2.jar";
/// Canonical installed file name: sorts before every Mixin-shading pack jar
/// (`fspatches`, `PalaMixins`, …) while staying visible to FML discovery.
/// The first-discovered `MixinTweaker` wins; anything else aborts the boot
/// with UniMixins' own version guard.
pub const MODERN_UNIMIXINS_FILE: &str = "Aunimixins-all-1.7.10-0.3.2.jar";
/// AnarchyCompat P0 shims (Mixin discovery guards, reflective-grab files,
/// menu fixes for the pack under lwjgl3ify). Loads last; harmless without
/// the pack mods it guards (file-driven, forces nothing) and inert unless
/// its own config enables entries.
pub const MODERN_COMPAT_URL: &str =
    "https://github.com/TaxMachine/46anarchycompat/releases/download/v0.1.0/anarchycompat-0.1.0.jar";
pub const MODERN_COMPAT_FILE: &str = "anarchycompat-0.1.0.jar";
/// Java feature release for the experimental runtime.
pub const MODERN_JAVA_MAJOR: u32 = 21;
/// Runtime dir (separate from `java/` — never clobbers the Java 8 game).
pub const MODERN_JAVA_DIR: &str = "java21";
/// GraalVM 21 binary when provisioned AND valid (`major == 21`), else
/// `None` (callers fall back to the Temurin 8 chain — a stale `java21/`
/// dir must never break launch). Pure path probe + `java -version`.
pub fn resolve_modern_java(game_dir: &Path) -> Option<PathBuf> {
    let bin = game_dir
        .join(MODERN_JAVA_DIR)
        .join("bin")
        .join(crate::java::java_exe_name(crate::os::Os::current()));
    if !bin.is_file() {
        return None;
    }
    if crate::java::check_java(&bin).major != Some(MODERN_JAVA_MAJOR) {
        return None;
    }
    Some(bin)
}
/// Java runtime key managed automatically from the pack flavor:
/// `experimental` → GraalVM 21, `default` → Temurin 8. Derived at use
/// time — the settings file keeps its `java_runtime` field for compat but
/// nothing reads it anymore (a non-empty custom `java_path` still wins).
pub fn java_runtime_for_flavor(experimental: bool) -> &'static str {
    if experimental {
        "graalvm21"
    } else {
        "temurin8"
    }
}
/// GraalVM Community 21 (Graal JIT) — pinned, bump deliberately.
pub const GRAALVM_VERSION: &str = "21.0.2";
/// GraalVM download URL for a (`std::env::consts::{OS, ARCH}`) pair.
/// Asset layout: `graalvm-community-jdk-21.0.2_<os>-<arch>_bin.(tar.gz|zip)`.
pub fn graalvm_url_for(std_os: &str, std_arch: &str) -> String {
    let (os_seg, arch_seg, ext) = match (temurin_os_for(std_os), std_arch) {
        ("windows", _) => ("windows", "x64", "zip"),
        ("mac", a) if a == "aarch64" || a == "arm64" => ("macos", "aarch64", "tar.gz"),
        ("mac", _) => ("macos", "x64", "tar.gz"),
        (_, a) if a == "aarch64" || a == "arm64" => ("linux", "aarch64", "tar.gz"),
        _ => ("linux", "x64", "tar.gz"),
    };
    format!(
        "https://github.com/graalvm/graalvm-ce-builds/releases/download/jdk-{GRAALVM_VERSION}/graalvm-community-jdk-{GRAALVM_VERSION}_{os_seg}-{arch_seg}_bin.{ext}"
    )
}

/// GC tuning appended on the experimental path. ZGC (generational,
/// product since JDK 21 — no unlock dance needed) for sub-millisecond
/// pauses on the render thread; heap itself stays settings-driven via
/// `-Xms/-Xmx`. No `MaxGCPauseMillis` (ZGC ignores it).
pub const MODERN_GC_TUNING: &[&str] = &[
    "-XX:+UseZGC",
    "-XX:+ZGenerational",
    "-XX:+AlwaysPreTouch",
    "-XX:+ParallelRefProcEnabled",
    "-XX:+DisableExplicitGC",
];

/// Zephyr-UI refuses to boot without a stencil buffer; lwjgl3ify honors
/// this flag when creating the Display. Always on for experimental
/// (production manifest ships it too).
pub const MODERN_STENCIL_FLAG: &str = "-Dforge.forceDisplayStencil=true";

/// Seed load-bearing default configs when absent (never overwrite user
/// edits). Forge merges missing keys with mod defaults on load, so minimal
/// files suffice:
/// - `falsetweaks.cfg`: `textureOptimizations=false` (collides with
///   lwjgl3ify's STB stitcher — FalseTweaks hard-errors otherwise);
/// - `hodgepodge.cfg`: `speedupChunkProviderClient=false` (its Redirect
///   assumes an ASM swap that never fires in this stack → CCE);
/// - `computium.cfg`: validation + generation on (the proven-good flags;
///   frustum culling stays off — parked upstream).
///
/// Returns human-readable lines for the Console log.
pub fn seed_modern_configs(game_dir: &Path) -> Vec<String> {
    const FALSEPATTERN: &str = "00_modules {\n    B:textureOptimizations=false\n}\n";
    const HODGEPODGE: &str = "asm {\n    B:speedupChunkProviderClient=false\n}\n";
    const COMPUTIUM: &str = "general {\n    B:enableGpuCommandValidation=true\n    B:enableGpuCommandGeneration=true\n    B:enableGpuFrustumCulling=false\n}\n";
    let mut out = Vec::new();
    for (file, content, what) in [
        (
            "falsetweaks.cfg",
            FALSEPATTERN,
            "textureOptimizations=false",
        ),
        (
            "hodgepodge.cfg",
            HODGEPODGE,
            "speedupChunkProviderClient=false",
        ),
        ("computium.cfg", COMPUTIUM, "validation+generation on"),
    ] {
        let dest = game_dir.join("config").join(file);
        if dest.is_file() {
            continue;
        }
        match (|| -> std::io::Result<()> {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, content)
        })() {
            Ok(()) => out.push(format!("Expérimental : {file} initialisé ({what}).")),
            Err(e) => out.push(format!("Expérimental : {file} non écrit ({e}).")),
        }
    }
    out
}
/// RetroFuturaBootstrap entry point (replaces the vanilla main class).
pub const MODERN_MAIN_CLASS: &str = "com.gtnewhorizons.retrofuturabootstrap.MainStartOnFirstThread";
/// RFB system classloader flag (required on the JVM command line).
pub const MODERN_CLASSLOADER_FLAG: &str =
    "-Djava.system.class.loader=com.gtnewhorizons.retrofuturabootstrap.RfbSystemClassLoader";

/// Upstream profile subset we consume (extra fields ignored).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModernProfile {
    /// Profile id (informational).
    #[serde(default)]
    pub id: String,
    /// Full library set (pinned profile version).
    #[serde(default)]
    pub libraries: Vec<MojangLibrary>,
}

/// Combined OS/arch name as used by the upstream profile rules
/// (`windows`, `windows-arm64`, `osx`, `osx-arm64`, `linux`, `linux-arm64`;
/// anything else falls back to the x64 base name).
pub fn modern_os_name(os: Os, arch: &str) -> &'static str {
    let arm64 = arch == "aarch64" || arch == "arm64";
    match (os, arm64) {
        (Os::Windows, true) => "windows-arm64",
        (Os::Windows, false) => "windows",
        (Os::Macos, true) => "osx-arm64",
        (Os::Macos, false) => "osx",
        (Os::Linux, true) => "linux-arm64",
        (Os::Linux, false) => "linux",
    }
}

/// Rule evaluation for the upstream profile: like
/// [`crate::mojang::mojang_lib_allowed`] but on the combined OS/arch name,
/// and `features`-gated entries are skipped (we add `--width/--height`
/// ourselves; demo entries are never wanted).
pub fn modern_lib_allowed(lib: &MojangLibrary, os: Os, arch: &str) -> bool {
    let Some(rules) = lib.rules.as_ref() else {
        return true;
    };
    let want = modern_os_name(os, arch);
    let mut allowed = false;
    for rule in rules {
        if rule.features.is_some() {
            continue;
        }
        let os_obj = rule.os.as_ref();
        let matches = match os_obj.and_then(|o| o.get("name")) {
            None => true,
            Some(name) => name == want,
        };
        // Arch-qualified rules (`{"name": ..., "arch": ...}`) only match
        // when the arch agrees too (x86_64 == x64 family here).
        let arch_ok = match os_obj.and_then(|o| o.get("arch")) {
            None => true,
            Some(a) => {
                (a == "x64" && !want.ends_with("arm64"))
                    || (a == "arm64" && want.ends_with("arm64"))
            }
        };
        if matches && arch_ok {
            allowed = rule.action == "allow";
        }
    }
    allowed
}

/// Plan the upstream library set (profile order preserved — forgePatches
/// already sits adjacent to Forge exactly as tested upstream).
/// `skip_names` drops anything the caller already provides (normally
/// nothing — experimental mode excludes CDN libraries wholesale).
/// Duplicate versions resolve newest-wins (see [`modern_dedupe_newest`]).
pub fn plan_modern_libraries(
    profile: &ModernProfile,
    game_dir: &Path,
    os: Os,
    arch: &str,
    skip_names: &HashSet<String>,
) -> Vec<PlannedFile> {
    let allowed: Vec<&MojangLibrary> = profile
        .libraries
        .iter()
        .filter(|lib| modern_lib_allowed(lib, os, arch))
        .collect();
    let mut out = Vec::new();
    for lib in modern_dedupe_newest(&allowed) {
        let Some(artifact) = lib.downloads.artifact.as_ref() else {
            continue;
        };
        // Upstream lwjgl3ify artifacts omit `path` (sha1/size/url only) —
        // derive the maven layout instead of writing to the libraries root.
        let Some(rel) = crate::mojang::artifact_local_path(lib, artifact) else {
            continue;
        };
        let file_name = rel.rsplit('/').next().unwrap_or(rel.as_str()).to_string();
        if skip_names.contains(&file_name) {
            continue;
        }
        out.push(PlannedFile {
            dest: libraries_dir(game_dir).join(&rel),
            file: ManifestFile {
                name: file_name,
                model: "modern".to_string(),
                url: artifact.url.clone(),
                size: artifact.size,
                sha1: artifact.sha1.clone(),
                path: rel,
                os: Vec::new(),
            },
        });
    }
    out
}

/// Classpath entries for the experimental launch: upstream profile order
/// (the order lwjgl3ify was tested with — forgePatches already sits next to
/// Forge), then the vanilla client jar last (modern launcher layout).
/// Entries point at on-disk destinations; missing files are NOT filtered
/// (the provision step guarantees them — a missing jar fails loudly at
/// spawn time instead of silently shifting every entry).
pub fn modern_classpath_entries(
    profile: &ModernProfile,
    game_dir: &Path,
    mc_version: &str,
    os: Os,
    arch: &str,
) -> Vec<String> {
    let allowed: Vec<&MojangLibrary> = profile
        .libraries
        .iter()
        .filter(|lib| modern_lib_allowed(lib, os, arch))
        .collect();
    let mut out = Vec::new();
    for lib in modern_dedupe_newest(&allowed) {
        if let Some(artifact) = lib.downloads.artifact.as_ref() {
            if let Some(rel) = crate::mojang::artifact_local_path(lib, artifact) {
                out.push(
                    libraries_dir(game_dir)
                        .join(&rel)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    out.push(
        crate::instance::versions_dir(game_dir)
            .join(mc_version)
            .join(format!("{mc_version}.jar"))
            .to_string_lossy()
            .into_owned(),
    );
    out
}

/// Classpath entries for the pinned CDN libraries on the experimental
/// path (mirrors [`crate::launch::build_classpath`] gating: OS-allowed
/// files from classpath models only). Appended after the upstream profile
/// entries by the launch assembly.
pub fn modern_cdn_classpath_entries(
    manifest: &crate::manifest::Manifest,
    game_dir: &Path,
    os: Os,
) -> Vec<String> {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String, String, String)> = HashSet::new();
    let mut out = Vec::new();
    for file in &manifest.files {
        if !modern_cdn_lib_wanted(&file.name) {
            continue;
        }
        // Même règle OS que le classique (plan + classpath : `libraries-*`
        // cross-platform, `natives-*`/`java-*` strict-OS).
        if !crate::integrity::file_allowed(manifest, file, os) {
            continue;
        }
        let Some(model) = manifest.model_by_name(&file.model) else {
            continue;
        };
        let on_classpath = model
            .properties
            .as_ref()
            .and_then(|p| p.classpath)
            .unwrap_or(false);
        if !on_classpath {
            continue;
        }
        let Ok(dest) = crate::integrity::resolve_local_path(game_dir, manifest, file) else {
            continue;
        };
        let key = crate::integrity::plan_dedupe_key(file, &dest);
        if seen.insert(key) {
            out.push(dest.to_string_lossy().into_owned());
        }
    }
    out
}

/// Coremod downloads (size-checked — GitHub releases publish no hashes).
pub fn plan_modern_coremods(game_dir: &Path) -> Vec<(String, PathBuf)> {
    vec![
        (
            MODERN_COREMOD_URL.to_string(),
            mods_dir(game_dir).join(MODERN_LWJGL3IFY_FILE),
        ),
        (
            MODERN_UNIMIXINS_URL.to_string(),
            mods_dir(game_dir).join(MODERN_UNIMIXINS_FILE),
        ),
        (
            MODERN_COMPAT_URL.to_string(),
            mods_dir(game_dir).join(MODERN_COMPAT_FILE),
        ),
    ]
}

/// Optimization stack (separate opt-in `experimental_optimizations`, same
/// size-checked treatment as coremods — GitHub releases publish no hashes).
/// Replaces OptiFine on the experimental path; standalone EntityCulling
/// has no 1.7.10 build (FalseTweaks covers culling).
pub const MODERN_OPT_GTNHLIB_URL: &str =
    "https://github.com/GTNewHorizons/GTNHLib/releases/download/0.11.53/gtnhlib-0.11.53.jar";
pub const MODERN_OPT_GTNHLIB_FILE: &str = "gtnhlib-0.11.53.jar";
pub const MODERN_OPT_HODGEPODGE_URL: &str =
    "https://github.com/GTNewHorizons/Hodgepodge/releases/download/2.7.216/hodgepodge-2.7.216.jar";
pub const MODERN_OPT_HODGEPODGE_FILE: &str = "hodgepodge-2.7.216.jar";
pub const MODERN_OPT_ARCHAICFIX_URL: &str =
    "https://github.com/embeddedt/ArchaicFix/releases/download/0.8.0/archaicfix-0.8.0.jar";
pub const MODERN_OPT_ARCHAICFIX_FILE: &str = "archaicfix-0.8.0.jar";
pub const MODERN_OPT_FALSETWEAKS_URL: &str =
    "https://github.com/FalsePattern/FalseTweaks/releases/download/4.4.5/falsetweaks-mc1.7.10-4.4.5.jar";
pub const MODERN_OPT_FALSETWEAKS_FILE: &str = "falsetweaks-mc1.7.10-4.4.5.jar";
/// Beddium (Sodium-port terrain renderer — the actual FPS unlock).
/// Pinned to the `-j21` build: `-j8` refuses modern Java, `-j25` needs
/// Java 25+. Cross-platform single jar.
pub const MODERN_OPT_BEDDIUM_URL: &str =
    "https://github.com/vfx-dev/Beddium/releases/download/1.2.3-j21/beddium-mc1.7.10-1.2.3-j21.jar";
pub const MODERN_OPT_BEDDIUM_FILE: &str = "beddium-mc1.7.10-1.2.3-j21.jar";
/// Hard FalseTweaks dependency (separate mod, same treatment).
pub const MODERN_OPT_FALSEPATTERNLIB_URL: &str =
    "https://github.com/FalsePattern/FalsePatternLib/releases/download/1.12.2/falsepatternlib-mc1.7.10-1.12.2.jar";
pub const MODERN_OPT_FALSEPATTERNLIB_FILE: &str = "falsepatternlib-mc1.7.10-1.12.2.jar";
/// Computium (GPU chunk command generation + indirect draws, Beddium
/// companion). Pinned to v0.1.0: generates on GPU, draws indirect, and
/// self-validates with automatic fallback to stock Beddium rendering.
/// Hooks Beddium's classes, so it loads last; harmless without Beddium
/// (mixins skip silently) and inert unless its config enables phases.
pub const MODERN_OPT_COMPUTIUM_URL: &str =
    "https://github.com/TaxMachine/Computium/releases/download/v0.1.0/computium-0.1.0.jar";
pub const MODERN_OPT_COMPUTIUM_FILE: &str = "computium-0.1.0.jar";

/// Optimization mod downloads: `(url, mods-dir file name)` in load-safe
/// order (GTNHLib first — Hodgepodge builds on it).
pub fn plan_modern_opt_mods() -> Vec<(String, String)> {
    vec![
        (
            MODERN_OPT_GTNHLIB_URL.to_string(),
            MODERN_OPT_GTNHLIB_FILE.to_string(),
        ),
        (
            MODERN_OPT_HODGEPODGE_URL.to_string(),
            MODERN_OPT_HODGEPODGE_FILE.to_string(),
        ),
        (
            MODERN_OPT_ARCHAICFIX_URL.to_string(),
            MODERN_OPT_ARCHAICFIX_FILE.to_string(),
        ),
        (
            MODERN_OPT_FALSEPATTERNLIB_URL.to_string(),
            MODERN_OPT_FALSEPATTERNLIB_FILE.to_string(),
        ),
        (
            MODERN_OPT_FALSETWEAKS_URL.to_string(),
            MODERN_OPT_FALSETWEAKS_FILE.to_string(),
        ),
        (
            MODERN_OPT_BEDDIUM_URL.to_string(),
            MODERN_OPT_BEDDIUM_FILE.to_string(),
        ),
        (
            MODERN_OPT_COMPUTIUM_URL.to_string(),
            MODERN_OPT_COMPUTIUM_FILE.to_string(),
        ),
    ]
}

/// Launcher-managed modern (experimental) mod stack: the EXACT file names
/// our modern plan writes into `mods/` — coremods via
/// [`plan_modern_coremods`] plus opt stack via [`plan_modern_opt_mods`].
/// Single source of truth for the classic↔experimental swap
/// ([`stash_modern_stack`] / [`restore_modern_stack`]): never confuse these
/// with the CDN-manifest set (`manifest.files` — CDN-managed, untouched by
/// either swap direction).
pub const MODERN_STACK_FILES: &[&str] = &[
    MODERN_LWJGL3IFY_FILE,
    MODERN_UNIMIXINS_FILE,
    MODERN_COMPAT_FILE,
    MODERN_OPT_GTNHLIB_FILE,
    MODERN_OPT_HODGEPODGE_FILE,
    MODERN_OPT_ARCHAICFIX_FILE,
    MODERN_OPT_FALSEPATTERNLIB_FILE,
    MODERN_OPT_FALSETWEAKS_FILE,
    MODERN_OPT_BEDDIUM_FILE,
    MODERN_OPT_COMPUTIUM_FILE,
];

/// Launcher-managed stash dir name: a sibling OUTSIDE `mods/` (FML only
/// scans `mods/`, so stashed jars never load and the install plan never
/// re-downloads them — bytes are preserved, not fetched again).
pub const MODERN_STASH_DIR_NAME: &str = "modstack-experimental";

/// `<gameDir>/modstack-experimental` (created on demand by the swap).
pub fn modern_stash_dir(game_dir: &Path) -> PathBuf {
    game_dir.join(MODERN_STASH_DIR_NAME)
}

/// Legacy UniMixins installs renamed at install time (`Aunimixins-…` sorts
/// first so its `MixinTweaker` wins discovery): match by infix so a renamed
/// copy never escapes the stash. Case-insensitive.
fn is_legacy_unimixins(name: &str) -> bool {
    name.to_lowercase().contains("unimixins")
}

/// Move one file without ever overwriting: `false` when the source is
/// missing or the destination already exists (both copies then stay —
/// the `mods/` copy wins for the boot, the stash copy waits; documented,
/// never merged or deleted). Falls back to copy+remove when `rename`
/// crosses devices.
pub(crate) fn move_without_overwrite(src: &Path, dst: &Path) -> bool {
    if !src.is_file() || dst.exists() {
        return false;
    }
    if let Some(parent) = dst.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if std::fs::rename(src, dst).is_ok() {
        return true;
    }
    std::fs::copy(src, dst).is_ok() && std::fs::remove_file(src).is_ok()
}

/// Classic-activate: MOVE every present modern-stack file (plus its
/// `.disabled` twin when the user toggled it off) from `mods/` to the
/// launcher-managed stash. Idempotent; returns the moved file names,
/// sorted. Manifest/CDN files are never touched — only [`MODERN_STACK_FILES`]
/// (and legacy `*unimixins*` renames) move.
pub fn stash_modern_stack(game_dir: &Path) -> Vec<String> {
    let mods = mods_dir(game_dir);
    let stash = modern_stash_dir(game_dir);
    let mut moved = Vec::new();
    let mut candidates: Vec<String> = MODERN_STACK_FILES.iter().map(|s| s.to_string()).collect();
    // Legacy renames never escape: anything unimixins-shaped stashes too.
    if let Ok(entries) = std::fs::read_dir(&mods) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let base = name.strip_suffix(".disabled").unwrap_or(name);
            if !base.ends_with(".jar") {
                continue;
            }
            if is_legacy_unimixins(base) && !candidates.iter().any(|c| c == base || c == name) {
                candidates.push(base.to_string());
            }
        }
    }
    for base in &candidates {
        for variant in [base.clone(), format!("{base}.disabled")] {
            if move_without_overwrite(&mods.join(&variant), &stash.join(&variant)) {
                moved.push(variant);
            }
        }
    }
    moved.sort();
    moved
}

/// Experimental-activate: move stashed files BACK to `mods/` (only what is
/// still missing there is re-downloaded afterwards — pre-seeded bytes are
/// reused, never fetched again). Symmetric with [`stash_modern_stack`]:
/// known stack files (plus `.disabled` twins and legacy `*unimixins*`
/// renames); when a `mods/` copy already exists the stash copy stays put
/// (both kept out of harm — the `mods/` copy wins for the boot).
/// Idempotent; returns the restored file names, sorted.
pub fn restore_modern_stack(game_dir: &Path) -> Vec<String> {
    let mods = mods_dir(game_dir);
    let stash = modern_stash_dir(game_dir);
    let mut candidates: Vec<String> = MODERN_STACK_FILES.iter().map(|s| s.to_string()).collect();
    // Symmetry with the stash sweep: legacy renames stashed under another
    // name come back too (they could only have been placed by our stash).
    if let Ok(entries) = std::fs::read_dir(&stash) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let base = name.strip_suffix(".disabled").unwrap_or(name);
            if !base.ends_with(".jar") {
                continue;
            }
            if is_legacy_unimixins(base) && !candidates.iter().any(|c| c == base || c == name) {
                candidates.push(base.to_string());
            }
        }
    }
    let mut restored = Vec::new();
    for base in &candidates {
        for variant in [base.clone(), format!("{base}.disabled")] {
            if move_without_overwrite(&stash.join(&variant), &mods.join(&variant)) {
                restored.push(variant);
            }
        }
    }
    restored.sort();
    restored
}

/// Extra process environment for the experimental game child.
///
/// Currently EMPTY on all platforms, deliberately: `mesa_glthread=true`
/// was tested and REMOVED — under heavy multidraw batches (Beddium at
/// 2000fps+) Mesa's threaded-context batching overflows the amdgpu
/// command ring (`amdgpu_pad_ib` → SIGABRT in a Gallium worker thread,
/// proven by coredump forensics). Stability beats the pacing win; frame
/// pacing belongs to vsync/fps-cap, not the driver threader.
/// Official Windows drivers thread internally and macOS has no
/// equivalent — nothing to set there either.
pub fn modern_env_vars(_os: Os, _experimental: bool) -> Vec<(String, String)> {
    Vec::new()
}

/// CDN libraries the upstream profile omits but pack mods require (exact
/// file names as shipped by `https://cdn.46anarchy.fr/manifest.json` —
/// production's own set, SHAs included there). Kept on the experimental
/// path while every other CDN library stays excluded (see
/// `cdn_plan_for_mode`).
pub const MODERN_CDN_LIBS: &[&str] = &[
    "slf4j-api-1.7.12.jar",
    "slf4j-simple-1.6.2.jar",
    "bson-4.5.0.jar",
    "mongodb-driver-core-4.5.0.jar",
    "mongodb-driver-sync-4.5.0.jar",
    "molang-18.jar",
    "mclib-18.jar",
    // Newest-wins single Guava: 15.0 (vanilla recipe) breaks FML's
    // AccessTransformers; 20.0 (CDN) is untested on this path.
    "guava-17.0.jar",
];

/// `true` when a CDN file belongs on the experimental classpath.
pub fn modern_cdn_lib_wanted(file_name: &str) -> bool {
    MODERN_CDN_LIBS.contains(&file_name)
}

/// Split an artifact path (`group/dirs/artifact/version/file.jar`) into a
/// dedupe key `(group/artifact, version)`. Returns `None` when the shape is
/// unexpected (never deduped away).
fn modern_artifact_key(path: &str) -> Option<(String, Vec<u64>)> {
    let mut parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 {
        return None;
    }
    let file = parts.pop()?;
    let version = parts.pop()?;
    let artifact = parts.pop()?;
    // Key is group + artifact + classifier (sibling artifacts like `lwjgl`
    // vs `lwjgl-glfw`, and plain vs `natives-linux` variants of the SAME
    // artifact, must never collide — only identical variants dedupe).
    let mut key = parts.join("/");
    key.push('/');
    key.push_str(artifact);
    let stem = file.strip_suffix(".jar").unwrap_or(file);
    let prefix = format!("{artifact}-{version}");
    let classifier = stem.strip_prefix(prefix.as_str()).unwrap_or("");
    key.push_str(classifier);
    let nums = version
        .split(['.', '-', '_'])
        .filter_map(|p| p.parse::<u64>().ok())
        .collect::<Vec<_>>();
    Some((key, nums))
}

/// Newest-wins dedupe for one group/artifact across a library list
/// (MultiMC/Gradle conflict resolution). The upstream profile pulls guava
/// 15.0 (vanilla set) and 17.0 (Forge set); 15.0 first on the classpath
/// breaks FML's AccessTransformers (`readLines(LineProcessor)` missing)
/// and the boot dies at `freezeData` — 17.0 must win.
pub fn modern_dedupe_newest<'a>(libs: &[&'a MojangLibrary]) -> Vec<&'a MojangLibrary> {
    use std::collections::HashMap;
    let mut best: HashMap<String, (Vec<u64>, usize, &'a MojangLibrary)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for lib in libs {
        let Some(artifact) = lib.downloads.artifact.as_ref() else {
            continue;
        };
        // Effective layout path (derived when the profile omits `path`):
        // without this, path-less entries (e.g. upstream guava 17.0) never
        // dedupe and both guavas land on the classpath, breaking FML.
        let effective = crate::mojang::artifact_local_path(lib, artifact).unwrap_or_default();
        let probe = if effective.is_empty() {
            artifact.path.clone()
        } else {
            effective
        };
        let Some((key, ver)) = modern_artifact_key(&probe) else {
            // Unparseable shape: keep unconditionally (never drop blindly).
            // Keyed by coordinate + URL so distinct entries never collide.
            let k = format!("raw:{}:{}", lib.name, artifact.url);
            if best.insert(k.clone(), (vec![], libs.len(), lib)).is_none() {
                order.push(k);
            }
            continue;
        };
        match best.get(&key) {
            Some((old_ver, _, _)) if old_ver >= &ver => {}
            _ => {
                if !best.contains_key(&key) {
                    order.push(key.clone());
                }
                best.insert(key, (ver, libs.len(), lib));
            }
        }
    }
    // Profile order preserved (first-seen position of each winner).
    order
        .into_iter()
        .filter_map(|k| best.get(&k).map(|(_, _, lib)| *lib))
        .collect()
}

/// Cached upstream profile path.
pub fn modern_cache_path(game_dir: &Path) -> PathBuf {
    caches_dir(game_dir).join(MODERN_CACHE_FILE)
}

/// Load the cached upstream profile, or fetch + cache it.
pub async fn load_or_fetch_modern_profile(
    client: &reqwest::Client,
    game_dir: &Path,
) -> Result<ModernProfile> {
    let path = modern_cache_path(game_dir);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(cached) = serde_json::from_slice::<ModernProfile>(&bytes) {
            if !cached.libraries.is_empty() {
                return Ok(cached);
            }
        }
    }
    let profile: ModernProfile = client
        .get(MODERN_PROFILE_URL)
        .send()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Profil LWJGL3 : {e}")))?
        .error_for_status()
        .map_err(|e| CoreError::CdnFailure(format!("Profil LWJGL3 : {e}")))?
        .json()
        .await
        .map_err(|e| CoreError::CdnFailure(format!("Profil LWJGL3 : {e}")))?;
    if profile.libraries.is_empty() {
        return Err(CoreError::CdnFailure(
            "Profil LWJGL3 : aucune bibliothèque.".to_string(),
        ));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&profile) {
        let _ = std::fs::write(&path, text);
    }
    Ok(profile)
}

/// Read the cached upstream profile without network (verify path).
pub fn load_cached_modern_profile(game_dir: &Path) -> Option<ModernProfile> {
    std::fs::read(modern_cache_path(game_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|p: &ModernProfile| !p.libraries.is_empty())
}

/// `true` when the experimental runtime needs (re)provisioning: no cached
/// profile, no Java 21 binary, no forgePatches jar, or a coremod missing.
pub fn modern_runtime_stale(game_dir: &Path) -> bool {
    if load_cached_modern_profile(game_dir).is_none() {
        return true;
    }
    let java21 = game_dir
        .join(MODERN_JAVA_DIR)
        .join("bin")
        .join(crate::java::java_exe_name(Os::current()));
    if !java21.is_file() {
        return true;
    }
    let libs = libraries_dir(game_dir);
    let mut has_forge_patches = false;
    let mut stack = vec![libs];
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
                .is_some_and(|n| n.contains("forgePatches") && n.ends_with(".jar"))
            {
                has_forge_patches = true;
            }
        }
    }
    if !has_forge_patches {
        return true;
    }
    let mods = mods_dir(game_dir);
    if !mods.join("lwjgl3ify.jar").is_file() || !mods.join(MODERN_UNIMIXINS_FILE).is_file() {
        return true;
    }
    false
}

/// `true` when any optimization-stack mod is missing from `mods/`.
/// Separate from [`modern_runtime_stale`] so the base runtime never
/// redownloads just because the opt-in is off.
pub fn modern_opt_mods_stale(game_dir: &Path) -> bool {
    let mods = mods_dir(game_dir);
    plan_modern_opt_mods()
        .iter()
        .any(|(_, file)| !mods.join(file).is_file())
}

/// JVM wall for Java 21 (transcribed from the upstream `version.json` order:
/// file encoding, RFB classloader, native access, then the `--add-opens`
/// pairs). OS extras (macOS `-XstartOnFirstThread`, Windows heap-dump path,
/// x86 `-Xss1M`) are appended by [`modern_jvm_extra_args`].
pub const MODERN_JVM_WALL: &[&str] = &[
    "-Dfile.encoding=UTF-8",
    MODERN_CLASSLOADER_FLAG,
    "--enable-native-access",
    "ALL-UNNAMED",
    "--add-opens",
    "java.base/java.io=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.lang.invoke=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.lang.ref=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.lang.reflect=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.lang=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.net.spi=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.net=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.nio.channels=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.nio.charset=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.nio.file=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.nio=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.text=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.time.chrono=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.time.format=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.time.temporal=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.time.zone=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.time=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.util.concurrent.atomic=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.util.concurrent.locks=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.util.jar=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.util.zip=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.util=ALL-UNNAMED",
    "--add-opens",
    "java.base/jdk.internal.loader=ALL-UNNAMED",
    "--add-opens",
    "java.base/jdk.internal.misc=ALL-UNNAMED",
    "--add-opens",
    "java.base/jdk.internal.ref=ALL-UNNAMED",
    "--add-opens",
    "java.base/jdk.internal.reflect=ALL-UNNAMED",
    "--add-opens",
    "java.base/sun.nio.ch=ALL-UNNAMED",
    "--add-opens",
    "java.desktop/com.sun.imageio.plugins.png=ALL-UNNAMED",
    "--add-opens",
    "java.desktop/sun.awt.image=ALL-UNNAMED",
    "--add-opens",
    "java.desktop/sun.awt=ALL-UNNAMED",
    "--add-opens",
    "java.desktop/sun.lwawt.macosx=ALL-UNNAMED",
    "--add-opens",
    "java.sql.rowset/javax.sql.rowset.serial=ALL-UNNAMED",
    "--add-opens",
    "jdk.dynalink/jdk.dynalink.beans=ALL-UNNAMED",
    "--add-opens",
    "jdk.naming.dns/com.sun.jndi.dns=ALL-UNNAMED,java.naming",
];

/// OS-specific JVM extras from the upstream profile (`-XstartOnFirstThread`
/// is mandatory on macOS; the rest is cosmetic but faithful).
pub fn modern_jvm_extra_args(os: Os, arch: &str) -> Vec<String> {
    let mut out = Vec::new();
    if os == Os::Macos {
        out.push("-XstartOnFirstThread".to_string());
    }
    if os == Os::Windows {
        out.push("-XX:HeapDumpPath=MojangTricksIntelDriversForPerformance_javaw.exe_minecraft.exe.heapdump".to_string());
    }
    if !arch.starts_with("aarch64") && !arch.starts_with("arm") {
        out.push("-Xss1M".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mojang::MojangLibrary;

    fn lib(name: &str, rules: Option<serde_json::Value>) -> MojangLibrary {
        let mut v = serde_json::json!({"name": name});
        if let Some(r) = rules {
            v["rules"] = r;
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn java_runtime_follows_flavor() {
        assert_eq!(java_runtime_for_flavor(true), "graalvm21");
        assert_eq!(java_runtime_for_flavor(false), "temurin8");
    }

    #[test]
    fn modern_os_names_cover_arches() {
        assert_eq!(modern_os_name(Os::Linux, "x86_64"), "linux");
        assert_eq!(modern_os_name(Os::Linux, "aarch64"), "linux-arm64");
        assert_eq!(modern_os_name(Os::Macos, "aarch64"), "osx-arm64");
        assert_eq!(modern_os_name(Os::Macos, "x86_64"), "osx");
        assert_eq!(modern_os_name(Os::Windows, "x86_64"), "windows");
        assert_eq!(modern_os_name(Os::Windows, "aarch64"), "windows-arm64");
    }

    #[test]
    fn modern_java_resolves_only_valid_provisioned() {
        // Absent dir → None (callers fall back to Temurin 8).
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_modern_java(dir.path()).is_none());
        // Present-but-bogus binary (unparseable `java -version`) → None.
        // No real JVM is executed beyond a failing probe.
        let bin_dir = dir.path().join(MODERN_JAVA_DIR).join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let fake = bin_dir.join(crate::java::java_exe_name(crate::os::Os::current()));
        std::fs::write(&fake, b"not a java binary").unwrap();
        assert!(resolve_modern_java(dir.path()).is_none());
    }

    #[test]
    fn modern_rules_respect_arch_and_skip_features() {
        let allow_linux = serde_json::json!([{"action": "allow", "os": {"name": "linux"}}]);
        let allow_arm = serde_json::json!([{"action": "allow", "os": {"name": "linux-arm64"}}]);
        let allow_win = serde_json::json!([{"action": "allow", "os": {"name": "windows"}}]);
        let demo = serde_json::json!([{"action": "allow", "features": {"is_demo_user": true}}]);
        assert!(modern_lib_allowed(
            &lib("x", Some(allow_linux.clone())),
            Os::Linux,
            "x86_64"
        ));
        assert!(!modern_lib_allowed(
            &lib("x", Some(allow_linux)),
            Os::Linux,
            "aarch64"
        ));
        assert!(modern_lib_allowed(
            &lib("x", Some(allow_arm)),
            Os::Linux,
            "aarch64"
        ));
        assert!(!modern_lib_allowed(
            &lib("x", Some(allow_win)),
            Os::Linux,
            "x86_64"
        ));
        assert!(!modern_lib_allowed(
            &lib("x", Some(demo)),
            Os::Linux,
            "x86_64"
        ));
        assert!(modern_lib_allowed(
            &lib("plain", None),
            Os::Macos,
            "aarch64"
        ));
    }

    #[test]
    fn modern_plan_orders_and_filters() {
        let profile: ModernProfile = serde_json::from_value(serde_json::json!({
            "id": "test",
            "libraries": [
                {"name": "a:one:1",
                 "downloads": {"artifact": {"path": "a/one/1/one-1.jar",
                     "sha1": "a", "size": 1, "url": "https://example.com/1"}}},
                {"name": "b:win:1",
                 "rules": [{"action": "allow", "os": {"name": "windows"}}],
                 "downloads": {"artifact": {"path": "b/win/1/win-1.jar",
                     "sha1": "b", "size": 2, "url": "https://example.com/2"}}},
                {"name": "c:skip:1",
                 "downloads": {"artifact": {"path": "c/skip/1/skip-1.jar",
                     "sha1": "c", "size": 3, "url": "https://example.com/3"}}}
            ]
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let skip: HashSet<String> = ["skip-1.jar".to_string()].into_iter().collect();
        let planned = plan_modern_libraries(&profile, dir.path(), Os::Linux, "x86_64", &skip);
        let names: Vec<_> = planned.iter().map(|p| p.file.name.as_str()).collect();
        assert_eq!(names, vec!["one-1.jar"]);
        assert!(planned[0].dest.ends_with("libraries/a/one/1/one-1.jar"));
    }

    #[test]
    fn modern_stale_without_cache_or_bins() {
        let dir = tempfile::tempdir().unwrap();
        assert!(modern_runtime_stale(dir.path()));
    }

    /// Live upstream shape (lwjgl3ify 3.0.38): artifacts WITHOUT `path`
    /// (sha1/size/url only). The profile must parse and plan to the derived
    /// maven layout — previously this failed with `missing field 'path'`
    /// and the experimental install died before downloading anything.
    #[test]
    fn modern_profile_parses_without_artifact_paths() {
        let profile: ModernProfile = serde_json::from_value(serde_json::json!({
            "id": "1.7.10-Forge10.13.4.1614-1.7.10-lwjgl3ify-3.0.38",
            "libraries": [
                {"name": "com.google.guava:guava:17.0",
                 "downloads": {"artifact": {"sha1": "9c6ef172e8de35fd8d4d8783e4821e57cdef7445",
                     "size": 2243036,
                     "url": "https://libraries.minecraft.net/com/google/guava/guava/17.0/guava-17.0.jar"}}},
                {"name": "org.lwjgl:lwjgl:3.4.3",
                 "downloads": {"artifact": {"sha1": "s", "size": 786332,
                     "url": "https://repo1.maven.org/maven2/org/lwjgl/lwjgl/3.4.3/lwjgl-3.4.3.jar"}}},
                {"name": "net.minecraftforge:forge:1.7.10-10.13.4.1614-1.7.10:universal",
                 "downloads": {"artifact": {"sha1": "25fd97f72beca728112256938e03e8105b1b78cc",
                     "size": 3018833,
                     "url": "https://maven.minecraftforge.net/net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10-universal.jar"}}}
            ]
        }))
        .expect("path-less upstream profile must parse");
        let dir = tempfile::tempdir().unwrap();
        let planned =
            plan_modern_libraries(&profile, dir.path(), Os::Linux, "x86_64", &HashSet::new());
        assert_eq!(planned.len(), 3, "all path-less entries must plan");
        let dests: Vec<String> = planned
            .iter()
            .map(|p| {
                p.dest
                    .strip_prefix(dir.path().join("libraries"))
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(
            dests.contains(&"com/google/guava/guava/17.0/guava-17.0.jar".to_string()),
            "guava derives maven layout, got {dests:?}"
        );
        assert!(
            dests.contains(&"org/lwjgl/lwjgl/3.4.3/lwjgl-3.4.3.jar".to_string()),
            "got {dests:?}"
        );
        // The official Forge universal must land on disk (the experimental
        // filter drops the CDN palaforge/forge jars for it).
        assert!(
            dests
                .iter()
                .any(|d| d.contains("forge-1.7.10-10.13.4.1614-1.7.10-universal.jar")),
            "got {dests:?}"
        );
        // Classpath mirrors the plan (no libraries-root entries).
        let cp = modern_classpath_entries(&profile, dir.path(), "1.7.10", Os::Linux, "x86_64");
        assert!(cp.iter().any(|e| e.ends_with("guava/17.0/guava-17.0.jar")));
        assert!(cp.iter().any(|e| e.ends_with("1.7.10.jar")));
    }

    /// Guava 15.0 (vanilla, WITH path) vs 17.0 (Forge set, WITHOUT path):
    /// newest-wins must still collapse them, or 15.0 first on the classpath
    /// breaks FML's AccessTransformers at `freezeData`.
    #[test]
    fn modern_dedupe_newest_prefers_pathless_newer_guava() {
        let g15: MojangLibrary = serde_json::from_value(serde_json::json!({
            "name": "com.google.guava:guava:15.0",
            "downloads": {"artifact": {"path": "com/google/guava/guava/15.0/guava-15.0.jar",
                "sha1": "s", "size": 1, "url": "https://example.com/guava-15.0.jar"}}
        }))
        .unwrap();
        let g17: MojangLibrary = serde_json::from_value(serde_json::json!({
            "name": "com.google.guava:guava:17.0",
            "downloads": {"artifact": {"sha1": "s", "size": 2,
                "url": "https://example.com/guava-17.0.jar"}}
        }))
        .unwrap();
        let in_order = [&g15, &g17];
        let out = modern_dedupe_newest(&in_order);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "com.google.guava:guava:17.0");
        // And the plan writes the derived layout, not the libraries root.
        let profile = ModernProfile {
            id: "test".to_string(),
            libraries: vec![g15, g17],
        };
        let dir = tempfile::tempdir().unwrap();
        let planned =
            plan_modern_libraries(&profile, dir.path(), Os::Linux, "x86_64", &HashSet::new());
        assert_eq!(planned.len(), 1);
        assert!(
            planned[0]
                .dest
                .ends_with("libraries/com/google/guava/guava/17.0/guava-17.0.jar"),
            "unexpected dest: {}",
            planned[0].dest.display()
        );
    }

    #[test]
    fn coremod_plan_targets_mods() {
        let dir = tempfile::tempdir().unwrap();
        let mods = plan_modern_coremods(dir.path());
        assert_eq!(mods.len(), 3);
        assert!(mods[0].1.ends_with("mods/lwjgl3ify.jar"));
        // UniMixins sorts first so its MixinTweaker wins discovery.
        assert!(mods[1].1.ends_with("mods/Aunimixins-all-1.7.10-0.3.2.jar"));
        assert!(mods[2].1.ends_with("mods/anarchycompat-0.1.0.jar"));
        assert!(mods.iter().all(|(u, _)| u.starts_with("https://")));
    }

    #[test]
    fn opt_mods_plan_lists_seven_size_checked_jars() {
        let mods = plan_modern_opt_mods();
        let names: Vec<_> = mods.iter().map(|(_, f)| f.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "gtnhlib-0.11.53.jar",
                "hodgepodge-2.7.216.jar",
                "archaicfix-0.8.0.jar",
                "falsepatternlib-mc1.7.10-1.12.2.jar",
                "falsetweaks-mc1.7.10-4.4.5.jar",
                "beddium-mc1.7.10-1.2.3-j21.jar",
                "computium-0.1.0.jar",
            ]
        );
        assert!(mods.iter().all(|(u, _)| u.starts_with("https://")));
    }

    #[test]
    fn modern_stack_files_match_plans_without_drift() {
        // The swap owns exactly what the plans write: every planned file
        // lands in the constant, every constant entry is planned. A version
        // bump touches the FILE constants — this test fails loudly if the
        // swap list is forgotten.
        let dir = tempfile::tempdir().unwrap();
        let mut planned: Vec<String> = plan_modern_coremods(dir.path())
            .into_iter()
            .map(|(_, dest)| dest.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        planned.extend(plan_modern_opt_mods().into_iter().map(|(_, f)| f));
        assert_eq!(
            planned.len(),
            10,
            "3 coremods + 7 opt-mods, got {planned:?}"
        );
        let mut planned_sorted = planned.clone();
        planned_sorted.sort();
        let mut stack_sorted: Vec<String> =
            MODERN_STACK_FILES.iter().map(|s| s.to_string()).collect();
        stack_sorted.sort();
        assert_eq!(planned_sorted, stack_sorted);
        // No duplicates in the owned list.
        assert_eq!(MODERN_STACK_FILES.len(), {
            let mut seen = std::collections::HashSet::new();
            MODERN_STACK_FILES
                .iter()
                .filter(|s| seen.insert(*s))
                .count()
        });
    }

    #[test]
    fn env_vars_empty_everywhere_after_glthread_removal() {
        // mesa_glthread caused amdgpu CS-ring aborts under heavy multidraw
        // (coredump-proven) — nothing is set anywhere for now.
        use crate::os::Os;
        assert!(modern_env_vars(Os::Linux, true).is_empty());
        assert!(modern_env_vars(Os::Linux, false).is_empty());
        assert!(modern_env_vars(Os::Windows, true).is_empty());
        assert!(modern_env_vars(Os::Macos, true).is_empty());
    }

    #[test]
    fn cdn_allowlist_pins_production_libs() {
        for name in [
            "slf4j-api-1.7.12.jar",
            "slf4j-simple-1.6.2.jar",
            "bson-4.5.0.jar",
            "mongodb-driver-core-4.5.0.jar",
            "mongodb-driver-sync-4.5.0.jar",
            "molang-18.jar",
            "mclib-18.jar",
            "guava-17.0.jar",
        ] {
            assert!(
                modern_cdn_lib_wanted(name),
                "{name} must stay on experimental"
            );
        }
        // Untested / shadowing versions stay out.
        assert!(!modern_cdn_lib_wanted("guava-20.0.jar"));
        assert!(!modern_cdn_lib_wanted("guava-15.0.jar"));
        assert!(!modern_cdn_lib_wanted("gson-2.8.6.jar"));
    }

    fn mock_lib(path: &str) -> MojangLibrary {
        serde_json::from_value(serde_json::json!({
            "name": "x",
            "downloads": {"artifact": {"path": path, "sha1": "s", "size": 1,
                "url": "https://example.com/f"}},
        }))
        .unwrap()
    }

    #[test]
    fn dedupe_newest_wins_but_keeps_variants() {
        let g15 = mock_lib("com/google/guava/guava/15.0/guava-15.0.jar");
        let g17 = mock_lib("com/google/guava/guava/17.0/guava-17.0.jar");
        let lw = mock_lib("org/lwjgl/lwjgl/3.4.3/lwjgl-3.4.3.jar");
        let lw_nat = mock_lib("org/lwjgl/lwjgl/3.4.3/lwjgl-3.4.3-natives-linux.jar");
        let glfw = mock_lib("org/lwjgl/lwjgl-glfw/3.4.3/lwjgl-glfw-3.4.3.jar");
        let in_order = [&g15, &g17, &lw, &lw_nat, &glfw];
        let out = modern_dedupe_newest(&in_order);
        let paths: Vec<_> = out
            .iter()
            .map(|l| l.downloads.artifact.as_ref().unwrap().path.as_str())
            .collect();
        // 17.0 wins over 15.0; natives + sibling artifacts untouched.
        assert_eq!(
            paths,
            vec![
                "com/google/guava/guava/17.0/guava-17.0.jar",
                "org/lwjgl/lwjgl/3.4.3/lwjgl-3.4.3.jar",
                "org/lwjgl/lwjgl/3.4.3/lwjgl-3.4.3-natives-linux.jar",
                "org/lwjgl/lwjgl-glfw/3.4.3/lwjgl-glfw-3.4.3.jar",
            ]
        );
    }

    #[test]
    fn seed_configs_writes_minimal_files_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = seed_modern_configs(dir.path());
        assert_eq!(first.len(), 3);
        let ft = std::fs::read_to_string(dir.path().join("config/falsetweaks.cfg")).unwrap();
        assert!(ft.contains("textureOptimizations=false"));
        let hp = std::fs::read_to_string(dir.path().join("config/hodgepodge.cfg")).unwrap();
        assert!(hp.contains("speedupChunkProviderClient=false"));
        let cp = std::fs::read_to_string(dir.path().join("config/computium.cfg")).unwrap();
        assert!(cp.contains("enableGpuCommandGeneration=true"));
        assert!(cp.contains("enableGpuFrustumCulling=false"));
        // Second run: user edits preserved (nothing rewritten).
        std::fs::write(dir.path().join("config/falsetweaks.cfg"), "custom").unwrap();
        let second = seed_modern_configs(dir.path());
        assert_eq!(second.len(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config/falsetweaks.cfg")).unwrap(),
            "custom"
        );
    }

    #[test]
    fn graalvm_urls_cover_desktop_targets() {
        assert_eq!(
            graalvm_url_for("linux", "x86_64"),
            "https://github.com/graalvm/graalvm-ce-builds/releases/download/jdk-21.0.2/graalvm-community-jdk-21.0.2_linux-x64_bin.tar.gz"
        );
        assert_eq!(
            graalvm_url_for("windows", "x86_64"),
            "https://github.com/graalvm/graalvm-ce-builds/releases/download/jdk-21.0.2/graalvm-community-jdk-21.0.2_windows-x64_bin.zip"
        );
        assert_eq!(
            graalvm_url_for("macos", "aarch64"),
            "https://github.com/graalvm/graalvm-ce-builds/releases/download/jdk-21.0.2/graalvm-community-jdk-21.0.2_macos-aarch64_bin.tar.gz"
        );
        assert_eq!(
            graalvm_url_for("macos", "x86_64"),
            "https://github.com/graalvm/graalvm-ce-builds/releases/download/jdk-21.0.2/graalvm-community-jdk-21.0.2_macos-x64_bin.tar.gz"
        );
    }

    #[test]
    fn gc_tuning_is_zgc_without_unlock_dance() {
        // ZGC is product since JDK 21: no UnlockExperimentalVMOptions
        // needed (and none of G1's flags, which ZGC would ignore).
        assert!(MODERN_GC_TUNING.contains(&"-XX:+UseZGC"));
        assert!(MODERN_GC_TUNING.contains(&"-XX:+ZGenerational"));
        assert!(!MODERN_GC_TUNING
            .iter()
            .any(|a| a.contains("G1") || a.contains("UnlockExperimental")));
        assert_eq!(MODERN_STENCIL_FLAG, "-Dforge.forceDisplayStencil=true");
    }

    #[test]
    fn jvm_wall_has_classloader_and_final_opens() {
        assert!(MODERN_JVM_WALL.contains(&MODERN_CLASSLOADER_FLAG));
        assert!(MODERN_JVM_WALL.contains(&"-Dfile.encoding=UTF-8"));
        assert_eq!(
            MODERN_JVM_WALL.last(),
            Some(&"jdk.naming.dns/com.sun.jndi.dns=ALL-UNNAMED,java.naming")
        );
        let extras = modern_jvm_extra_args(Os::Macos, "aarch64");
        assert!(extras.contains(&"-XstartOnFirstThread".to_string()));
        assert!(!modern_jvm_extra_args(Os::Linux, "x86_64")
            .contains(&"-XstartOnFirstThread".to_string()));
    }
}
