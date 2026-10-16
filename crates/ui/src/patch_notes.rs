// SPDX-License-Identifier: AGPL-3.0-only

//! Patch-notes feed: local `CHANGELOG.md` + installed-version summary +
//! best-effort GitHub COMMITS across the `46Anarchy` organisation
//! (`46Launcher`, `FS-Patches`, `SafeRespawn`, `46Core`,
//! `cdn.46anarchy.fr`, `46Anarchy.fr`).
//!
//! Never errors: GitHub failures degrade to local-only entries (with
//! serve-stale-on-failure while the in-memory cache holds data), and an
//! empty feed degrades to a single honest `"Aucune note"` info entry.
//! The currently-PRIVATE `46Launcher` repo answers `401`/`404`: it is
//! skipped silently (no error, no retry storm) and joins the feed on its
//! own once public.

use std::path::Path;
use std::time::Duration;

use launcher_core::Manifest;

use crate::state::PatchNoteDto;

/// Commit sources: full `owner/repo` (API path) + short repo tag shown on
/// each card (`PatchNoteDto.version`, surfaced by the frontend subtitle).
pub const PATCH_COMMIT_REPOS: &[(&str, &str)] = &[
    ("46Anarchy/46Launcher", "46Launcher"),
    ("46Anarchy/FS-Patches", "FS-Patches"),
    ("46Anarchy/SafeRespawn", "SafeRespawn"),
    ("46Anarchy/46Core", "46Core"),
    ("46Anarchy/cdn.46anarchy.fr", "cdn.46anarchy.fr"),
    ("46Anarchy/46Anarchy.fr", "46Anarchy.fr"),
];
/// Commits read per repo (the global date sort + [`MAX_PATCH_NOTES`] bound
/// the feed afterwards). 3 is enough: 6 repos × 3 = 18 candidates merged
/// down to a top-5, so smaller pages cut GitHub payload without ever
/// starving the merge.
pub const PATCH_COMMITS_PER_REPO: usize = 3;
/// In-memory cache lifetime (stale entries are still served on failure).
pub const PATCH_COMMITS_TTL: Duration = Duration::from_secs(15 * 60);
/// `User-Agent` header (required by the GitHub API).
pub const PATCH_USER_AGENT: &str = "46anarchy-launcher";

/// Commits endpoint for one repo (best-effort, short timeout).
pub fn commits_url(repo_full: &str) -> String {
    format!("https://api.github.com/repos/{repo_full}/commits?per_page={PATCH_COMMITS_PER_REPO}")
}

/// `true` for the statuses a still-private repo answers without auth
/// (`401`/`404` → skip silently, no error, no retry storm).
pub fn commit_repo_private(status: u16) -> bool {
    status == 401 || status == 404
}
/// Changelog file name looked up inside the game dir.
pub const CHANGELOG_FILE_NAME: &str = "CHANGELOG.md";
/// Request timeout for one GitHub best-effort fetch: 10 s — cold release
/// builds lose the TLS + response race to api.github.com well past 3 s, so
/// the old aggressive value failed all 6 repos into the silent empty state.
/// The 6 repos stay CONCURRENT (see [`remote_release_entries`]), and a
/// single retry on timeout-only failures absorbs one slow handshake without
/// hammering rate limits. Other failures (4xx — a still-private repo stays
/// skipped — other HTTP, bad body) never retry. Total failure degrades to
/// the 15-min cache / stale fallback plus a visible UI hint, never to an
/// error.
pub const PATCH_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Total attempts per repo fetch: initial try + ONE retry on
/// timeout/elapsed errors only (see [`fetch_with_retry`]).
pub const PATCH_FETCH_MAX_ATTEMPTS: u32 = 2;
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

