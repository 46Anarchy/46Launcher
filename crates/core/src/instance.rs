// SPDX-License-Identifier: AGPL-3.0-only

//! Instance layout: game-dir resolution, well-known subfolders, and the
//! `cdn_last.json` vs fresh-manifest freshness state machine.
//!
//! ```text
//! <gameDir>/            # <base>/.46anarchy/paladium (see below)
//!   cdn_last.json      # last successfully applied manifest (version compare)
//!   mods/ libraries/ natives/ versions/ assets/ java/ config/ ...
//! ```
//!
//! The instance root is `<base>/.46anarchy/paladium`: the CDN manifest's
//! `root` is `"paladium"`, and PalaMod deliberately segfaults (`Unsafe`
//! tripwire in `PlayerUtil.fix()`) unless the gameDir path contains
//! `"paladium"` (case-sensitive) — `~/.46anarchy` alone does NOT qualify.
//! Pre-`paladium`-subdir installs are treated as fresh (full redownload).
//!
//! Freshness states: `INIT` (no `cdn_last.json` yet), `OK` (versions match),
//! `OUT_OF_DATE` (CDN moved), `CDN_FAILURE` (no fresh manifest to compare).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::manifest::Manifest;

/// Folder name used on every OS (`~/.46anarchy` / `%APPDATA%/.46anarchy`).
pub const GAME_DIR_NAME: &str = ".46anarchy";
/// Instance subfolder (CDN `root`, PalaMod tripwire requirement).
pub const GAME_ROOT_NAME: &str = "paladium";

/// Resolve the instance root (`<base>/.46anarchy/paladium`):
/// - Windows → `%APPDATA%/.46anarchy/paladium`;
/// - macOS/Linux → `$HOME/.46anarchy/paladium`.
///
/// The trailing `paladium` is load-bearing, not cosmetic (see module docs).
///
/// Prefers the `directories` crate, falling back to raw env vars so the
/// launcher still works in minimal containers.
pub fn default_game_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(base) = directories::BaseDirs::new() {
            return base.data_dir().join(GAME_DIR_NAME).join(GAME_ROOT_NAME);
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata)
                .join(GAME_DIR_NAME)
                .join(GAME_ROOT_NAME);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(base) = directories::BaseDirs::new() {
            return base.home_dir().join(GAME_DIR_NAME).join(GAME_ROOT_NAME);
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(GAME_DIR_NAME).join(GAME_ROOT_NAME);
        }
    }
    PathBuf::from(GAME_DIR_NAME).join(GAME_ROOT_NAME)
}

/// Ensure an explicit game dir also satisfies the tripwire: append
/// `paladium` unless the path already ends with it (case-sensitive — the
/// mod checks `contains("paladium")`, and a user dir like
/// `D:\Games\PaladiumPack` already qualifies by containment, so only the
/// exact trailing segment is normalized here).
pub fn normalize_game_dir(dir: &Path) -> PathBuf {
    let qualifies = dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == GAME_ROOT_NAME);
    if qualifies {
        dir.to_path_buf()
    } else {
        dir.join(GAME_ROOT_NAME)
    }
}

/// `<gameDir>/cdn_last.json`: last applied manifest snapshot.
pub fn cdn_last_path(game_dir: &Path) -> PathBuf {
    game_dir.join("cdn_last.json")
}

/// Well-known subfolders (mirrors the manifest `dest` values).
pub fn mods_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("mods")
}

