// SPDX-License-Identifier: AGPL-3.0-only

//! Patch-notes feed: local `CHANGELOG.md` + installed-version summary +
//! best-effort GitHub releases of `46Anarchy/FS-Patches`.
//!
//! Never errors: GitHub failures degrade to local-only entries, and an empty
//! feed degrades to a single honest `"Aucune note"` info entry.

use std::path::Path;
use std::time::Duration;

use launcher_core::Manifest;

use crate::state::PatchNoteDto;

/// GitHub releases endpoint (best-effort, short timeout).
pub const PATCH_RELEASES_URL: &str =
    "https://api.github.com/repos/46Anarchy/FS-Patches/releases?per_page=5";
/// Changelog file name looked up inside the game dir.
pub const CHANGELOG_FILE_NAME: &str = "CHANGELOG.md";
/// Request timeout for the GitHub best-effort fetch.
pub const PATCH_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum entries returned (newest first).
pub const MAX_PATCH_NOTES: usize = 5;

/// `true` when `kind` is one of the three known values.
pub fn is_known_kind(kind: &str) -> bool {
    matches!(kind, "major" | "hotfix" | "info")
}

/// Infer an entry kind from its title + body (French + English keywords).
pub fn infer_kind(title: &str, body: &str) -> String {
    let hay = format!("{title}\n{body}").to_lowercase();
    if hay.contains("hotfix")
        || hay.contains("hot-fix")
        || hay.contains("correctif")
        || hay.contains("patch correctif")
    {
        "hotfix".to_string()
    } else {
        "info".to_string()
    }
}

/// Extract the first `vX.Y.Z`-ish token from a heading, if any.
fn extract_version(heading: &str) -> Option<String> {
    let mut token: Option<String> = None;
    let mut current = String::new();
    for c in heading.chars() {
        if c.is_ascii_alphanumeric() || c == 'v' || c == 'V' || c == '.' || c == '-' || c == '_' {
            current.push(c);
        } else {
            if !current.is_empty() {
                let t = current
                    .trim_matches(|x| {
                        x == '[' || x == ']' || x == '(' || x == ')' || x == ':' || x == ','
                    })
                    .to_string();
                if looks_like_version(&t) {
                    token = Some(t);
                    break;
                }
                current.clear();
            }
        }
    }
    if token.is_none() && !current.is_empty() {
        let t = current
            .trim_matches(|x| x == '[' || x == ']' || x == '(' || x == ')' || x == ':' || x == ',')
            .to_string();
        if looks_like_version(&t) {
            token = Some(t);
        }
    }
    token
}

fn looks_like_version(token: &str) -> bool {
    let t = token
        .strip_prefix('v')
        .or_else(|| token.strip_prefix('V'))
        .unwrap_or(token);
    let dots = t.chars().filter(|c| *c == '.').count();
    if dots == 0 {
        return false;
    }
    t.chars().all(|c| c.is_ascii_digit() || c == '.') && t.chars().any(|c| c.is_ascii_digit())
}

/// Extract the first `YYYY-MM-DD` date from a heading, if any.
fn extract_date(heading: &str) -> String {
    let bytes = heading.as_bytes();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        let slice = &heading[i..i + 10];
        let b = slice.as_bytes();
        let is_date = b[0].is_ascii_digit()
            && b[1].is_ascii_digit()
            && b[2].is_ascii_digit()
            && b[3].is_ascii_digit()
            && b[4] == b'-'
            && b[5].is_ascii_digit()
            && b[6].is_ascii_digit()
            && b[7] == b'-'
            && b[8].is_ascii_digit()
            && b[9].is_ascii_digit();
        if is_date {
            return slice.to_string();
        }
        i += 1;
    }
    String::new()
}

