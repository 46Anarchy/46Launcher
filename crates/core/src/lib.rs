// SPDX-License-Identifier: AGPL-3.0-only

//! `launcher-core`: pure game-file logic for the 46Anarchy launcher.
//!
//! No UI, no auth flows — just manifest parsing, OS-gated integrity rules,
//! SHA1 verification, parallel downloads, and instance bookkeeping. The
//! `auth` and `ui` crates build on this API.
//!
//! Quick start for `auth`/`ui`:
//! ```no_run
//! use launcher_core::{default_game_dir, fetch_manifest, plan_downloads_for_current_os};
//! use launcher_core::{build_client, cdn_last_path, compare_manifests, download_files, load_manifest};
//! ```

pub mod download;
pub mod error;
pub mod hash;
pub mod instance;
pub mod integrity;
pub mod java;
pub mod launch;
pub mod manifest;
pub mod modern;
pub mod mojang;
pub mod os;

// --- error ---
pub use error::{CoreError, Result};

// --- os ---
pub use os::{is_allowed, is_allowed_for, system_memory_mb, Os};

// --- manifest ---
pub use manifest::{
    fetch_manifest, parse_manifest, Arguments, Maintenance, Manifest, ManifestFile, Model,
    ModelProperties, MANIFEST_URL,
};

// --- integrity ---
pub use integrity::{
    cdn_libraries_stale, check_jar_class_versions, class_version_cap_for, file_allowed,
    is_cdn_patched_lwjgl, is_dedicated_server_file, is_file_blacklisted, is_libraries_model,
    is_palaforge_file, max_class_version_in_jar, model_allowed, plan_dedupe_key, plan_downloads,
    plan_downloads_for_current_os, resolve_local_path, should_download, validate_rel_path,
    PlannedFile, BLACKLISTED_FILES, CLASSIC_MAX_CLASS_VERSION, DEDICATED_SERVER_PATH_PREFIX,
    EXPERIMENTAL_MAX_CLASS_VERSION,
};

// --- hash ---
pub use hash::{ensure_file_hash, hash_file, sha1_hex, verify_bytes, verify_file};

// --- download ---
pub use download::{
    build_client, download_files, is_cancelled, needs_update, DownloadSummary, ProgressUpdate,
    DEFAULT_MAX_CONCURRENCY, DEFAULT_MAX_RETRIES,
};

// --- instance ---
pub use instance::{
    assets_dir, caches_dir, cdn_last_path, compare_manifests, compare_versions, default_game_dir,
    installed_flavor, java21_dir, java_dir, libraries_dir, load_manifest, migrate_mods_to_jar,
    mods_dir, natives_dir, normalize_game_dir, prune_other_flavor, save_manifest,
    uninstall_instance, versions_dir, write_installed_flavor, UninstallReport, UpdateStatus,
    GAME_DIR_NAME, GAME_ROOT_NAME,
};

// --- java ---
pub use java::{
    bundled_java_bin, bundled_java_bin_for, check_java, exe_name, extract_java_archive,
    install_java_from_bytes, install_java_from_bytes_in, is_java8_output, is_modern_java_output,
    java8_status, java_exe_name, java_home_bin, java_home_bin_for, java_home_bin_in,
    java_version_output, java_version_warning, java_warning_for, parse_java_major, provision_java8,
    provision_java8_from_url, provision_temurin, provision_temurin_from_url, resolve_java,
    resolve_java_for, resolve_java_with_home, temurin_arch_for, temurin_archive_kind_for,
    temurin_os_for, temurin_url_current, temurin_url_current_for_major, temurin_url_for,
    temurin_url_for_major, Java8Status, JavaCheck, JavaProgressCallback, TEMURIN_API_BASE,
};

// --- launch ---
pub use launch::{
    build_classpath, build_full_command, classpath_jar_paths, classpath_separator, library_path,
    quote_command, substitute_arg, substitute_args, FullCommand, LaunchProfile, Settings, UserType,
    DEFAULT_MEMORY_MAX, DEFAULT_MEMORY_MIN,
};