/// Rename every `mods/*.pala` on disk back to its `.jar` base name
/// (`*.pala.disabled` → `*.jar.disabled`).
///
/// Defensive reverse migration for machines touched by the PalaForge-era
/// build (which stored every mod as `.pala` because PalaForge ignored
/// `.jar`) : vanilla Forge découvre les `.jar`, donc ces restes seraient
/// sinon invisibles. Bytes are never re-downloaded — only renamed. Idempotent:
/// already-`.jar` files are skipped, and an existing `.jar` target is never
/// overwritten (the stale `.pala` is then left in place). Returns the new
/// (`.jar`) file names, sorted.
///
/// Runs in the install ensure path (before planning downloads, so the
/// freshness check sees the final destinations and skips re-downloads).
pub fn migrate_mods_to_jar(game_dir: &Path) -> Vec<String> {
    const PALA_SUFFIX: &str = ".pala";
    const PALA_DISABLED_SUFFIX: &str = ".pala.disabled";
    let mods = mods_dir(game_dir);
    let Ok(entries) = std::fs::read_dir(&mods) else {
        return Vec::new();
    };
    let mut renamed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let mapped = if let Some(base) = name.strip_suffix(PALA_DISABLED_SUFFIX) {
            format!("{base}.jar.disabled")
        } else if let Some(base) = name.strip_suffix(PALA_SUFFIX) {
            format!("{base}.jar")
        } else {
            continue;
        };
        if mapped == name || mods.join(&mapped).exists() {
            continue;
        }
        if std::fs::rename(&path, mods.join(&mapped)).is_ok() {
            renamed.push(mapped);
        }
    }
    renamed.sort();
    renamed
}
/// Libraries folder (`dest: "libraries"`).
pub fn libraries_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("libraries")
}
/// Natives folder (`dest: "natives"`).
pub fn natives_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("natives")
}
/// Java runtime folder (`dest: "java"`).
pub fn java_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("java")
}
/// Experimental LWJGL3/Java-21 runtime folder (never clobbers `java/`).
pub fn java21_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("java21")
}
/// Versions folder (`dest: "versions"`).
pub fn versions_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("versions")
}
/// Assets folder (`dest: "assets"`).
pub fn assets_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("assets")
}
/// Launcher caches (Mojang version JSON, asset index — never game data).
pub fn caches_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("caches")
}

/// Uninstall report (counts for the confirmation toast).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallReport {
    /// Files deleted.
    pub removed_files: u64,
    /// Directories deleted (recursive counts roll into files).
    pub removed_dirs: u64,
}