/// Parse a GitHub commits JSON array for one repo (fixture-testable, no
/// network). Each item becomes one entry: repo short tag (`version`, shown
/// on the card), first message line (`title`), `YYYY-MM-DD` (`date`), and
/// the remaining message lines + commit URL (`body`).
pub fn parse_github_commits_json(json: &str, repo_short: &str) -> Vec<PatchNoteDto> {
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
        let commit = match item.get("commit") {
            Some(c) => c,
            None => continue,
        };
        let message = commit
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .trim();
        let stamped = commit
            .get("author")
            .and_then(|a| a.get("date"))
            .and_then(|d| d.as_str())
            .or_else(|| {
                commit
                    .get("committer")
                    .and_then(|c| c.get("date"))
                    .and_then(|d| d.as_str())
            })
            .unwrap_or("");
        let date = if stamped.len() >= 10 {
            stamped[..10].to_string()
        } else {
            String::new()
        };
        let url = item
            .get("html_url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let mut lines = message.lines();
        let first = lines.next().unwrap_or("").trim();
        let title = if first.is_empty() {
            // Message vide : le SHA court reste un titre honnête.
            let sha = item.get("sha").and_then(|s| s.as_str()).unwrap_or("");
            if sha.len() >= 7 {
                format!("Commit {}", &sha[..7])
            } else {
                "Commit".to_string()
            }
        } else {
            first.to_string()
        };
        let rest = lines.collect::<Vec<_>>().join("\n").trim().to_string();
        let body = match (rest.is_empty(), url.is_empty()) {
            (true, true) => String::new(),
            (false, true) => rest,
            (true, false) => url,
            (false, false) => format!("{rest}\n{url}"),
        };
        let kind = infer_kind(&title, &body);
        out.push(PatchNoteDto {
            version: repo_short.to_string(),
            date,
            title,
            body,
            kind,
        });
    }
    out
}

/// Merge commit entries from every repo: commit-date descending (stable —
/// ties keep the [`PATCH_COMMIT_REPOS`] order), max 5. Pure (unit-tested).
pub fn merge_commit_notes(mut notes: Vec<PatchNoteDto>) -> Vec<PatchNoteDto> {
    notes.sort_by(|a, b| b.date.cmp(&a.date));
    notes.truncate(MAX_PATCH_NOTES);
    // Newest entry is the feed highlight unless it's a hotfix.
    if let Some(first) = notes.first_mut() {
        if first.kind == "info" {
            first.kind = "major".to_string();
        }
    }
    notes
}

/// Total-failure verdict (pure, unit-tested): `true` when the fresh fetch
/// yielded nothing across all repos — the UI then shows the muted offline
/// hint (the stale cache is still served when present, see below).
pub fn remote_fetch_totally_failed(fresh: &[PatchNoteDto]) -> bool {
    fresh.is_empty()
}

/// Serve-stale selection (pure, unit-tested): a fresh fetch wins, even a
/// partial one; only a total failure falls back to the stale cache.
fn select_commit_notes(fresh: Vec<PatchNoteDto>, stale: Vec<PatchNoteDto>) -> Vec<PatchNoteDto> {
    if fresh.is_empty() {
        stale
    } else {
        fresh
    }
}

type CommitNotesCacheEntry = Option<(std::time::Instant, Vec<PatchNoteDto>)>;
type CommitNotesCache = std::sync::OnceLock<std::sync::Mutex<CommitNotesCacheEntry>>;

static COMMIT_NOTES_CACHE: CommitNotesCache = std::sync::OnceLock::new();

fn commit_cache() -> &'static std::sync::Mutex<CommitNotesCacheEntry> {
    COMMIT_NOTES_CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Fresh cached feed (`None` when absent or older than [`PATCH_COMMITS_TTL`]).
fn cached_commit_notes() -> Option<Vec<PatchNoteDto>> {
    let guard = commit_cache().lock().ok()?;
    let (at, notes) = guard.as_ref()?;
    if at.elapsed() < PATCH_COMMITS_TTL {
        Some(notes.clone())
    } else {
        None
    }
}

