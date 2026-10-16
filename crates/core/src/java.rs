// SPDX-License-Identifier: AGPL-3.0-only

//! Java 8 resolution for Forge 1.7.10.
//!
//! Lookup order in [`resolve_java`]:
//! 1. bundled runtime `<gameDir>/java/bin/java(.exe)` (manifest `java-*` models,
//!    `dest: "java"`);
//! 2. `$JAVA_HOME/bin/java(.exe)`;
//! 3. bare `"java"` resolved via `PATH`.
//!
//! Version helpers are best-effort: `java -version` prints to stderr, so
//! [`java_version_output`] merges both streams. Forge 1.7.10 requires Java 8
//! (output contains `1.8`); Java 17+ launches it only with extra `--add-opens`
//! flags, hence [`is_modern_java_output`] drives the UI warning.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::instance::java_dir;
use crate::os::Os;

/// Binary name per OS (`java.exe` on Windows, `java` elsewhere).
pub fn java_exe_name(os: Os) -> &'static str {
    match os {
        Os::Windows => "java.exe",
        _ => "java",
    }
}

/// Current-OS binary name.
pub fn exe_name() -> &'static str {
    java_exe_name(Os::current())
}

/// `<gameDir>/java/bin/java(.exe)` for a given OS (testable cross-platform).
pub fn bundled_java_bin_for(game_dir: &Path, os: Os) -> PathBuf {
    java_dir(game_dir).join("bin").join(java_exe_name(os))
}

/// Current-OS bundled path.
pub fn bundled_java_bin(game_dir: &Path) -> PathBuf {
    bundled_java_bin_for(game_dir, Os::current())
}

/// `$JAVA_HOME/bin/java(.exe)` when `JAVA_HOME` points at a dir containing it.
pub fn java_home_bin_for(os: Os) -> Option<PathBuf> {
    let home = std::env::var_os("JAVA_HOME")?;
    if home.is_empty() {
        return None;
    }
    java_home_bin_in(Path::new(&home), os)
}

/// Explicit-home core of [`java_home_bin_for`] (no env access; test-safe).
pub fn java_home_bin_in(home: &Path, os: Os) -> Option<PathBuf> {
    let cand = home.join("bin").join(java_exe_name(os));
    if cand.is_file() {
        Some(cand)
    } else {
        None
    }
}

/// Current-OS `$JAVA_HOME` candidate.
pub fn java_home_bin() -> Option<PathBuf> {
    java_home_bin_for(Os::current())
}

/// Resolve the `java` binary for `game_dir` (no network, no spawn).
///
/// 1. bundled `<gameDir>/java/bin/java(.exe)` when the file exists;
/// 2. `$JAVA_HOME/bin/java(.exe)` when that file exists;
/// 3. bare `java` (PATH lookup at spawn time).
pub fn resolve_java(game_dir: &Path) -> PathBuf {
    resolve_java_for(game_dir, Os::current())
}

/// OS-parameterized core of [`resolve_java`] (lets tests cover the `.exe`
/// suffix on any host).
pub fn resolve_java_for(game_dir: &Path, os: Os) -> PathBuf {
    let bundled = bundled_java_bin_for(game_dir, os);
    if bundled.is_file() {
        return bundled;
    }
    if let Some(home) = java_home_bin_for(os) {
        return home;
    }
    PathBuf::from("java")
}

/// Explicit-home variant (no env access; parallel-test safe).
pub fn resolve_java_with_home(game_dir: &Path, os: Os, java_home: Option<&Path>) -> PathBuf {
    let bundled = bundled_java_bin_for(game_dir, os);
    if bundled.is_file() {
        return bundled;
    }
    if let Some(home) = java_home.and_then(|h| java_home_bin_in(h, os)) {
        return home;
    }
    PathBuf::from("java")
}