/// Installed pack flavor marker (`default` | `experimental`). Written
/// after every successful install; a mismatch with the requested flavor
/// forces a reinstall that replaces the other stack's mods entirely
/// (switching branches never leaves both stacks active — Forge would
/// load every jar in `mods/`, including the wrong runtime's).
pub fn installed_flavor(game_dir: &Path) -> Option<String> {
    std::fs::read_to_string(game_dir.join(".installed_flavor"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Record the installed flavor (best effort; install correctness never
/// depends on it — worst case the next launch reinstalls).
pub fn write_installed_flavor(game_dir: &Path, experimental: bool) {
    let _ = std::fs::write(
        game_dir.join(".installed_flavor"),
        if experimental {
            "experimental"
        } else {
            "default"
        },
    );
}

/// Swap the OTHER flavor's mods when (re)installing — never delete, never
/// merge: switching branches replaces the active mod set while preserving
/// bytes. Classic (`experimental == false`) MOVES the launcher-managed
/// modern stack ([`crate::modern::MODERN_STACK_FILES`], `.disabled` twins
/// included) from `mods/` to the launcher-managed stash
/// (`<gameDir>/modstack-experimental/`, FML-invisible, never re-downloaded)
/// and restores stashed `optifine*` jars back to `mods/`;
/// experimental moves the modern stack BACK (only still-missing files are
/// downloaded afterwards) and stashes `optifine*` (`optifine*` prefix,
/// case-insensitive, `.disabled` twins included — OptiFine is replaced by
/// the Beddium stack on the modern branch and crashes its LWJGL3/Java 21
/// runtime). Both directions are idempotent and never overwrite: when a
/// `mods/` copy and a stash copy somehow coexist, the `mods/` copy wins for
/// the boot and the stash copy stays put. Manifest/CDN files are never
/// touched — only the launcher-managed stack plus `optifine*` move.
///
/// Returns the moved file names (sorted) for the Console log.
pub fn prune_other_flavor(game_dir: &Path, experimental: bool) -> Vec<String> {
    if experimental {
        let mut moved = crate::modern::restore_modern_stack(game_dir);
        moved.extend(stash_optifine(game_dir));
        moved.sort();
        moved
    } else {
        let mut moved = crate::modern::stash_modern_stack(game_dir);
        moved.extend(restore_optifine(game_dir));
        moved.sort();
        moved
    }
}

/// `optifine*` match (case-insensitive) on the file name, `.disabled` twin
/// stripped before matching so toggled-off copies swap too.
fn is_optifine_file_name(name: &str) -> bool {
    let base = name.strip_suffix(".disabled").unwrap_or(name);
    base.to_lowercase().starts_with("optifine")
}

/// Experimental-activate: MOVE every `optifine*` file from `mods/` to the
/// launcher-managed stash (reuses [`crate::modern::move_without_overwrite`]).
fn stash_optifine(game_dir: &Path) -> Vec<String> {
    let mods = mods_dir(game_dir);
    let stash = crate::modern::modern_stash_dir(game_dir);
    let mut bases: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&mods) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_optifine_file_name(name) {
                let base = name.strip_suffix(".disabled").unwrap_or(name).to_string();
                if !bases.iter().any(|b| b == &base) {
                    bases.push(base);
                }
            }
        }
    }
    let mut moved = Vec::new();
    for base in &bases {
        for variant in [base.clone(), format!("{base}.disabled")] {
            if crate::modern::move_without_overwrite(&mods.join(&variant), &stash.join(&variant)) {
                moved.push(variant);
            }
        }
    }
    moved
}

/// Classic-activate: move stashed `optifine*` files BACK to `mods/`.
fn restore_optifine(game_dir: &Path) -> Vec<String> {
    let mods = mods_dir(game_dir);
    let stash = crate::modern::modern_stash_dir(game_dir);
    let mut bases: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&stash) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_optifine_file_name(name) {
                let base = name.strip_suffix(".disabled").unwrap_or(name).to_string();
                if !bases.iter().any(|b| b == &base) {
                    bases.push(base);
                }
            }
        }
    }
    let mut restored = Vec::new();
    for base in &bases {
        for variant in [base.clone(), format!("{base}.disabled")] {
            if crate::modern::move_without_overwrite(&stash.join(&variant), &mods.join(&variant)) {
                restored.push(variant);
            }
        }
    }
    restored
}

/// Remove the installed game payload: mods, the experimental mod stash
/// (`modstack-experimental/` — no orphaned 100 Mo+ left behind),
/// libraries, versions, assets, natives, caches, resource/shader packs,
/// the manifest snapshot, and pack-owned configs. Deliberately KEPT:
/// `launcher-settings.json` (user
/// settings incl. accounts), `logs/` (forensics), `java/` + `java21/`
/// (runtimes are expensive to re-download — re-verified, not wiped).
/// Missing paths are skipped silently; IO errors abort with the path.
pub fn uninstall_instance(game_dir: &Path) -> std::io::Result<UninstallReport> {
    fn count_files(dir: &Path) -> u64 {
        let mut n = 0u64;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    n += 1;
                }
            }
        }
        n
    }
    let mut report = UninstallReport {
        removed_files: 0,
        removed_dirs: 0,
    };
    for dir in [
        mods_dir(game_dir),
        crate::modern::modern_stash_dir(game_dir),
        libraries_dir(game_dir),
        versions_dir(game_dir),
        assets_dir(game_dir),
        natives_dir(game_dir),
        caches_dir(game_dir),
        game_dir.join("resourcepacks"),
        game_dir.join("shaderpacks"),
    ] {
        if !dir.is_dir() {
            continue;
        }
        report.removed_files += count_files(&dir);
        std::fs::remove_dir_all(&dir)?;
        report.removed_dirs += 1;
    }
    // Pack-owned configs go; the launcher settings stay.
    let config_dir = game_dir.join("config");
    if config_dir.is_dir() {
        let entries = std::fs::read_dir(&config_dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .map(|n| n == "launcher-settings.json")
                .unwrap_or(false)
            {
                continue;
            }
            if path.is_dir() {
                report.removed_files += count_files(&path);
                std::fs::remove_dir_all(&path)?;
                report.removed_dirs += 1;
            } else {
                std::fs::remove_file(&path)?;
                report.removed_files += 1;
            }
        }
    }
    // Manifest snapshot: without it the status returns to `Init`
    // (main button flips back to INSTALLER).
    let snapshot = cdn_last_path(game_dir);
    if snapshot.is_file() {
        std::fs::remove_file(&snapshot)?;
        report.removed_files += 1;
    }
    Ok(report)
}