/// Parse a Markdown changelog (`## ` headings start new entries, newest first
/// as written in the file).
pub fn parse_changelog_markdown(text: &str) -> Vec<PatchNoteDto> {
    let mut entries: Vec<PatchNoteDto> = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_version: Option<String> = None;
    let mut current_date = String::new();
    let mut current_body: Vec<String> = Vec::new();

    let flush = |title: &mut Option<String>,
                 version: &mut Option<String>,
                 date: &mut String,
                 body: &mut Vec<String>,
                 entries: &mut Vec<PatchNoteDto>| {
        if let Some(t) = title.take() {
            let v = version.take().unwrap_or_else(|| t.clone());
            let b = body.join("\n").trim().to_string();
            let kind = infer_kind(&t, &b);
            entries.push(PatchNoteDto {
                version: v,
                date: std::mem::take(date),
                title: t,
                body: b,
                kind,
            });
            body.clear();
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            flush(
                &mut current_title,
                &mut current_version,
                &mut current_date,
                &mut current_body,
                &mut entries,
            );
            let heading = heading.trim().to_string();
            current_version = extract_version(&heading);
            current_date = extract_date(&heading);
            current_title = Some(heading);
        } else if current_title.is_some() {
            current_body.push(line.to_string());
        }
    }
    flush(
        &mut current_title,
        &mut current_version,
        &mut current_date,
        &mut current_body,
        &mut entries,
    );
    // Newest entry (first) is the release highlight unless it's a hotfix.
    if let Some(first) = entries.first_mut() {
        if first.kind == "info" {
            first.kind = "major".to_string();
        }
    }
    entries
}

/// Parse a GitHub releases JSON array (fixture-testable, no network).
/// Drafts are skipped; `published_at` is truncated to `YYYY-MM-DD`.
pub fn parse_github_releases_json(json: &str) -> Vec<PatchNoteDto> {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let items = match value.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in items {
        if item
            .get("draft")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let version = item
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if version.is_empty() {
            continue;
        }
        let title = item
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&version)
            .to_string();
        let body = item
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let published = item
            .get("published_at")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let date = if published.len() >= 10 {
            published[..10].to_string()
        } else {
            String::new()
        };
        let kind = infer_kind(&title, &body);
        out.push(PatchNoteDto {
            version,
            date,
            title,
            body,
            kind,
        });
    }
    if let Some(first) = out.first_mut() {
        if first.kind == "info" {
            first.kind = "major".to_string();
        }
    }
    out
}

/// Count manifest files belonging to a `mods` model (model name or dest).
pub fn mods_count(manifest: &Manifest) -> usize {
    let mods_models: Vec<&str> = manifest
        .models
        .iter()
        .filter(|m| m.name == "mods" || m.dest == "mods")
        .map(|m| m.name.as_str())
        .collect();
    manifest
        .files
        .iter()
        .filter(|f| mods_models.contains(&f.model.as_str()))
        .count()
}

/// Single `"Version installée"` summary entry from the in-memory manifest.
pub fn installed_version_entry(manifest: Option<&Manifest>) -> Option<PatchNoteDto> {
    let m = manifest?;
    let n = mods_count(m);
    let mod_word = if n > 1 { "mods" } else { "mod" };
    Some(PatchNoteDto {
        version: m.version.clone(),
        date: String::new(),
        title: "Version installée".to_string(),
        body: format!(
            "{} • {} {} • Minecraft {}",
            m.name, n, mod_word, m.mc_version
        ),
        kind: "info".to_string(),
    })
}

/// Read + parse `<gameDir>/CHANGELOG.md` (empty when absent/unreadable).
pub fn local_changelog_entries(game_dir: &Path) -> Vec<PatchNoteDto> {
    let path = game_dir.join(CHANGELOG_FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_changelog_markdown(&text),
        Err(_) => Vec::new(),
    }
}