/// Run `<java> -version` and return merged stdout+stderr (best-effort).
pub fn java_version_output(java_bin: &Path) -> Option<String> {
    let out = Command::new(java_bin).arg("-version").output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    if text.trim().is_empty() && !out.status.success() {
        return None;
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Parse the major version from `java -version` output.
///
/// Handles legacy `java version "1.8.0_392"` → `8` and modern
/// `openjdk version "17.0.9"` → `17`. Returns `None` when unparseable.
pub fn parse_java_major(output: &str) -> Option<u32> {
    for line in output.lines() {
        // Find the first quoted token: version "1.8.0_392" / "17.0.9" / ...
        // Lines without quotes cannot carry a version: skip them instead of
        // aborting the whole scan (java -version prints several lines).
        let Some(start) = line.find('"') else {
            continue;
        };
        let rest = &line[start + 1..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let ver = &rest[..end];
        let mut parts = ver.split(['.', '_', '-', '+']);
        let Some(first_str) = parts.next() else {
            continue;
        };
        let Ok(first): Result<u32, _> = first_str.parse() else {
            continue;
        };
        if first == 1 {
            // Legacy 1.<major>: 1.8.x -> 8.
            let Some(minor_str) = parts.next() else {
                continue;
            };
            let Ok(minor): Result<u32, _> = minor_str.parse() else {
                continue;
            };
            return Some(minor);
        }
        return Some(first);
    }
    None
}

/// `true` when the output looks like Java 8 (`1.8.x`).
pub fn is_java8_output(output: &str) -> bool {
    parse_java_major(output) == Some(8)
}

/// `true` when the output looks like Java 17+ (warn: Forge 1.7.10 needs
/// extra `--add-opens` flags there and prefers Java 8).
pub fn is_modern_java_output(output: &str) -> bool {
    matches!(parse_java_major(output), Some(m) if m >= 17)
}

/// French UI warning for a parsed major version.
///
/// - `None` (unparseable probe) → warning (version unknown, Java 8 required);
/// - `Some(8)` → no warning;
/// - `Some(>=17)` → modern-Java warning (`--add-opens` needed);
/// - any other major → non-Java-8 warning.
pub fn java_version_warning(major: Option<u32>) -> Option<String> {
    match major {
        Some(8) => None,
        Some(m) if m >= 17 => Some(format!(
            "Java {m} détecté : Forge 1.7.10 exige Java 8 (le lancement risque d'échouer sans drapeaux --add-opens supplémentaires)."
        )),
        Some(m) => Some(format!(
            "Java {m} détecté : Forge 1.7.10 exige Java 8 (version non prise en charge)."
        )),
        None => Some(
            "Version Java indétectable : Forge 1.7.10 exige Java 8. Vérifiez votre installation Java."
                .to_string(),
        ),
    }
}

/// Warning for a full [`JavaCheck`] probe (convenience over
/// [`java_version_warning`]).
pub fn java_warning_for(check: &JavaCheck) -> Option<String> {
    java_version_warning(check.major)
}

/// Best-effort version probe of a resolved binary.
#[derive(Debug, Clone)]
pub struct JavaCheck {
    /// Raw merged `java -version` output, if the spawn succeeded.
    pub output: Option<String>,
    /// Parsed major version, if recognizable.
    pub major: Option<u32>,
    /// `major == 8` — the Forge 1.7.10 requirement.
    pub is_java8: bool,
    /// `major >= 17` — UI should warn.
    pub warns_modern: bool,
}

/// Probe `java_bin` via `java -version` (never fails hard).
pub fn check_java(java_bin: &Path) -> JavaCheck {
    let output = java_version_output(java_bin);
    let major = output.as_deref().and_then(parse_java_major);
    JavaCheck {
        is_java8: major == Some(8),
        warns_modern: matches!(major, Some(m) if m >= 17),
        major,
        output,
    }
}

// ---------------------------------------------------------------------------
// Java 8 auto-provision (Adoptium Temurin 8 JRE)
// ---------------------------------------------------------------------------

/// Base of the Adoptium v3 binary API (fixed to feature 8 / GA release).
pub const TEMURIN_API_BASE: &str = "https://api.adoptium.net/v3/binary/latest/8/ga";

/// Map `std::env::consts::OS` to the Adoptium API OS segment.
///
/// `windows` → `windows`, `macos`/`mac`/`darwin` → `mac`, anything else →
/// `linux` (covers `linux` and other unix-likes, like [`Os::from_consts_str`]).
pub fn temurin_os_for(std_os: &str) -> &'static str {
    match std_os {
        "windows" | "win" => "windows",
        "macos" | "mac" | "darwin" => "mac",
        _ => "linux",
    }
}

/// Map `std::env::consts::ARCH` to the Adoptium API arch segment.
///
/// `x86_64`/`x64`/`amd64` → `x64`, `aarch64`/`arm64` → `aarch64`; anything
/// else falls back to `x64` (the only other Temurin 8 desktop tier).
pub fn temurin_arch_for(std_arch: &str) -> &'static str {
    match std_arch {
        "aarch64" | "arm64" => "aarch64",
        _ => "x64",
    }
}

/// Full Temurin 8 JRE download URL for a (`std::env::consts::{OS, ARCH}`)
/// pair, e.g. `temurin_url_for("linux", "x86_64")`.
///
/// `.zip` on Windows, `.tar.gz` on linux/mac (decided server-side by the API;
/// see [`temurin_archive_kind_for`]).
pub fn temurin_url_for(std_os: &str, std_arch: &str) -> String {
    format!(
        "{TEMURIN_API_BASE}/{}/{}/jre/hotspot/normal/eclipse",
        temurin_os_for(std_os),
        temurin_arch_for(std_arch),
    )
}

/// Full Temurin JRE download URL for a feature release + host pair, e.g.
/// `temurin_url_for_major(21, "linux", "x86_64")`.
///
/// Same layout as [`temurin_url_for`], which pins feature 8.
pub fn temurin_url_for_major(major: u32, std_os: &str, std_arch: &str) -> String {
    format!(
        "https://api.adoptium.net/v3/binary/latest/{major}/ga/{}/{}/jre/hotspot/normal/eclipse",
        temurin_os_for(std_os),
        temurin_arch_for(std_arch),
    )
}

/// Temurin JRE URL for the current host and `major` release.
pub fn temurin_url_current_for_major(major: u32) -> String {
    temurin_url_for_major(major, std::env::consts::OS, std::env::consts::ARCH)
}

/// Temurin 8 JRE URL for the current host.
pub fn temurin_url_current() -> String {
    temurin_url_current_for_major(8)
}

/// Archive kind served by the Temurin API for an OS (`zip` on Windows,
/// `tar.gz` on linux/mac).
pub fn temurin_archive_kind_for(std_os: &str) -> &'static str {
    if temurin_os_for(std_os) == "windows" {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Flat Java 8 readiness view for `game_dir` (no network, best-effort probe).
#[derive(Debug, Clone)]
pub struct Java8Status {
    /// Resolved binary from [`resolve_java`] (bundled → `JAVA_HOME` → `PATH`).
    pub path: PathBuf,
    /// Parsed major version, if recognizable.
    pub major: Option<u32>,
    /// `major == 8` — the Forge 1.7.10 requirement.
    pub is_java8: bool,
}

/// Probe the resolved runtime for `game_dir` (never fails hard; missing or
/// unparseable Java reports `is_java8: false`).
pub fn java8_status(game_dir: &Path) -> Java8Status {
    let path = resolve_java(game_dir);
    let check = check_java(&path);
    Java8Status {
        path,
        major: check.major,
        is_java8: check.is_java8,
    }
}

/// Progress callback for [`provision_java8`]: `(downloaded_bytes,
/// total_bytes)`; `total_bytes == 0` when the server omits the length.
pub type JavaProgressCallback = dyn Fn(u64, u64) + Send + Sync;

/// Drop the top-level folder component of an archive entry
/// (`jdk8uXXX-jre/bin/java` → `bin/java`).
///
/// Returns `None` for the top-level entry itself (nothing left after the
/// strip) so callers skip it. Rejects absolute paths and `..` escapes.
fn strip_top_level(entry: &Path) -> Option<PathBuf> {
    // Absolute entries would escape the destination once joined: refuse them.
    // Any `..` likewise escapes (even `top/../../evil`): refuse those too.
    if entry.is_absolute()
        || entry.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let mut parts: Vec<&std::ffi::OsStr> = entry
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(p) => Some(p),
            // Skip the archive root prefix/curdir markers; reject anything
            // that would escape the destination.
            std::path::Component::CurDir => None,
            _ => None,
        })
        // First normal component is the top-level folder: drop it.
        .skip(1)
        .collect();
    // macOS archives (Temurin + GraalVM tarballs) nest the payload one
    // level deeper (`<top>/Contents/Home/bin/java`): drop that prefix so
    // `bin/java` lands at the expected depth on every OS. Linux/Windows
    // archives never carry it, so they are unaffected.
    if parts.len() >= 2 && parts[0] == "Contents" && parts[1] == "Home" {
        parts.drain(..2);
    }
    // Top-level entry itself or a `.`-only path: nothing to install.
    if parts.is_empty() {
        return None;
    }
    let mut rel = PathBuf::new();
    // `..` cannot appear here (filtered above), but belt-and-braces: the
    // caller joins this onto the destination, so never emit ParentDir.
    for part in parts {
        if part == ".." {
            return None;
        }
        rel.push(part);
    }
    Some(rel)
}