/// CDN freshness state machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateStatus {
    /// No `cdn_last.json` yet — first install.
    Init,
    /// Cached and fresh manifests share the same `version`.
    Ok {
        /// Agreed manifest version.
        version: String,
    },
    /// Fresh manifest moved to a new `version` — update required.
    OutOfDate {
        /// Version recorded in `cdn_last.json`.
        last_version: String,
        /// Version advertised by the CDN.
        new_version: String,
    },
    /// No fresh manifest available (network/HTTP failure) — reuse cache.
    CdnFailure(String),
}

/// Compare optional version strings (the `cdn_last.json` vs CDN core).
///
/// - `new == None` → [`UpdateStatus::CdnFailure`];
/// - `last == None` → [`UpdateStatus::Init`];
/// - equal → [`UpdateStatus::Ok`], else [`UpdateStatus::OutOfDate`].
pub fn compare_versions(last: Option<&str>, new: Option<&str>) -> UpdateStatus {
    match (last, new) {
        (_, None) => UpdateStatus::CdnFailure("empty fresh manifest".to_string()),
        (None, Some(_)) => UpdateStatus::Init,
        (Some(l), Some(n)) if l == n => UpdateStatus::Ok {
            version: n.to_string(),
        },
        (Some(l), Some(n)) => UpdateStatus::OutOfDate {
            last_version: l.to_string(),
            new_version: n.to_string(),
        },
    }
}

/// Compare full manifests by their `version` field.
pub fn compare_manifests(last: Option<&Manifest>, new: Option<&Manifest>) -> UpdateStatus {
    compare_versions(
        last.map(|m| m.version.as_str()),
        new.map(|m| m.version.as_str()),
    )
}