/// Best-effort GitHub fetch (short timeout, failures → empty, never errors).
pub async fn remote_release_entries() -> Vec<PatchNoteDto> {
    let client = match launcher_core::build_client() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let fut = client
        .get(PATCH_RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .send();
    let res = match tokio::time::timeout(PATCH_FETCH_TIMEOUT, fut).await {
        Ok(Ok(r)) => r,
        _ => return Vec::new(),
    };
    if !res.status().is_success() {
        return Vec::new();
    }
    let text = match tokio::time::timeout(PATCH_FETCH_TIMEOUT, res.text()).await {
        Ok(Ok(t)) => t,
        _ => return Vec::new(),
    };
    parse_github_releases_json(&text)
}

/// Merge remote + local + installed entries (max 5, newest first).
pub fn merge_patch_notes(
    mut remote: Vec<PatchNoteDto>,
    mut local: Vec<PatchNoteDto>,
    installed: Option<PatchNoteDto>,
    manifest_version: &str,
) -> Vec<PatchNoteDto> {
    let mut out: Vec<PatchNoteDto> = Vec::new();
    out.append(&mut remote);
    out.append(&mut local);
    if let Some(entry) = installed {
        out.push(entry);
    }
    out.truncate(MAX_PATCH_NOTES);
    if out.is_empty() {
        let version = if manifest_version.trim().is_empty() {
            "inconnue".to_string()
        } else {
            manifest_version.to_string()
        };
        out.push(PatchNoteDto {
            version: version.clone(),
            date: String::new(),
            title: format!("Aucune note — version {version}"),
            body: "Les notes de patch seront affichées ici dès qu'elles seront disponibles."
                .to_string(),
            kind: "info".to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASES_FIXTURE: &str = r#"[
        {
            "tag_name": "v1.2.0",
            "name": "Saison 2 — Nouveau monde",
            "body": "Ajout des minerais légendaires.\n- Nouveau spawn\n- Équilibrage PvP",
            "published_at": "2026-03-01T10:00:00Z",
            "draft": false,
            "prerelease": false
        },
        {
            "tag_name": "v1.1.1",
            "name": "Hotfix coffres",
            "body": "Correctif dupliquation des coffres.",
            "published_at": "2026-02-10T08:30:00Z",
            "draft": false,
            "prerelease": false
        },
        {
            "tag_name": "v1.1.0-draft",
            "name": "Brouillon ignoré",
            "body": "Ne doit jamais apparaître.",
            "published_at": "2026-02-01T00:00:00Z",
            "draft": true,
            "prerelease": false
        }
    ]"#;

    #[test]
    fn parses_github_releases_fixture() {
        let notes = parse_github_releases_json(RELEASES_FIXTURE);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].version, "v1.2.0");
        assert_eq!(notes[0].date, "2026-03-01");
        assert_eq!(notes[0].kind, "major");
        assert!(notes[0].body.contains("minerais"));
        assert_eq!(notes[1].version, "v1.1.1");
        assert_eq!(notes[1].kind, "hotfix");
    }

    #[test]
    fn parses_changelog_fixture() {
        let md = "# Changelog\n\n## [v1.2.0] - 2026-03-01\n\n- Nouveau spawn\n- Équilibrage\n\n## v1.1.0 - 2026-01-15\n\nPremière version.\n";
        let notes = parse_changelog_markdown(md);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].version, "v1.2.0");
        assert_eq!(notes[0].date, "2026-03-01");
        assert_eq!(notes[0].kind, "major");
        assert!(notes[0].body.contains("spawn"));
        assert_eq!(notes[1].version, "v1.1.0");
    }

    #[test]
    fn merge_caps_at_five_and_falls_back_when_empty() {
        let many: Vec<PatchNoteDto> = (0..8)
            .map(|i| PatchNoteDto {
                version: format!("v{i}"),
                date: String::new(),
                title: format!("Note {i}"),
                body: String::new(),
                kind: "info".to_string(),
            })
            .collect();
        let merged = merge_patch_notes(many, Vec::new(), None, "v9");
        assert_eq!(merged.len(), MAX_PATCH_NOTES);
        let empty = merge_patch_notes(Vec::new(), Vec::new(), None, "v1.0.0");
        assert_eq!(empty.len(), 1);
        assert!(empty[0].title.contains("v1.0.0"));
        assert_eq!(empty[0].kind, "info");
    }

    #[test]
    fn infer_kind_detects_hotfix_keywords() {
        assert_eq!(infer_kind("Hotfix coffres", ""), "hotfix");
        assert_eq!(infer_kind("Correctif dupplication", ""), "hotfix");
        assert_eq!(infer_kind("Saison 2", "gros contenu"), "info");
    }
}