/// Extract a Temurin 8 archive (`.zip` or `.tar.gz`, sniffed by magic bytes)
/// into `dest`, stripping the top-level folder. `dest` is created when
/// missing; existing files are overwritten.
pub fn extract_java_archive(bytes: &[u8], dest: &Path) -> Result<PathBuf, crate::error::CoreError> {
    std::fs::create_dir_all(dest)?;
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        extract_zip_stripped(bytes, dest)
    } else if bytes.starts_with(b"\x1f\x8b") {
        extract_tar_gz_stripped(bytes, dest)
    } else {
        Err(crate::error::CoreError::CdnFailure(
            "Archive Java illisible : format inattendu (ni zip ni tar.gz).".to_string(),
        ))
    }
}

fn extract_zip_stripped(bytes: &[u8], dest: &Path) -> Result<PathBuf, crate::error::CoreError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        crate::error::CoreError::CdnFailure(format!("Archive Java illisible (zip) : {e}"))
    })?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| {
            crate::error::CoreError::CdnFailure(format!("Archive Java illisible (zip) : {e}"))
        })?;
        let Some(rel) = entry.enclosed_name().and_then(|p| strip_top_level(&p)) else {
            continue;
        };
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut file)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            // Preserve the archived executable bits (Temurin ships 0755).
            let perms = std::fs::Permissions::from_mode(mode);
            let _ = std::fs::set_permissions(&out, perms);
        }
    }
    Ok(dest.to_path_buf())
}