/// Load a cached manifest (`cdn_last.json`); `Ok(None)` when absent.
pub fn load_manifest(path: &Path) -> Result<Option<Manifest>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Persist a manifest snapshot (creates parent folders, pretty JSON).
pub fn save_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(manifest)?;
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    const JSON: &str = include_str!("../tests/fixtures/minimal_manifest.json");

    #[test]
    fn migrate_mods_to_jar_renames_without_redownload() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let mods = game.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("PalaMod-9.5.0.pala"), b"bytes").unwrap();
        std::fs::write(mods.join("Extra.pala.disabled"), b"off").unwrap();
        std::fs::write(mods.join("notes.txt"), b"n").unwrap();
        std::fs::write(mods.join("already.jar"), b"j").unwrap();
        let renamed = migrate_mods_to_jar(game);
        assert_eq!(
            renamed,
            vec![
                "Extra.jar.disabled".to_string(),
                "PalaMod-9.5.0.jar".to_string()
            ]
        );
        // Bytes untouched, no intermediate `.pala` left behind.
        assert_eq!(
            std::fs::read(mods.join("PalaMod-9.5.0.jar")).unwrap(),
            b"bytes"
        );
        assert!(!mods.join("PalaMod-9.5.0.pala").exists());
        assert!(!mods.join("Extra.pala.disabled").exists());
        assert!(mods.join("notes.txt").is_file());
        assert!(mods.join("already.jar").is_file());
        // Idempotent second run.
        assert!(migrate_mods_to_jar(game).is_empty());
    }

    #[test]
    fn migrate_never_overwrites_existing_jar() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let mods = game.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("Foo.jar"), b"keep").unwrap();
        std::fs::write(mods.join("Foo.pala"), b"stale").unwrap();
        assert!(migrate_mods_to_jar(game).is_empty());
        assert_eq!(std::fs::read(mods.join("Foo.jar")).unwrap(), b"keep");
        assert!(mods.join("Foo.pala").is_file());
    }

    #[test]
    fn flavor_swap_stashes_restores_and_never_deletes() {
        use crate::modern::{MODERN_STACK_FILES, MODERN_STASH_DIR_NAME};
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let mods = game.join("mods");
        let stash = game.join(MODERN_STASH_DIR_NAME);
        std::fs::create_dir_all(&mods).unwrap();
        // Full modern stack in `mods/` (as the experimental plan writes it)
        // plus user/CDN files that must never move.
        for n in MODERN_STACK_FILES {
            std::fs::write(mods.join(n), b"x").unwrap();
        }
        for n in ["PalaMod-9.5.0.jar", "optifine-prod.jar", "shared-mod.jar"] {
            std::fs::write(mods.join(n), b"pack").unwrap();
        }
        // Classic-activate stashes the whole stack (bytes preserved, dir
        // created on demand), keeps the pack and CDN files. OptiFine stays
        // on classic (restored when present in the stash, untouched
        // otherwise).
        let moved = prune_other_flavor(game, false);
        assert_eq!(moved.len(), MODERN_STACK_FILES.len(), "{moved:?}");
        for n in MODERN_STACK_FILES {
            assert!(!mods.join(n).exists(), "{n} hors mods/");
            assert_eq!(std::fs::read(stash.join(n)).unwrap(), b"x");
        }
        for n in ["PalaMod-9.5.0.jar", "optifine-prod.jar", "shared-mod.jar"] {
            assert_eq!(std::fs::read(mods.join(n)).unwrap(), b"pack");
            assert!(!stash.join(n).exists());
        }
        // Boot file-set clean: no modern jar left for the Java 8 Mixin/ASM
        // chain to trip over.
        let remaining: Vec<String> = std::fs::read_dir(&mods)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !remaining
                .iter()
                .any(|n| MODERN_STACK_FILES.contains(&n.as_str())),
            "{remaining:?}"
        );
        // Idempotent second run (same result, nothing moved).
        assert!(prune_other_flavor(game, false).is_empty());
        // Experimental-activate restores everything, no download needed,
        // and stashes `optifine*` (Beddium stack replaces OptiFine).
        let back = prune_other_flavor(game, true);
        assert_eq!(back.len(), MODERN_STACK_FILES.len() + 1, "{back:?}");
        assert!(back.contains(&"optifine-prod.jar".to_string()), "{back:?}");
        for n in MODERN_STACK_FILES {
            assert_eq!(std::fs::read(mods.join(n)).unwrap(), b"x");
            assert!(!stash.join(n).exists());
        }
        assert!(!mods.join("optifine-prod.jar").exists());
        assert_eq!(
            std::fs::read(stash.join("optifine-prod.jar")).unwrap(),
            b"pack"
        );
        assert!(prune_other_flavor(game, true).is_empty());
        // Back to classic restores OptiFine and re-stashes the modern stack.
        let classic_again = prune_other_flavor(game, false);
        assert_eq!(
            classic_again.len(),
            MODERN_STACK_FILES.len() + 1,
            "{classic_again:?}"
        );
        assert_eq!(
            std::fs::read(mods.join("optifine-prod.jar")).unwrap(),
            b"pack"
        );
        assert!(!stash.join("optifine-prod.jar").exists());
        // Flavor marker roundtrip (None when absent).
        assert_eq!(installed_flavor(game), None);
        write_installed_flavor(game, true);
        assert_eq!(installed_flavor(game), Some("experimental".to_string()));
        write_installed_flavor(game, false);
        assert_eq!(installed_flavor(game), Some("default".to_string()));
    }

    #[test]
    fn flavor_swap_moves_disabled_twins_and_keeps_both_on_conflict() {
        use crate::modern::{MODERN_OPT_BEDDIUM_FILE, MODERN_OPT_COMPUTIUM_FILE};
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let mods = game.join("mods");
        let stash = crate::modern::modern_stash_dir(game);
        std::fs::create_dir_all(&mods).unwrap();
        // Toggled-off twins stash alongside their enabled siblings.
        std::fs::write(mods.join(MODERN_OPT_BEDDIUM_FILE), b"on").unwrap();
        std::fs::write(
            mods.join(format!("{MODERN_OPT_COMPUTIUM_FILE}.disabled")),
            b"off",
        )
        .unwrap();
        let moved = prune_other_flavor(game, false);
        assert_eq!(
            moved,
            vec![
                MODERN_OPT_BEDDIUM_FILE.to_string(),
                format!("{MODERN_OPT_COMPUTIUM_FILE}.disabled"),
            ]
        );
        assert_eq!(
            std::fs::read(stash.join(format!("{MODERN_OPT_COMPUTIUM_FILE}.disabled"))).unwrap(),
            b"off"
        );
        // Restore brings the twin back to `mods/`.
        let back = prune_other_flavor(game, true);
        assert_eq!(back.len(), 2);
        assert!(mods
            .join(format!("{MODERN_OPT_COMPUTIUM_FILE}.disabled"))
            .is_file());
        // Conflict (both copies somehow exist): the `mods/` copy wins for
        // the boot, the stash copy stays put — nothing lost, nothing
        // overwritten. Isolate the pair first (the restored twin above
        // would otherwise stash again — also correct, just not this test).
        std::fs::remove_file(mods.join(format!("{MODERN_OPT_COMPUTIUM_FILE}.disabled"))).unwrap();
        std::fs::remove_file(mods.join(MODERN_OPT_BEDDIUM_FILE)).unwrap();
        std::fs::write(mods.join(MODERN_OPT_BEDDIUM_FILE), b"mods-copy").unwrap();
        std::fs::write(stash.join(MODERN_OPT_BEDDIUM_FILE), b"stash-copy").unwrap();
        assert!(prune_other_flavor(game, false).is_empty());
        assert_eq!(
            std::fs::read(mods.join(MODERN_OPT_BEDDIUM_FILE)).unwrap(),
            b"mods-copy"
        );
        assert_eq!(
            std::fs::read(stash.join(MODERN_OPT_BEDDIUM_FILE)).unwrap(),
            b"stash-copy"
        );
        assert!(prune_other_flavor(game, true).is_empty());
    }

    #[test]
    fn flavor_swap_stashes_legacy_unimixins_renames() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        let mods = game.join("mods");
        let stash = crate::modern::modern_stash_dir(game);
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("old-unimixins-tweaker.jar"), b"legacy").unwrap();
        std::fs::write(mods.join("PalaMod-9.5.0.jar"), b"pack").unwrap();
        let moved = prune_other_flavor(game, false);
        assert_eq!(moved, vec!["old-unimixins-tweaker.jar".to_string()]);
        assert_eq!(
            std::fs::read(stash.join("old-unimixins-tweaker.jar")).unwrap(),
            b"legacy"
        );
        assert!(mods.join("PalaMod-9.5.0.jar").is_file());
        // Symmetric restore (it could only have been placed by our stash).
        assert_eq!(
            prune_other_flavor(game, true),
            vec!["old-unimixins-tweaker.jar".to_string()]
        );
        assert_eq!(
            std::fs::read(mods.join("old-unimixins-tweaker.jar")).unwrap(),
            b"legacy"
        );
    }

    #[test]
    fn uninstall_keeps_settings_runtimes_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path();
        // Payload to wipe.
        for d in ["mods", "libraries", "versions", "assets", "config"] {
            std::fs::create_dir_all(game.join(d)).unwrap();
        }
        std::fs::write(game.join("mods/a.jar"), b"x").unwrap();
        std::fs::write(game.join("libraries/b.jar"), b"yy").unwrap();
        std::fs::write(game.join("config/pack.cfg"), b"z").unwrap();
        std::fs::write(game.join("cdn_last.json"), b"{}").unwrap();
        // Stashed experimental bytes (100 Mo+ scale in production) go too —
        // no orphaned stash left behind.
        let stash = crate::modern::modern_stash_dir(game);
        std::fs::create_dir_all(&stash).unwrap();
        std::fs::write(stash.join("beddium-mc1.7.10-1.2.3-j21.jar"), b"big").unwrap();
        // Protected: settings, runtimes, logs.
        std::fs::write(game.join("config/launcher-settings.json"), b"{\"a\":1}").unwrap();
        for d in ["logs", "java", "java21"] {
            std::fs::create_dir_all(game.join(d)).unwrap();
        }
        std::fs::write(game.join("logs/x.log"), b"l").unwrap();
        let rep = uninstall_instance(game).expect("uninstall");
        assert!(rep.removed_files >= 4, "{rep:?}");
        assert!(!game.join("mods").exists());
        assert!(!game.join("libraries").exists());
        assert!(!crate::modern::modern_stash_dir(game).exists());
        assert!(!game.join("cdn_last.json").exists());
        assert!(!game.join("config/pack.cfg").exists());
        assert!(game.join("config/launcher-settings.json").is_file());
        assert!(game.join("logs/x.log").is_file());
        assert!(game.join("java").is_dir() && game.join("java21").is_dir());
        // Idempotent on an empty instance.
        let rep2 = uninstall_instance(game).expect("second uninstall");
        assert_eq!(rep2.removed_files, 0);
    }

    #[test]
    fn status_transitions() {
        assert_eq!(
            compare_versions(None, None),
            UpdateStatus::CdnFailure("empty fresh manifest".to_string())
        );
        assert_eq!(compare_versions(None, Some("v1.0.0")), UpdateStatus::Init);
        assert_eq!(
            compare_versions(Some("v1.0.0"), Some("v1.0.0")),
            UpdateStatus::Ok {
                version: "v1.0.0".to_string()
            }
        );
        assert_eq!(
            compare_versions(Some("v1.0.0"), Some("v1.0.1")),
            UpdateStatus::OutOfDate {
                last_version: "v1.0.0".to_string(),
                new_version: "v1.0.1".to_string(),
            }
        );
        assert!(matches!(
            compare_versions(Some("v1.0.0"), None),
            UpdateStatus::CdnFailure(_)
        ));
    }

    #[test]
    fn manifests_compare_by_version() {
        let a = parse_manifest(JSON).unwrap();
        let mut b = a.clone();
        assert!(matches!(
            compare_manifests(Some(&a), Some(&b)),
            UpdateStatus::Ok { .. }
        ));
        b.version = "v9.9.9".to_string();
        assert!(matches!(
            compare_manifests(Some(&a), Some(&b)),
            UpdateStatus::OutOfDate { .. }
        ));
        assert!(matches!(
            compare_manifests(None, Some(&b)),
            UpdateStatus::Init
        ));
        assert!(matches!(
            compare_manifests(Some(&a), None),
            UpdateStatus::CdnFailure(_)
        ));
    }

    #[test]
    fn manifest_roundtrip_on_disk() {
        let m = parse_manifest(JSON).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cdn_last.json");
        assert!(load_manifest(&path).unwrap().is_none());
        save_manifest(&path, &m).unwrap();
        let back = load_manifest(&path).unwrap().unwrap();
        assert_eq!(back.version, m.version);
        assert_eq!(back.files.len(), m.files.len());
    }

    #[test]
    fn game_dir_ends_with_dot_46anarchy_paladium() {
        let dir = default_game_dir();
        assert!(dir.ends_with("paladium"));
        assert!(dir.ends_with(Path::new(GAME_DIR_NAME).join(GAME_ROOT_NAME)));
    }

    #[test]
    fn normalize_game_dir_appends_paladium_once() {
        assert_eq!(
            normalize_game_dir(Path::new("/home/u/.46anarchy")),
            PathBuf::from("/home/u/.46anarchy/paladium")
        );
        assert_eq!(
            normalize_game_dir(Path::new("/home/u/.46anarchy/paladium")),
            PathBuf::from("/home/u/.46anarchy/paladium")
        );
        // Case-sensitive like the mod check: "PaladiumPack" does NOT
        // qualify, so the subdir is appended.
        assert_eq!(
            normalize_game_dir(Path::new("D:/Games/PaladiumPack")),
            PathBuf::from("D:/Games/PaladiumPack/paladium")
        );
    }
}