/// Last fetched feed, however old (serve-stale-on-failure).
fn stale_commit_notes() -> Vec<PatchNoteDto> {
    commit_cache()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|(_, notes)| notes.clone()))
        .unwrap_or_default()
}

fn store_commit_notes(notes: &[PatchNoteDto]) {
    if let Ok(mut guard) = commit_cache().lock() {
        *guard = Some((std::time::Instant::now(), notes.to_vec()));
    }
}

/// Vide le cache en mémoire (tests uniquement : le statique est partagé
/// par tout le binaire de test, chaque test hermétique repart donc d'un
/// état connu).
#[cfg(test)]
fn clear_commit_cache_for_tests() {
    if let Ok(mut guard) = commit_cache().lock() {
        *guard = None;
    }
}

/// Outcome of one fetch attempt (retry driver input, network-free itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchAttempt<T> {
    /// Attempt resolved (success or definitive skip): keep, stop retrying.
    Done(T),
    /// Transient timeout/elapsed failure: retry once when attempts remain.
    Retryable,
    /// Definitive failure (private-repo 4xx, other HTTP status, bad body):
    /// keep the failure, never retry (a 4xx replayed stays a 4xx).
    Failed,
}

/// Retry driver: runs `once` up to [`PATCH_FETCH_MAX_ATTEMPTS`] times,
/// retrying ONLY [`FetchAttempt::Retryable`] outcomes (timeout/elapsed).
/// The closure is injected, so the loop is unit-testable without network
/// (counter closures below prove: one retry on timeout, none on final
/// failure, at most one retry on persistent timeouts).
pub async fn fetch_with_retry<F, Fut, T>(mut once: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = FetchAttempt<T>>,
{
    for _ in 0..PATCH_FETCH_MAX_ATTEMPTS {
        match once().await {
            FetchAttempt::Done(v) => return Some(v),
            FetchAttempt::Failed => return None,
            FetchAttempt::Retryable => {}
        }
    }
    None
}

/// Fetch one repo's commits (`None` = skip: private repo, timeout, HTTP
/// error — never a hard failure). Timeout/elapsed failures replay ONCE;
/// anything else (private 4xx, other HTTP, bad body) never retries.
async fn fetch_repo_commits(
    client: &reqwest::Client,
    repo_full: &str,
    repo_short: &str,
) -> Option<Vec<PatchNoteDto>> {
    fetch_with_retry(|| async {
        let fut = client
            .get(commits_url(repo_full))
            .header("Accept", "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, PATCH_USER_AGENT)
            .send();
        let res = match tokio::time::timeout(PATCH_FETCH_TIMEOUT, fut).await {
            Ok(Ok(r)) => r,
            // Seul le timeout (réseau lent, poignée TLS froide) rejoue : un
            // 4xx rejoué resterait un 4xx, et une erreur d'envoi franche
            // (DNS…) ne guérira pas en une seconde tentative.
            Ok(Err(e)) if e.is_timeout() => return FetchAttempt::Retryable,
            Ok(Err(_)) => return FetchAttempt::Failed,
            Err(_) => return FetchAttempt::Retryable,
        };
        if commit_repo_private(res.status().as_u16()) {
            // Dépôt encore privé : ignoré silencieusement — il rejoint le flux
            // automatiquement une fois public (jamais réessayé).
            return FetchAttempt::Failed;
        }
        if !res.status().is_success() {
            return FetchAttempt::Failed;
        }
        let text = match tokio::time::timeout(PATCH_FETCH_TIMEOUT, res.text()).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) if e.is_timeout() => return FetchAttempt::Retryable,
            Ok(Err(_)) => return FetchAttempt::Failed,
            Err(_) => return FetchAttempt::Retryable,
        };
        FetchAttempt::Done(parse_github_commits_json(&text, repo_short))
    })
    .await
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

/// Merge per-repo feeds into one date-ordered feed (pure, unit-tested):
/// flatten in [`PATCH_COMMIT_REPOS`] order (stable ties keep that order),
/// then the shared date-desc + top-5 bound. The concurrent fetch below only
/// changes arrival order — this function restores determinism.
pub fn merge_repo_feeds(feeds: Vec<Vec<PatchNoteDto>>) -> Vec<PatchNoteDto> {
    let all: Vec<PatchNoteDto> = feeds.into_iter().flatten().collect();
    merge_commit_notes(all)
}

/// Best-effort GitHub fetch (10 s timeout + one retry on timeout-only
/// failures; other failures → empty or stale, never errors). The 6 repos
/// are fetched CONCURRENTLY (`tokio::join!`, one
/// [`PATCH_FETCH_TIMEOUT`] each), then merged by [`merge_repo_feeds`].
/// Returns the merged feed plus the total-failure verdict (`true` when the
/// fresh fetch yielded nothing across all 6 repos — even with stale cache
/// re-served): [`crate::commands::get_patch_notes`] forwards it so the UI
/// shows the muted offline hint instead of a silent empty state.
pub async fn remote_release_entries() -> (Vec<PatchNoteDto>, bool) {
    match launcher_core::build_client() {
        Ok(client) => {
            fetch_remote_notes(|full, short| fetch_repo_commits(&client, full, short)).await
        }
        // Sans client HTTP, aucun dépôt n'est joignable : même issue
        // qu'un échec total (périmé resservi, indice affiché).
        Err(_) => (select_commit_notes(Vec::new(), stale_commit_notes()), true),
    }
}

/// Corps testable de [`remote_release_entries`] : le fetch par dépôt est
/// injecté (réseau réel en production, stub JSON en test — hermétique,
/// sans réseau). Sémantique inchangée : cache frais servi d'abord, sinon
/// fetch concurrent des 6 dépôts, fusion, stockage, repli périmé.
async fn fetch_remote_notes<F, Fut>(fetch_one: F) -> (Vec<PatchNoteDto>, bool)
where
    F: Fn(&'static str, &'static str) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<PatchNoteDto>>>,
{
    // Cache frais (< 15 min) : servi aussitôt, sans réseau — c'est ce
    // chemin qu'emprunte chaque retour sur l'accueil. Le verrou n'est tenu
    // que le temps de cloner (jamais pendant le réseau :
    // cloner-puis-relâcher, calculer, puis stocker), donc un second appel
    // ne peut pas rester coincé derrière un fetch en cours.
    if let Some(cached) = cached_commit_notes() {
        return (cached, false);
    }
    let started = std::time::Instant::now();
    // Fixed arity on purpose: `PATCH_COMMIT_REPOS` has exactly 6
    // entries, so `join!` needs no extra dependency and every repo is
    // fetched in parallel instead of sequentially.
    let (r0, r1, r2, r3, r4, r5) = tokio::join!(
        fetch_one(PATCH_COMMIT_REPOS[0].0, PATCH_COMMIT_REPOS[0].1),
        fetch_one(PATCH_COMMIT_REPOS[1].0, PATCH_COMMIT_REPOS[1].1),
        fetch_one(PATCH_COMMIT_REPOS[2].0, PATCH_COMMIT_REPOS[2].1),
        fetch_one(PATCH_COMMIT_REPOS[3].0, PATCH_COMMIT_REPOS[3].1),
        fetch_one(PATCH_COMMIT_REPOS[4].0, PATCH_COMMIT_REPOS[4].1),
        fetch_one(PATCH_COMMIT_REPOS[5].0, PATCH_COMMIT_REPOS[5].1),
    );
    let results = [r0, r1, r2, r3, r4, r5];
    let mut feeds: Vec<Vec<PatchNoteDto>> = Vec::with_capacity(results.len());
    for (i, res) in results.into_iter().enumerate() {
        match res {
            Some(notes) => feeds.push(notes),
            None => {
                let full = PATCH_COMMIT_REPOS[i].0;
                crate::log_verbose!(
                    "[launcher] notes de patch : dépôt {full} ignoré (privé ou injoignable)."
                );
                feeds.push(Vec::new());
            }
        }
    }
    let all = merge_repo_feeds(feeds);
    crate::log_verbose!(
        "[launcher] notes de patch : {} entrée(s) en {} ms (6 dépôts en parallèle).",
        all.len(),
        started.elapsed().as_millis()
    );
    // `all` is already merged (date-desc + top-5) by `merge_repo_feeds`.
    let merged = all;
    if merged.is_empty() {
        // Échec total (hors-ligne, quota…) : resservir le cache périmé
        // plutôt que de casser l'interface — vide quand rien n'a jamais
        // été lu — ET signaler l'échec (`true`) pour l'indice visible.
        // Le périmé n'est jamais ré-estampillé : l'appel suivant retente le
        // réseau aussitôt.
        let failed = remote_fetch_totally_failed(&merged);
        return (
            select_commit_notes(Vec::new(), stale_commit_notes()),
            failed,
        );
    }
    store_commit_notes(&merged);
    (merged, false)
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

    const COMMITS_FIXTURE: &str = r#"[
        {
            "sha": "abc1234567890def",
            "html_url": "https://github.com/46Anarchy/FS-Patches/commit/abc1234",
            "commit": {
                "message": "Saison 2 — Nouveau monde\n\nAjout des minerais légendaires.\n- Nouveau spawn\n- Équilibrage PvP",
                "author": { "date": "2026-03-01T10:00:00Z" },
                "committer": { "date": "2026-03-01T10:05:00Z" }
            }
        },
        {
            "sha": "def5678901234abc",
            "html_url": "https://github.com/46Anarchy/FS-Patches/commit/def5678",
            "commit": {
                "message": "Hotfix coffres\nCorrectif dupliquation des coffres.",
                "author": { "date": "2026-02-10T08:30:00Z" },
                "committer": { "date": "2026-02-10T08:31:00Z" }
            }
        },
        {
            "sha": "0011223344556677",
            "html_url": "https://github.com/46Anarchy/FS-Patches/commit/0011223",
            "commit": {
                "message": "",
                "author": { "date": "2026-01-05T00:00:00Z" },
                "committer": { "date": "2026-01-05T00:00:00Z" }
            }
        }
    ]"#;

    #[test]
    fn parses_github_commits_fixture() {
        let notes = parse_github_commits_json(COMMITS_FIXTURE, "FS-Patches");
        assert_eq!(notes.len(), 3);
        // Titre = première ligne, date tronquée, tag dépôt, URL dans le corps.
        assert_eq!(notes[0].version, "FS-Patches");
        assert_eq!(notes[0].title, "Saison 2 — Nouveau monde");
        assert_eq!(notes[0].date, "2026-03-01");
        assert!(notes[0].body.contains("minerais"));
        assert!(notes[0]
            .body
            .contains("https://github.com/46Anarchy/FS-Patches/commit/abc1234"));
        // `correctif` → hotfix, comme l'ancien flux releases.
        assert_eq!(notes[1].kind, "hotfix");
        assert_eq!(notes[1].title, "Hotfix coffres");
        // Message vide → titre SHA court honnête, jamais vide.
        assert_eq!(notes[2].title, "Commit 0011223");
        // JSON illisible → vide, jamais d'erreur.
        assert!(parse_github_commits_json("pas du json", "X").is_empty());
        assert!(parse_github_commits_json("{}", "X").is_empty());
    }

    #[test]
    fn private_repo_statuses_skip_silently() {
        assert!(commit_repo_private(401));
        assert!(commit_repo_private(404));
        assert!(!commit_repo_private(200));
        assert!(!commit_repo_private(403));
        assert!(!commit_repo_private(500));
    }

    #[test]
    fn merge_commit_notes_orders_by_date_desc_and_caps() {
        let mk = |repo: &str, date: &str, title: &str| PatchNoteDto {
            version: repo.to_string(),
            date: date.to_string(),
            title: title.to_string(),
            body: String::new(),
            kind: "info".to_string(),
        };
        // Entrées volontairement mélangées sur deux dépôts.
        let notes = vec![
            mk("SafeRespawn", "2026-01-20", "Respawn"),
            mk("FS-Patches", "2026-03-01", "Saison 2"),
            mk("46Core", "2026-02-10", "Noyau"),
        ];
        let merged = merge_commit_notes(notes);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].title, "Saison 2");
        assert_eq!(merged[1].title, "Noyau");
        assert_eq!(merged[2].title, "Respawn");
        // Le plus récent est mis en avant, comme l'ancien flux.
        assert_eq!(merged[0].kind, "major");
        // Plafond global à 5 sur 8 entrées.
        let many: Vec<PatchNoteDto> = (0..8)
            .map(|i| mk("46Core", &format!("2026-01-0{i}"), &format!("Note {i}")))
            .collect();
        assert_eq!(merge_commit_notes(many).len(), MAX_PATCH_NOTES);
    }

    #[test]
    fn commits_url_uses_small_pages() {
        // 6 dépôts × `per_page` 3 = 18 candidats pour un top-5 fusionné.
        let url = commits_url("46Anarchy/46Core");
        assert!(url.contains("per_page=3"), "unexpected url: {url}");
        assert_eq!(PATCH_COMMITS_PER_REPO, 3);
        assert_eq!(PATCH_COMMIT_REPOS.len(), 6);
    }

    #[test]
    fn merge_repo_feeds_orders_across_repos_and_caps() {
        let mk = |repo: &str, date: &str, title: &str| PatchNoteDto {
            version: repo.to_string(),
            date: date.to_string(),
            title: title.to_string(),
            body: String::new(),
            kind: "info".to_string(),
        };
        // Flux par dépôt, volontairement dans le désordre d'arrivée.
        let feeds = vec![
            vec![mk("46Core", "2026-02-10", "Noyau")],
            vec![
                mk("SafeRespawn", "2026-01-20", "Respawn"),
                mk("SafeRespawn", "2026-03-05", "Respawn 2"),
            ],
            vec![mk("FS-Patches", "2026-03-01", "Saison 2")],
        ];
        let merged = merge_repo_feeds(feeds);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].title, "Respawn 2");
        assert_eq!(merged[1].title, "Saison 2");
        assert_eq!(merged[2].title, "Noyau");
        assert_eq!(merged[3].title, "Respawn");
        assert_eq!(merged[0].kind, "major");
        // Plafond global à 5 sur 8 entrées réparties sur 3 dépôts.
        let many: Vec<Vec<PatchNoteDto>> = (0..8)
            .map(|i| vec![mk("46Core", &format!("2026-01-0{i}"), &format!("Note {i}"))])
            .collect();
        assert_eq!(merge_repo_feeds(many).len(), MAX_PATCH_NOTES);
        // Dépôts vides (privés/ignorés) → vide, jamais d'erreur.
        let empty: Vec<Vec<PatchNoteDto>> = vec![Vec::new(), Vec::new()];
        assert!(merge_repo_feeds(empty).is_empty());
    }

    #[test]
    fn stale_fallback_only_on_total_failure() {
        let stale = vec![PatchNoteDto {
            version: "FS-Patches".to_string(),
            date: "2026-01-01".to_string(),
            title: "Ancien".to_string(),
            body: String::new(),
            kind: "info".to_string(),
        }];
        // Échec total → périmé resservi (l'interface ne casse jamais).
        assert_eq!(select_commit_notes(Vec::new(), stale.clone()), stale);
        // Rien nulle part → vide (le merge ajoute alors « Aucune note »).
        assert!(select_commit_notes(Vec::new(), Vec::new()).is_empty());
        // Frais même partiel → le frais gagne, jamais mélangé.
        let fresh = vec![PatchNoteDto {
            version: "46Core".to_string(),
            date: "2026-03-01".to_string(),
            title: "Frais".to_string(),
            body: String::new(),
            kind: "info".to_string(),
        }];
        assert_eq!(select_commit_notes(fresh.clone(), stale), fresh);
    }

    #[test]
    fn total_failure_verdict_drives_visible_hint() {
        // Frais vide (les 6 dépôts ont échoué) → indice « injoignable »
        // affiché, avec ou sans périmé resservi.
        assert!(remote_fetch_totally_failed(&[]));
        // Frais même partiel → pas d'indice, le frais gagne.
        let fresh = vec![PatchNoteDto {
            version: "46Core".to_string(),
            date: "2026-03-01".to_string(),
            title: "Frais".to_string(),
            body: String::new(),
            kind: "info".to_string(),
        }];
        assert!(!remote_fetch_totally_failed(&fresh));
    }

    #[tokio::test]
    async fn retry_driver_replays_timeouts_once_only() {
        use std::sync::{Arc, Mutex};
        // Timeout simulé : échec rejouable puis succès → 2 appels, succès gardé.
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        let out = fetch_with_retry(|| {
            let c = c.clone();
            async move {
                let mut n = c.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    FetchAttempt::Retryable
                } else {
                    FetchAttempt::Done(42)
                }
            }
        })
        .await;
        assert_eq!(out, Some(42));
        assert_eq!(*calls.lock().unwrap(), 2);
        assert_eq!(PATCH_FETCH_MAX_ATTEMPTS, 2);
        // Échec définitif (4xx, dépôt privé…) : jamais réessayé → 1 seul appel.
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        let out = fetch_with_retry(|| {
            let c = c.clone();
            async move {
                *c.lock().unwrap() += 1;
                FetchAttempt::Failed
            }
        })
        .await;
        assert_eq!(out, None::<i32>);
        assert_eq!(*calls.lock().unwrap(), 1);
        // Timeouts persistants : UN seul réessai → 2 appels, puis abandon.
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        let out = fetch_with_retry(|| {
            let c = c.clone();
            async move {
                *c.lock().unwrap() += 1;
                FetchAttempt::<i32>::Retryable
            }
        })
        .await;
        assert_eq!(out, None);
        assert_eq!(*calls.lock().unwrap(), PATCH_FETCH_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn second_appel_cache_repond_vite_sans_refetch() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        // Retour sur l'accueil : le second appel séquentiel doit sortir du
        // cache, vite et sans retoucher le réseau (garde anti-blocage avec
        // délai : un second fetch ou un verrou coincé ferait expirer le
        // délai au lieu de rendre le flux).
        clear_commit_cache_for_tests();
        // Stub hermétique (jamais de réseau) : la fixture JSON existante,
        // servie pour chaque dépôt, avec compteur d'appels.
        let calls = Arc::new(Mutex::new(0u32));
        let stub = || {
            let calls = calls.clone();
            move |_full: &'static str, _short: &'static str| {
                let calls = calls.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    Some(parse_github_commits_json(COMMITS_FIXTURE, "FS-Patches"))
                }
            }
        };
        // Premier appel : un fetch par dépôt, flux non vide, pas d'indice.
        let (first, failed) = fetch_remote_notes(stub()).await;
        assert!(!failed);
        assert!(!first.is_empty());
        assert_eq!(*calls.lock().unwrap(), PATCH_COMMIT_REPOS.len() as u32);
        // Second appel : contenu identique du cache, sans nouveau fetch.
        let second = tokio::time::timeout(Duration::from_secs(2), fetch_remote_notes(stub()))
            .await
            .expect("second appel : le cache doit répondre vite, sans réseau");
        assert_eq!(second.0, first);
        assert!(!second.1);
        assert_eq!(
            *calls.lock().unwrap(),
            PATCH_COMMIT_REPOS.len() as u32,
            "second appel : aucun nouveau fetch"
        );
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