fn extract_tar_gz_stripped(bytes: &[u8], dest: &Path) -> Result<PathBuf, crate::error::CoreError> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    let entries = archive.entries().map_err(|e| {
        crate::error::CoreError::CdnFailure(format!("Archive Java illisible (tar.gz) : {e}"))
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            crate::error::CoreError::CdnFailure(format!("Archive Java illisible (tar.gz) : {e}"))
        })?;
        let path = entry.path().map_err(|e| {
            crate::error::CoreError::CdnFailure(format!("Archive Java illisible (tar.gz) : {e}"))
        })?;
        let Some(rel) = strip_top_level(&path) else {
            continue;
        };
        let out = dest.join(&rel);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&out).map_err(|e| {
            crate::error::CoreError::CdnFailure(format!("Archive Java illisible (tar.gz) : {e}"))
        })?;
    }
    Ok(dest.to_path_buf())
}

/// Best-effort `chmod +x` on the provisioned `bin/java` (unix only).
fn make_provisioned_java_executable(java_bin: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(java_bin) {
            let mut perms = meta.permissions();
            let mode = perms.mode();
            if mode & 0o111 != 0o111 {
                perms.set_mode(mode | 0o755);
                let _ = std::fs::set_permissions(java_bin, perms);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = java_bin;
    }
}

/// Verify a freshly extracted runtime really is the expected `major`
/// (`bin/java -version` parses to it).
fn verify_provisioned_java(java_bin: &Path, major: u32) -> Result<(), crate::error::CoreError> {
    let output = java_version_output(java_bin).unwrap_or_default();
    match parse_java_major(&output) {
        Some(m) if m == major => Ok(()),
        Some(m) => Err(crate::error::CoreError::CdnFailure(format!(
            "Le runtime installé n'est pas Java {major} (Java {m} détecté). Réessayez le téléchargement."
        ))),
        None => Err(crate::error::CoreError::CdnFailure(format!(
            "Le runtime installé ne répond pas comme Java {major} (`java -version` illisible)."
        ))),
    }
}

/// Install a Java runtime from raw archive bytes into
/// `<gameDir>/<dir_name>` (e.g. `java`, `java21`).
///
/// Staging: extracts into `<gameDir>/<dir_name>.tmp`, then swaps it over
/// (old dir removed first). Verifies `bin/java -version` parses to
/// `expected_major` before reporting success. Returns the verified
/// `bin/java(.exe)` path.
pub fn install_java_from_bytes_in(
    game_dir: &Path,
    bytes: &[u8],
    dir_name: &str,
    expected_major: u32,
) -> Result<PathBuf, crate::error::CoreError> {
    let dest = game_dir.join(dir_name);
    let staging = dest.with_extension("tmp");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).ok();
    }
    let outcome = extract_java_archive(bytes, &staging);
    if let Err(e) = outcome {
        std::fs::remove_dir_all(&staging).ok();
        return Err(e);
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    match std::fs::rename(&staging, &dest) {
        Ok(()) => {}
        Err(e) => {
            std::fs::remove_dir_all(&staging).ok();
            return Err(e.into());
        }
    }
    let java_bin = dest.join("bin").join(exe_name());
    make_provisioned_java_executable(&java_bin);
    if let Err(e) = verify_provisioned_java(&java_bin, expected_major) {
        std::fs::remove_dir_all(&dest).ok();
        return Err(e);
    }
    Ok(java_bin)
}

