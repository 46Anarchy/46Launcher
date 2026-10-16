// SPDX-License-Identifier: AGPL-3.0-only

//! OS model: the manifest tags models/files with `os: ["WINDOWS"|"MACOS"|"LINUX"]`.
//!
//! Filter rule (mirrors the legacy launcher): a model/file is relevant when its
//! `os` list is empty (cross-platform) **or** contains the current OS.

use serde::{Deserialize, Deserializer, Serialize};

/// OS identifiers exactly as they appear in `manifest.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Os {
    /// `WINDOWS` in the manifest; `std::env::consts::OS == "windows"`.
    #[serde(rename = "WINDOWS")]
    Windows,
    /// `MACOS` in the manifest; `std::env::consts::OS == "macos"`.
    #[serde(rename = "MACOS")]
    Macos,
    /// `LINUX` in the manifest; everything else maps here (incl. `"linux"`).
    #[serde(rename = "LINUX")]
    Linux,
}

impl<'de> Deserialize<'de> for Os {
    /// Insensible à la casse : `WINDOWS`, `windows`, `Windows` (et
    /// `macos`/`osx`/`darwin`, `linux`) sont équivalents.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().as_str() {
            "windows" | "win" => Ok(Self::Windows),
            "macos" | "mac" | "osx" | "darwin" => Ok(Self::Macos),
            "linux" => Ok(Self::Linux),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["WINDOWS", "MACOS", "LINUX"],
            )),
        }
    }
}

impl Os {
    /// Detect the current OS via `std::env::consts::OS`.
    ///
    /// Mapping: `"windows"` → `Windows`, `"macos"` → `Macos`, anything else →
    /// `Linux` (covers `"linux"` and BSDs, matching the legacy launcher which
    /// only ships `*-linux` natives for the third slot).
    pub fn current() -> Self {
        Self::from_consts_str(std::env::consts::OS)
    }

    /// Pure mapping helper (testable without depending on the host).
    pub fn from_consts_str(s: &str) -> Self {
        match s {
            "windows" => Self::Windows,
            "macos" => Self::Macos,
            _ => Self::Linux,
        }
    }

    /// Manifest spelling (`"WINDOWS"` / `"MACOS"` / `"LINUX"`).
    pub fn as_manifest_str(self) -> &'static str {
        match self {
            Self::Windows => "WINDOWS",
            Self::Macos => "MACOS",
            Self::Linux => "LINUX",
        }
    }

    /// Model-name suffix used for the per-OS models
    /// (`libraries-*`, `natives-*`, `java-*`), e.g. `"windows"`.
    pub fn model_suffix(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }

    /// OS déduit du suffixe `-windows`/`-macos`/`-linux` d'un nom de
    /// modèle (insensible à la casse ; `-osx`/`-mac` valent `macos`).
    /// Signal de repli quand le champ `os` du modèle est vide : le
    /// manifeste utilise les deux conventions en parallèle.
    pub fn from_model_suffix(name: &str) -> Option<Self> {
        // Sans allocation : comparaisons ASCII insensibles à la casse
        // (appelé une fois par fichier et par plan — ~500× par JOUER).
        let b = name.as_bytes();
        fn ends_with_ci(hay: &[u8], needle: &[u8]) -> bool {
            hay.len() >= needle.len()
                && hay[hay.len() - needle.len()..].eq_ignore_ascii_case(needle)
        }
        if ends_with_ci(b, b"-windows") || name.eq_ignore_ascii_case("windows") {
            Some(Self::Windows)
        } else if ends_with_ci(b, b"-macos")
            || ends_with_ci(b, b"-osx")
            || ends_with_ci(b, b"-mac")
            || name.eq_ignore_ascii_case("macos")
        {
            Some(Self::Macos)
        } else if ends_with_ci(b, b"-linux") || name.eq_ignore_ascii_case("linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }
}

/// Current-OS shorthand for [`is_allowed_for`].
pub fn is_allowed(os_list: &[Os]) -> bool {
    is_allowed_for(os_list, Os::current())
}

/// Core OS filter: empty list means cross-platform (keep); otherwise keep only
/// when `current` is listed.
pub fn is_allowed_for(os_list: &[Os], current: Os) -> bool {
    os_list.is_empty() || os_list.contains(&current)
}

/// Total physical RAM in MiB (for the memory slider ceiling). sysinfo 0.30+
/// reports bytes; clamped to a sane range so a version-skew surprise (KB
/// vs bytes) can never size a heap at 46 TB.
pub fn system_memory_mb() -> u64 {
    let bytes = sysinfo::System::new_all().total_memory();
    let mb = bytes / 1024 / 1024;
    mb.clamp(512, 2 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_is_cross_platform() {
        assert!(is_allowed_for(&[], Os::Windows));
        assert!(is_allowed_for(&[], Os::Macos));
        assert!(is_allowed_for(&[], Os::Linux));
    }

    #[test]
    fn only_matching_os_is_kept() {
        assert!(is_allowed_for(&[Os::Windows], Os::Windows));
        assert!(!is_allowed_for(&[Os::Windows], Os::Linux));
        assert!(!is_allowed_for(&[Os::Macos], Os::Linux));
        assert!(is_allowed_for(&[Os::Macos, Os::Linux], Os::Linux));
    }

    #[test]
    fn consts_mapping() {
        assert_eq!(Os::from_consts_str("windows"), Os::Windows);
        assert_eq!(Os::from_consts_str("macos"), Os::Macos);
        assert_eq!(Os::from_consts_str("linux"), Os::Linux);
        // Unknown unix-likes fall back to the linux payloads.
        assert_eq!(Os::from_consts_str("freebsd"), Os::Linux);
    }

    #[test]
    fn model_suffix_match_is_case_insensitive_without_alloc() {
        // Garde anti-régression : même verdicts que l'ancienne forme
        // `to_ascii_lowercase()`, sans allocation par appel.
        assert_eq!(
            Os::from_model_suffix("libraries-WINDOWS"),
            Some(Os::Windows)
        );
        assert_eq!(Os::from_model_suffix("natives-MacOS"), Some(Os::Macos));
        assert_eq!(Os::from_model_suffix("java-OSX"), Some(Os::Macos));
        assert_eq!(Os::from_model_suffix("natives-MAC"), Some(Os::Macos));
        assert_eq!(Os::from_model_suffix("java-LINUX"), Some(Os::Linux));
        assert_eq!(Os::from_model_suffix("mods"), None);
    }

    #[test]
    fn system_memory_in_sane_range() {
        // Any real machine (incl. CI runners and containers reporting the
        // host): more than 512 MiB, less than 2 TiB.
        let mb = system_memory_mb();
        assert!((512..=2 * 1024 * 1024).contains(&mb), "saw {mb} MiB");
    }
}