// --- modern (experimental LWJGL3 + Java 21 runtime) ---
pub use modern::{
    graalvm_url_for, java_runtime_for_flavor, load_cached_modern_profile,
    load_or_fetch_modern_profile, modern_cache_path, modern_cdn_classpath_entries,
    modern_cdn_lib_wanted, modern_classpath_entries, modern_dedupe_newest, modern_env_vars,
    modern_jvm_extra_args, modern_lib_allowed, modern_opt_mods_stale, modern_os_name,
    modern_runtime_stale, modern_stash_dir, plan_modern_coremods, plan_modern_libraries,
    plan_modern_opt_mods, resolve_modern_java, restore_modern_stack, seed_modern_configs,
    stash_modern_stack, ModernProfile, GRAALVM_VERSION, MODERN_CACHE_FILE, MODERN_CDN_LIBS,
    MODERN_CLASSLOADER_FLAG, MODERN_COMPAT_FILE, MODERN_COMPAT_URL, MODERN_COREMOD_URL,
    MODERN_GC_TUNING, MODERN_JAVA_DIR, MODERN_JAVA_MAJOR, MODERN_JVM_WALL, MODERN_LWJGL3IFY_FILE,
    MODERN_MAIN_CLASS, MODERN_OPT_ARCHAICFIX_FILE, MODERN_OPT_ARCHAICFIX_URL,
    MODERN_OPT_BEDDIUM_FILE, MODERN_OPT_BEDDIUM_URL, MODERN_OPT_COMPUTIUM_FILE,
    MODERN_OPT_COMPUTIUM_URL, MODERN_OPT_FALSEPATTERNLIB_FILE, MODERN_OPT_FALSEPATTERNLIB_URL,
    MODERN_OPT_FALSETWEAKS_FILE, MODERN_OPT_FALSETWEAKS_URL, MODERN_OPT_GTNHLIB_FILE,
    MODERN_OPT_GTNHLIB_URL, MODERN_OPT_HODGEPODGE_FILE, MODERN_OPT_HODGEPODGE_URL,
    MODERN_PROFILE_URL, MODERN_PROFILE_VERSION, MODERN_STACK_FILES, MODERN_STASH_DIR_NAME,
    MODERN_STENCIL_FLAG, MODERN_UNIMIXINS_FILE, MODERN_UNIMIXINS_URL,
};

// --- mojang (vanilla fallback: client jar, per-OS natives, assets, hardening) ---
pub use mojang::{
    artifact_base, artifact_local_path, coordinate_base, extract_all_natives, extract_natives,
    forge_jar_present, forge_universal_attempts, forge_universal_dest, forge_zip_ok, harden_log4j,
    load_cached_asset_index, load_cached_mojang_version, load_or_fetch_asset_index,
    load_or_fetch_mojang_version, log4j_patched_sha, mojang_cache_path, mojang_classpath_entries,
    mojang_lib_allowed, mojang_os_name, mojang_runtime_stale, native_classifier_keys,
    plan_forge_universal, plan_mojang_assets, plan_mojang_libraries, plan_vanilla_jar,
    select_native_classifier, Log4jReport, MojangArtifact, MojangAssetIndex, MojangAssetIndexRef,
    MojangAssetObject, MojangClientDownload, MojangLibrary, MojangLibraryDownloads, MojangRule,
    MojangVersion, MojangVersionDownloads, FORGE_MAVEN_FALLBACK, FORGE_MAVEN_PRIMARY,
    FORGE_UNIVERSAL_FILE, FORGE_UNIVERSAL_PATH, FORGE_UNIVERSAL_SHA1, FORGE_UNIVERSAL_SIZE,
    FORGE_VERSION, JNDI_LOOKUP_PREFIX, LOG4J_SIDECAR, MOJANG_ASSET_INDEX_CACHE_FILE,
    MOJANG_CACHE_FILE, MOJANG_MANIFEST_URL, MOJANG_MC_VERSION, MOJANG_RESOURCES_BASE,
};