/// Install a Java 8 runtime from raw archive bytes into `<gameDir>/java`.
///
/// Staging: extracts into `<gameDir>/java.tmp`, then swaps it over
/// `<gameDir>/java` (old dir removed first). Verifies `bin/java -version`
/// parses to major 8 before reporting success. Returns the verified
/// `bin/java(.exe)` path.
pub fn install_java_from_bytes(
    game_dir: &Path,
    bytes: &[u8],
) -> Result<PathBuf, crate::error::CoreError> {
    install_java_from_bytes_in(game_dir, bytes, "java", 8)
}

/// Download the Temurin 8 JRE from `url` into memory, forwarding
/// `(downloaded, total)` progress. Follows redirects (reqwest default).
/// `cancel` (the frontend `Annuler` flag) is checked per chunk.
async fn download_temurin_bytes(
    url: &str,
    progress: Option<std::sync::Arc<JavaProgressCallback>>,
    cancel: Option<Arc<AtomicBool>>,
    major: u32,
) -> Result<Vec<u8>, crate::error::CoreError> {
    use futures_util::StreamExt;
    let client = crate::download::build_client()?;
    let res = client.get(url).send().await?;
    let status = res.status();
    if !status.is_success() {
        return Err(crate::error::CoreError::CdnFailure(format!(
            "Téléchargement Java {major} : HTTP {status} ({url})"
        )));
    }
    let total = res.content_length().unwrap_or(0);
    let mut stream = res.bytes_stream();
    let mut bytes = Vec::with_capacity(total.try_into().unwrap_or(0));
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        if cancel.as_ref().is_some_and(|f| f.load(Ordering::Relaxed)) {
            return Err(crate::error::CoreError::Cancelled(format!(
                "Installation de Java {major} annulée."
            )));
        }
        let chunk = chunk?;
        downloaded += chunk.len() as u64;
        bytes.extend_from_slice(&chunk);
        if let Some(cb) = &progress {
            cb(downloaded, total);
        }
    }
    Ok(bytes)
}

/// Download the Temurin 8 JRE from an explicit URL and install it into
/// `<gameDir>/java` (verified Java 8). Test seam for [`provision_java8`].
/// Download a Temurin JRE from an explicit URL and install it into
/// `<gameDir>/<dir_name>` (verified `major`). Test seam for
/// [`provision_temurin`].
pub async fn provision_temurin_from_url(
    game_dir: &Path,
    major: u32,
    dir_name: &str,
    url: &str,
    progress: Option<std::sync::Arc<JavaProgressCallback>>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, crate::error::CoreError> {
    let bytes = download_temurin_bytes(url, progress, cancel, major).await?;
    if bytes.is_empty() {
        return Err(crate::error::CoreError::CdnFailure(format!(
            "Téléchargement Java {major} : archive vide reçue."
        )));
    }
    // Extraction + chmod + `java -version` check are blocking: off-load so
    // async workers never stall on ~100MB of archive IO.
    let game_dir = game_dir.to_path_buf();
    let dir_name = dir_name.to_string();
    tokio::task::spawn_blocking(move || {
        install_java_from_bytes_in(&game_dir, &bytes, &dir_name, major)
    })
    .await?
}

/// Download the Temurin 8 JRE from an explicit URL and install it into
/// `<gameDir>/java` (verified Java 8). Test seam for [`provision_java8`].
pub async fn provision_java8_from_url(
    game_dir: &Path,
    url: &str,
    progress: Option<std::sync::Arc<JavaProgressCallback>>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, crate::error::CoreError> {
    provision_temurin_from_url(game_dir, 8, "java", url, progress, cancel).await
}

/// Download the Temurin 8 JRE for the current host and install it into
/// `<gameDir>/java` (verified Java 8). Returns the verified binary path.
///
/// Existing `<gameDir>/java` content is replaced. Progress is reported as
/// `(downloaded_bytes, total_bytes)` (`total_bytes == 0` when unknown).
pub async fn provision_java8(
    game_dir: &Path,
    progress: Option<std::sync::Arc<JavaProgressCallback>>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, crate::error::CoreError> {
    provision_java8_from_url(game_dir, &temurin_url_current(), progress, cancel).await
}

/// Download the Temurin JRE for `major` on the current host and install it
/// into `<gameDir>/<dir_name>` (verified). Returns the verified binary path.
///
/// Existing content is replaced. Progress is reported as
/// `(downloaded_bytes, total_bytes)` (`total_bytes == 0` when unknown).
pub async fn provision_temurin(
    game_dir: &Path,
    major: u32,
    dir_name: &str,
    progress: Option<std::sync::Arc<JavaProgressCallback>>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, crate::error::CoreError> {
    provision_temurin_from_url(
        game_dir,
        major,
        dir_name,
        &temurin_url_current_for_major(major),
        progress,
        cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const JAVA8_SAMPLE: &str =
        "openjdk version \"1.8.0_392\"\nOpenJDK Runtime Environment (build 1.8.0_392-xxx)\n";
    const JAVA17_SAMPLE: &str =
        "openjdk version \"17.0.9\" 2023-10-17\nOpenJDK Runtime Environment (build 17.0.9+9)\n";
    const JAVA21_SAMPLE: &str = "openjdk version \"21.0.1\" 2023-10-17 LTS\n";

    #[test]
    fn parses_legacy_and_modern_versions() {
        assert_eq!(parse_java_major(JAVA8_SAMPLE), Some(8));
        assert_eq!(parse_java_major(JAVA17_SAMPLE), Some(17));
        assert_eq!(parse_java_major(JAVA21_SAMPLE), Some(21));
        assert_eq!(parse_java_major("garbage"), None);
    }

    #[test]
    fn quoteless_leading_lines_are_skipped() {
        // Real `java -version` output starts with an unquoted line; the
        // parser must continue scanning instead of bailing out early.
        let out = "openjdk version\nopenjdk version \"17.0.9\" 2023-10-17\n";
        assert_eq!(parse_java_major(out), Some(17));
        let out8 = "pickled peppers\njava version \"1.8.0_392\"\n";
        assert_eq!(parse_java_major(out8), Some(8));
        // Unterminated quote on the first line must not abort the scan either.
        let unterminated = "broken \"line\nopenjdk version \"21.0.1\" 2023-10-17\n";
        assert_eq!(parse_java_major(unterminated), Some(21));
    }

    #[test]
    fn version_warnings_cover_java8_modern_and_unknown() {
        assert_eq!(java_version_warning(Some(8)), None);
        assert!(java_version_warning(Some(17)).is_some());
        assert!(java_version_warning(Some(11)).is_some());
        assert!(java_version_warning(None).is_some());
    }

    #[test]
    fn java8_and_modern_classifiers() {
        assert!(is_java8_output(JAVA8_SAMPLE));
        assert!(!is_java8_output(JAVA17_SAMPLE));
        assert!(!is_modern_java_output(JAVA8_SAMPLE));
        assert!(is_modern_java_output(JAVA17_SAMPLE));
        assert!(is_modern_java_output(JAVA21_SAMPLE));
    }

    #[test]
    fn bundled_takes_precedence_over_java_home() {
        let game = tempfile::tempdir().unwrap();
        let bundled = bundled_java_bin_for(game.path(), Os::Linux);
        std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        std::fs::write(&bundled, b"fake").unwrap();

        let home = tempfile::tempdir().unwrap();
        let home_java = home.path().join("bin").join("java");
        std::fs::create_dir_all(home_java.parent().unwrap()).unwrap();
        std::fs::write(&home_java, b"fake").unwrap();

        let resolved = resolve_java_with_home(game.path(), Os::Linux, Some(home.path()));
        assert_eq!(resolved, bundled);
    }

    #[test]
    fn falls_back_to_java_home_then_path() {
        // No bundled runtime in a fresh temp dir.
        let game = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let home_java = home.path().join("bin").join("java");
        std::fs::create_dir_all(home_java.parent().unwrap()).unwrap();
        std::fs::write(&home_java, b"fake").unwrap();

        let via_home = resolve_java_with_home(game.path(), Os::Linux, Some(home.path()));

        // JAVA_HOME without bin/java: PATH fallback is bare "java".
        let empty = tempfile::tempdir().unwrap();
        let via_path = resolve_java_with_home(game.path(), Os::Linux, Some(empty.path()));
        let via_none = resolve_java_with_home(game.path(), Os::Linux, None);

        assert_eq!(via_home, home_java);
        assert_eq!(via_path, PathBuf::from("java"));
        assert_eq!(via_none, PathBuf::from("java"));
    }

    #[test]
    fn temurin_url_mapping_covers_desktop_targets() {
        let base = "https://api.adoptium.net/v3/binary/latest/8/ga";
        assert_eq!(
            temurin_url_for("windows", "x86_64"),
            format!("{base}/windows/x64/jre/hotspot/normal/eclipse")
        );
        assert_eq!(
            temurin_url_for("linux", "x86_64"),
            format!("{base}/linux/x64/jre/hotspot/normal/eclipse")
        );
        assert_eq!(
            temurin_url_for("linux", "aarch64"),
            format!("{base}/linux/aarch64/jre/hotspot/normal/eclipse")
        );
        assert_eq!(
            temurin_url_for("macos", "x86_64"),
            format!("{base}/mac/x64/jre/hotspot/normal/eclipse")
        );
        assert_eq!(
            temurin_url_for("macos", "aarch64"),
            format!("{base}/mac/aarch64/jre/hotspot/normal/eclipse")
        );
        // Aliases + unknown fallback (still a usable Temurin URL).
        assert!(
            temurin_url_for("macos", "arm64").ends_with("/mac/aarch64/jre/hotspot/normal/eclipse")
        );
        // Feature-release URLs share the layout with a different base.
        assert_eq!(
            temurin_url_for_major(21, "linux", "x86_64"),
            "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jre/hotspot/normal/eclipse"
        );
        assert_eq!(
            temurin_url_for_major(21, "windows", "x86_64"),
            "https://api.adoptium.net/v3/binary/latest/21/ga/windows/x64/jre/hotspot/normal/eclipse"
        );
        assert!(
            temurin_url_for("linux", "riscv64").ends_with("/linux/x64/jre/hotspot/normal/eclipse")
        );
        assert_eq!(temurin_archive_kind_for("windows"), "zip");
        assert_eq!(temurin_archive_kind_for("linux"), "tar.gz");
        assert_eq!(temurin_archive_kind_for("macos"), "tar.gz");
    }

    #[test]
    fn strip_top_level_drops_single_root_and_rejects_escapes() {
        assert_eq!(
            strip_top_level(Path::new("jdk8u392-b08-jre/bin/java")),
            Some(PathBuf::from("bin/java"))
        );
        assert_eq!(
            strip_top_level(Path::new("top/nested/dir/file.so")),
            Some(PathBuf::from("nested/dir/file.so"))
        );
        // Top-level entry itself: nothing to install.
        assert_eq!(strip_top_level(Path::new("jdk8u392-b08-jre")), None);
        assert_eq!(strip_top_level(Path::new("jdk8u392-b08-jre/")), None);
        // Absolute paths and escapes are refused.
        assert_eq!(strip_top_level(Path::new("/etc/passwd")), None);
        assert_eq!(strip_top_level(Path::new("top/../../evil")), None);
        // macOS archives: `Contents/Home` nesting is flattened so the
        // provisioned `bin/java` matches `bundled_java_bin` on every OS.
        assert_eq!(
            strip_top_level(Path::new("jdk8u392-b08-jre/Contents/Home/bin/java")),
            Some(PathBuf::from("bin/java"))
        );
        assert_eq!(
            strip_top_level(Path::new(
                "graalvm-jdk-21/Contents/Home/lib/server/libjvm.dylib"
            )),
            Some(PathBuf::from("lib/server/libjvm.dylib"))
        );
        // A bare `Contents/Home` top level (without payload) installs nothing.
        assert_eq!(strip_top_level(Path::new("top/Contents/Home")), None);
        // A `Contents` dir that is NOT followed by `Home` is kept verbatim.
        assert_eq!(
            strip_top_level(Path::new("top/Contents/Info.plist")),
            Some(PathBuf::from("Contents/Info.plist"))
        );
    }

    fn sample_zip_bytes() -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        // Top-level folder + payload files (mirrors a Temurin JRE layout).
        writer.add_directory("jdk8u392-b08-jre/", options).unwrap();
        writer
            .add_directory("jdk8u392-b08-jre/bin/", options)
            .unwrap();
        writer
            .start_file("jdk8u392-b08-jre/bin/java", options)
            .unwrap();
        std::io::Write::write_all(&mut writer, b"fake-java").unwrap();
        writer
            .start_file("jdk8u392-b08-jre/release", options)
            .unwrap();
        std::io::Write::write_all(&mut writer, b"JAVA_VERSION=\"1.8.0_392\"").unwrap();
        // Absolute-path smuggling attempt: must be skipped on extract.
        writer.start_file("/evil.txt", options).unwrap();
        std::io::Write::write_all(&mut writer, b"evil").unwrap();
        writer.finish().unwrap();
        buf.into_inner()
    }

    fn sample_tar_gz_bytes() -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "jdk8u392-b08-jre/bin", std::io::empty())
                .unwrap();
            let payload: &[u8] = b"fake-java";
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "jdk8u392-b08-jre/bin/java", payload)
                .unwrap();
            let enc = builder.into_inner().unwrap();
            enc.finish().unwrap();
        }
        buf
    }

    #[test]
    fn zip_extraction_strips_top_level_folder() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        extract_java_archive(&sample_zip_bytes(), &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("bin/java")).unwrap(), b"fake-java");
        assert!(dest.join("release").is_file());
        // No top-level folder kept, no smuggled absolute path.
        assert!(!dest.join("jdk8u392-b08-jre").exists());
        assert!(!dest.join("evil.txt").exists());
    }

    #[test]
    fn tar_gz_extraction_strips_top_level_folder() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        extract_java_archive(&sample_tar_gz_bytes(), &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("bin/java")).unwrap(), b"fake-java");
        assert!(!dest.join("jdk8u392-b08-jre").exists());
    }

    #[test]
    fn garbage_bytes_are_rejected_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let err = extract_java_archive(b"not an archive", &dir.path().join("out")).unwrap_err();
        assert!(
            err.to_string().contains("ni zip ni tar.gz"),
            "unexpected: {err}"
        );
    }

    #[tokio::test]
    async fn temurin_download_reports_byte_progress_against_stub() {
        // `download_java8`/`download_graalvm21` bridge these exact
        // `(downloaded, total)` pairs onto `java-progress` events — if no
        // callbacks fire here, the frontend bar can never move.
        use std::sync::{Arc, Mutex};
        let body = vec![0x7Au8; 96 * 1024];
        let bodies = [("/jre.bin".to_string(), body.clone())]
            .into_iter()
            .collect();
        let (base, srv) = crate::download::tests::stub_server(bodies).await;
        let seen: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let probe = seen.clone();
        let cb = Arc::new(move |downloaded: u64, total: u64| {
            probe.lock().unwrap().push((downloaded, total));
        });
        let out = download_temurin_bytes(&format!("{base}/jre.bin"), Some(cb), None, 8)
            .await
            .expect("stub download succeeds");
        assert_eq!(out, body);
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "progress callbacks must fire per chunk");
        assert!(
            seen.iter().all(|(_, total)| *total == body.len() as u64),
            "total comes from Content-Length, got {seen:?}"
        );
        let (last_dl, _) = seen.last().unwrap();
        assert_eq!(*last_dl, body.len() as u64);
        // Monotonic byte counters (the payload layer clamps to 0..1).
        let mut last = 0u64;
        for (dl, _) in seen.iter() {
            assert!(*dl >= last, "counters must not go backwards: {seen:?}");
            last = *dl;
        }
        srv.abort();
    }

    #[tokio::test]
    async fn provision_from_unreachable_url_fails_fast_without_network() {
        // Loopback port that refuses connections: no external network touched.
        let dir = tempfile::tempdir().unwrap();
        let err =
            provision_java8_from_url(dir.path(), "http://127.0.0.1:9/java8.tar.gz", None, None)
                .await
                .unwrap_err();
        assert!(!err.to_string().is_empty());
        // Nothing half-installed left behind.
        assert!(!dir.path().join("java").exists());
    }

    #[test]
    fn java8_status_probes_resolved_binary_without_network() {
        // Empty temp dir, no network: resolves either to bare "java" (PATH
        // probe, best effort — machines without a system JDK) or to a real
        // absolute java binary (e.g. CI runners shipping Temurin under
        // /usr/lib/jvm). Both are valid resolutions; assert the shape, and
        // keep the is_java8/major consistency invariant in both cases.
        let dir = tempfile::tempdir().unwrap();
        let status = java8_status(dir.path());
        if status.path.as_os_str() == "java" {
            // Fallback: nothing to check beyond consistency below.
        } else {
            assert!(
                status
                    .path
                    .file_name()
                    .is_some_and(|n| n == "java" || n == "java.exe"),
                "resolved java must be a java binary: {}",
                status.path.display()
            );
        }
        assert_eq!(status.is_java8, status.major == Some(8));
    }
}
