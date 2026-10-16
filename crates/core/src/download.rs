// SPDX-License-Identifier: AGPL-3.0-only

//! Parallel CDN downloader.
//!
//! Guarantees, per file:
//! - up-to-date files (size + SHA1 match) are skipped without any request;
//! - downloads stream to `<dest>.tmp` and are atomically renamed on success;
//! - SHA1 is verified before the rename; mismatch triggers a retry;
//! - up to `max_retries` attempts (default 5) with exponential backoff;
//! - at most `max_concurrency` concurrent downloads (default 6, clamped 1–8);
//! - progress is reported over an optional `tokio::sync::mpsc` channel as
//!   `(completed_files/total_files)` plus run-wide byte counters.
//!
//! Modèle de progression (FICHIERS, pas de phases) :
//! - `completed_bytes` compte TOUS les octets réglés du run, y compris le
//!   morceau en vol : chaque morceau reçu l'incrémente aussitôt, donc la
//!   barre grimpe PENDANT un gros fichier au lieu de sauter à sa fin ;
//! - chaque flux émet au plus un événement par ~150 ms ou par 256 Kio
//!   (le premier des deux), pour ne pas inonder le canal à 6 flux parallèles ;
//! - la fin de fichier émet toujours (compteurs exacts, `current_bytes` à 0) ;
//! - une tentative échouée retire ses octets du compteur avant retry, pour
//!   ne jamais compter deux fois les mêmes octets.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, Semaphore};

use crate::error::{CoreError, Result};
use crate::hash::hash_file;
use crate::integrity::PlannedFile;

/// Default parallelism (within the required 4–8 window).
pub const DEFAULT_MAX_CONCURRENCY: usize = 6;
/// Default per-file retry budget.
pub const DEFAULT_MAX_RETRIES: u32 = 5;
/// Per-stream progress throttle: at most one event per interval…
pub const CHUNK_EMIT_INTERVAL_MS: u64 = 150;
/// …or per streamed bytes, whichever comes first (no channel flood).
pub const CHUNK_EMIT_MIN_BYTES: u64 = 256 * 1024;

/// Non-blocking cancel check (shared `Annuler` flag).
pub fn is_cancelled(cancel: &Option<Arc<AtomicBool>>) -> bool {
    cancel.as_ref().is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Future resolving once the shared flag is set (never resolves without one).
async fn wait_cancelled(cancel: Option<Arc<AtomicBool>>) {
    if let Some(flag) = cancel {
        while !flag.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    } else {
        std::future::pending::<()>().await;
    }
}

/// Progress snapshot sent over the channel (throttled per chunk, exact per file).
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    /// Files finished so far (downloaded + skipped).
    pub completed_files: usize,
    /// Total files in this run.
    pub total_files: usize,
    /// Bytes settled so far across the run, IN-FLIGHT CHUNK INCLUDED: every
    /// received chunk increments this immediately (concurrent streams share
    /// the counter), so `completed_bytes / total_bytes` climbs during a big
    /// file. A finished file contributes exactly its `size` (streamed chunks
    /// for downloads, one credit for skips — never both).
    pub completed_bytes: u64,
    /// Sum of `size` for all files.
    pub total_bytes: u64,
    /// REAL name of the file this update refers to (never a phase word).
    pub current_file: String,
    /// Bytes written so far for `current_file` (this stream only).
    pub current_bytes: u64,
    /// Expected bytes for `current_file` (manifest `size`).
    pub current_total: u64,
    /// `true` when this update closes a file already fresh on disk
    /// (hash-check skip): its bytes were just credited and the UI line reads
    /// `Vérification i/n — name` instead of a download line.
    pub skipped: bool,
}

/// Totals returned when every file resolved successfully.
#[derive(Debug, Clone, Default)]
pub struct DownloadSummary {
    /// Files in this run.
    pub total_files: usize,
    /// Files actually fetched from the CDN.
    pub downloaded_files: usize,
    /// Files skipped because size + SHA1 already matched.
    pub skipped_files: usize,
    /// Sum of manifest `size` over all files.
    pub total_bytes: u64,
}

/// Build a rustls-backed client with a launcher user-agent.
pub fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("46anarchy-launcher/0.1.0")
        .build()?)
}

/// `true` when `dest` is missing, has the wrong size, or fails the SHA1 check.
///
/// An empty `expected_sha1` disables the hash check (size-only freshness).
///
/// Log4Shell-hardened jars are special: [`crate::mojang::log4j_patched_sha`]
/// records the post-strip hash in a sidecar, and a patched jar compares
/// against *that* (never against the manifest hash, which describes the
/// vulnerable upstream bytes) — otherwise every verify would "repair" the
/// jar back to vulnerable.
///
/// This is a blocking filesystem + hashing routine: call it via
/// [`needs_update_blocking`] from async contexts (see below).
pub fn needs_update(dest: &Path, expected_size: u64, expected_sha1: &str) -> bool {
    if let Some(patched) = crate::mojang::log4j_patched_sha(dest) {
        return match hash_file(dest) {
            Ok(actual) => !actual.eq_ignore_ascii_case(&patched),
            Err(_) => true,
        };
    }
    let meta = match std::fs::metadata(dest) {
        Ok(m) => m,
        Err(_) => return true,
    };
    if meta.len() != expected_size {
        return true;
    }
    if expected_sha1.trim().is_empty() {
        return false;
    }
    match hash_file(dest) {
        Ok(actual) => !actual.eq_ignore_ascii_case(expected_sha1.trim()),
        Err(_) => true,
    }
}

fn tmp_path_for(dest: &Path) -> PathBuf {
    let mut os = dest.as_os_str().to_owned();
    os.push(".tmp");
    PathBuf::from(os)
}

/// Blocking [`hash_file`] off-loaded to the blocking pool so tokio workers
/// are never stalled by large-file hashing.
async fn hash_file_blocking(path: PathBuf) -> Result<String> {
    tokio::task::spawn_blocking(move || hash_file(&path)).await?
}

/// Blocking [`needs_update`] off-loaded to the blocking pool (same reason).
async fn needs_update_blocking(dest: PathBuf, size: u64, sha1: String) -> Result<bool> {
    tokio::task::spawn_blocking(move || needs_update(&dest, size, &sha1))
        .await
        .map_err(CoreError::Join)
}

/// `true` when `dest` is a file that must be executable on unix:
/// anything under `java/bin/` plus `natives/*.so`.
fn should_make_executable(dest: &Path) -> bool {
    // Sans allocation : inspection des composants (appelé une fois par
    // fichier téléchargé/vérifié — ~500× par run).
    if dest
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "bin")
        && dest
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == "java")
    {
        return true;
    }
    if dest.extension().is_some_and(|e| e == "so")
        && dest.components().any(|c| c.as_os_str() == "natives")
    {
        return true;
    }
    false
}

/// Best-effort `chmod +x` for freshly-downloaded (or already-cached) runtime
/// bits on unix. Never fails the download: a chmod failure surfaces later as
/// a clear spawn error instead of aborting the whole run here.
async fn make_executable_if_needed(dest: &Path) {
    if !should_make_executable(dest) {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = tokio::fs::metadata(dest).await {
            let mut perms = meta.permissions();
            let mode = perms.mode();
            if mode & 0o111 != 0o111 {
                perms.set_mode(mode | 0o111);
                let _ = tokio::fs::set_permissions(dest, perms).await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dest;
    }
}

async fn download_one_attempt(
    client: &reqwest::Client,
    planned: &PlannedFile,
    progress: &Arc<ProgressState>,
    progress_tx: &Option<mpsc::Sender<ProgressUpdate>>,
) -> Result<()> {
    let res = client.get(&planned.file.url).send().await?;
    let status = res.status();
    if !status.is_success() {
        return Err(CoreError::CdnFailure(format!(
            "GET {}: HTTP {status}",
            planned.file.url
        )));
    }

    if let Some(parent) = planned.dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = tmp_path_for(&planned.dest);
    let mut out = tokio::fs::File::create(&tmp).await?;
    let mut stream = res.bytes_stream();
    let mut written: u64 = 0;
    // Émission par morceau, limitée (anti-inondation à flux parallèles) :
    // le premier seuil atteint (temps ou octets) déclenche l'envoi.
    let mut last_emit = std::time::Instant::now();
    let mut last_emit_written: u64 = 0;
    // Échec après des octets déjà crédités : on les retire avant de rendre
    // la main, pour que le retry ne les compte pas deux fois.
    let rollback = |progress: &Arc<ProgressState>, written: u64| {
        if written > 0 {
            progress
                .completed_bytes
                .fetch_sub(written, Ordering::Relaxed);
        }
    };
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                rollback(progress, written);
                return Err(e.into());
            }
        };
        if let Err(e) = tokio::io::AsyncWriteExt::write_all(&mut out, &chunk).await {
            rollback(progress, written);
            return Err(e.into());
        }
        written += chunk.len() as u64;
        // Octets réglés aussitôt : la barre grimpe pendant le gros fichier.
        progress
            .completed_bytes
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        let due_bytes = written.saturating_sub(last_emit_written) >= CHUNK_EMIT_MIN_BYTES;
        let due_time = last_emit.elapsed() >= Duration::from_millis(CHUNK_EMIT_INTERVAL_MS);
        // Jamais d'intermédiaire sur le dernier morceau : l'émission de fin
        // de fichier (exacte, `current_bytes` à 0) reste l'unique 1.0.
        if (due_bytes || due_time) && written < planned.file.size && progress_tx.is_some() {
            last_emit = std::time::Instant::now();
            last_emit_written = written;
            if let Some(tx) = progress_tx {
                let _ = tx
                    .send(ProgressUpdate {
                        completed_files: progress.completed_files.load(Ordering::Relaxed),
                        total_files: progress.total_files,
                        completed_bytes: progress.completed_bytes.load(Ordering::Relaxed),
                        total_bytes: progress.total_bytes,
                        current_file: planned.file.name.clone(),
                        current_bytes: written,
                        current_total: planned.file.size,
                        skipped: false,
                    })
                    .await;
            }
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut out).await?;
    drop(out);

    // Size check before hashing (cheap rejection of truncated streams).
    let actual_len = match tokio::fs::metadata(&tmp).await {
        Ok(m) => m.len(),
        Err(e) => {
            rollback(progress, written);
            return Err(e.into());
        }
    };
    if actual_len != planned.file.size {
        rollback(progress, written);
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(CoreError::CdnFailure(format!(
            "size mismatch for {}: expected {}, got {actual_len}",
            planned.file.name, planned.file.size
        )));
    }
    // SHA1 check before the atomic rename (hashed off the async worker).
    if !planned.file.sha1.trim().is_empty() {
        let actual = match hash_file_blocking(tmp.clone()).await {
            Ok(h) => h,
            Err(e) => {
                rollback(progress, written);
                return Err(e);
            }
        };
        if !actual.eq_ignore_ascii_case(planned.file.sha1.trim()) {
            rollback(progress, written);
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(CoreError::HashMismatch {
                file: planned.file.name.clone(),
                expected: planned.file.sha1.clone(),
                actual,
            });
        }
    }
    if let Err(e) = tokio::fs::rename(&tmp, &planned.dest).await {
        rollback(progress, written);
        return Err(e.into());
    }
    make_executable_if_needed(&planned.dest).await;
    Ok(())
}

struct ProgressState {
    completed_files: AtomicUsize,
    completed_bytes: AtomicU64,
    total_files: usize,
    total_bytes: u64,
}

/// Download every file in `planned` with bounded parallelism and retries.
///
/// Files already matching size + SHA1 are skipped (counted in the summary and
/// in progress counters, no request issued). Any file still failing after
/// `max_retries` attempts aborts the whole run with its last error.
///
/// `cancel` (usually the frontend `Annuler` button flag) aborts the run:
/// in-flight attempts are dropped and pending files never start, returning
/// [`CoreError::Cancelled`].
pub async fn download_files(
    client: &reqwest::Client,
    planned: &[PlannedFile],
    max_concurrency: usize,
    max_retries: u32,
    progress_tx: Option<mpsc::Sender<ProgressUpdate>>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<DownloadSummary> {
    let max_concurrency = max_concurrency.clamp(1, 8);
    let total_files = planned.len();
    let total_bytes: u64 = planned.iter().map(|p| p.file.size).sum();

    let progress = Arc::new(ProgressState {
        completed_files: AtomicUsize::new(0),
        completed_bytes: AtomicU64::new(0),
        total_files,
        total_bytes,
    });
    let semaphore = Arc::new(Semaphore::new(max_concurrency));
    let mut set = tokio::task::JoinSet::new();

    for item in planned.iter().cloned() {
        let client = client.clone();
        let permit_owner = Arc::clone(&semaphore);
        let progress = Arc::clone(&progress);
        let tx = progress_tx.clone();
        let cancel = cancel.clone();
        set.spawn(async move {
            if is_cancelled(&cancel) {
                return Err(CoreError::Cancelled("Installation annulée.".to_string()));
            }
            let _permit = permit_owner.acquire_owned().await.expect("semaphore open");
            // Fast path: already on disk and valid (blocking hash off-loaded).
            let stale =
                needs_update_blocking(item.dest.clone(), item.file.size, item.file.sha1.clone())
                    .await?;
            if !stale {
                // Already cached: still ensure runtime bits are executable
                // (older runs predating the chmod fix may have left them 0644).
                make_executable_if_needed(&item.dest).await;
                progress.completed_files.fetch_add(1, Ordering::Relaxed);
                // Vérification : le fichier ne circule pas, ses octets sont
                // crédités d'un coup (la barre avance aussi pendant le contrôle).
                progress
                    .completed_bytes
                    .fetch_add(item.file.size, Ordering::Relaxed);
                if let Some(tx) = &tx {
                    let _ = tx
                        .send(ProgressUpdate {
                            completed_files: progress.completed_files.load(Ordering::Relaxed),
                            total_files: progress.total_files,
                            completed_bytes: progress.completed_bytes.load(Ordering::Relaxed),
                            total_bytes: progress.total_bytes,
                            current_file: item.file.name.clone(),
                            // Fichier réglé : 0 octet en vol (ses octets sont
                            // dans `completed_bytes`) — exactitude de la
                            // fraction terminale.
                            current_bytes: 0,
                            current_total: item.file.size,
                            skipped: true,
                        })
                        .await;
                }
                return Ok::<bool, CoreError>(false);
            }

            let mut attempt: u32 = 0;
            let last_err = loop {
                match download_one_attempt(&client, &item, &progress, &tx).await {
                    Ok(()) => break None,
                    Err(e) => {
                        attempt += 1;
                        // Retryable transport/hash failures: back off and retry.
                        let retryable = attempt <= max_retries && !matches!(e, CoreError::Join(_));
                        // Hash/size mismatches are worth retrying (CDN flake);
                        // still bounded by max_retries.
                        if !retryable {
                            break Some(e);
                        }
                        let backoff_ms = 200u64.saturating_mul(1u64 << attempt.min(6));
                        tokio::time::sleep(Duration::from_millis(backoff_ms.min(5_000))).await;
                        if is_cancelled(&cancel) {
                            break Some(CoreError::Cancelled("Installation annulée.".to_string()));
                        }
                        if attempt > max_retries {
                            break Some(e);
                        }
                    }
                }
            };
            if let Some(e) = last_err {
                return Err(e);
            }
            progress.completed_files.fetch_add(1, Ordering::Relaxed);
            // Téléchargement : les octets sont déjà tous crédités morceau par
            // morceau — on ne crédite que le fichier (jamais deux fois).
            if let Some(tx) = &tx {
                let _ = tx
                    .send(ProgressUpdate {
                        completed_files: progress.completed_files.load(Ordering::Relaxed),
                        total_files: progress.total_files,
                        completed_bytes: progress.completed_bytes.load(Ordering::Relaxed),
                        total_bytes: progress.total_bytes,
                        current_file: item.file.name.clone(),
                        // Fichier terminé → 0 octet en vol (exactitude de la
                        // fraction terminale : émission toujours envoyée).
                        current_bytes: 0,
                        current_total: item.file.size,
                        skipped: false,
                    })
                    .await;
            }
            Ok(true)
        });
    }

    let mut downloaded_files = 0usize;
    let mut skipped_files = 0usize;
    let mut first_err: Option<CoreError> = None;
    loop {
        tokio::select! {
            // User abort: drop in-flight attempts, never start pending ones.
            _ = wait_cancelled(cancel.clone()) => {
                set.abort_all();
                while set.join_next().await.is_some() {}
                return Err(CoreError::Cancelled("Installation annulée.".to_string()));
            }
            res = set.join_next() => {
                let Some(res) = res else { break };
                match res {
                    Ok(Ok(true)) => downloaded_files += 1,
                    Ok(Ok(false)) => skipped_files += 1,
                    Ok(Err(e)) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(CoreError::Join(e));
                        }
                    }
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    Ok(DownloadSummary {
        total_files,
        downloaded_files,
        skipped_files,
        total_bytes,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::integrity::PlannedFile;
    use crate::manifest::ManifestFile;

    /// Minimal loopback HTTP stub: serves `bodies[path]` with Content-Length,
    /// 404 otherwise. Returns the base URL (no external network touched).
    /// Bodies stream in 16 Kio slices (~1 ms apart) so multi-Mo files yield
    /// many transport chunks, like a real CDN (single-write stubs would often
    /// arrive in one chunk on loopback and hide chunk-progress bugs).
    pub(crate) async fn stub_server(
        bodies: std::collections::HashMap<String, Vec<u8>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let bodies = bodies.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = vec![0u8; 4096];
                    let mut head = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                head.extend_from_slice(&buf[..n]);
                                if head.len() > 8192 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let path = String::from_utf8_lossy(&head)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .split('?')
                        .next()
                        .unwrap_or("/")
                        .to_string();
                    let body = bodies.get(&path).cloned().unwrap_or_default();
                    let status = if bodies.contains_key(&path) {
                        "200 OK"
                    } else {
                        "404 Not Found"
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    use tokio::io::AsyncWriteExt;
                    let _ = sock.write_all(reply.as_bytes()).await;
                    for slice in body.chunks(16 * 1024) {
                        let _ = sock.write_all(slice).await;
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                });
            }
        });
        (base, handle)
    }

    fn planned_for(base: &str, game_dir: &Path, name: &str, body: &[u8]) -> PlannedFile {
        PlannedFile {
            dest: game_dir.join(name),
            file: ManifestFile {
                name: name.to_string(),
                model: "test".to_string(),
                url: format!("{base}/{name}"),
                size: body.len() as u64,
                sha1: crate::hash::sha1_hex(body),
                path: name.to_string(),
                os: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn download_files_streams_monotonic_progress_to_channel() {
        let bodies: std::collections::HashMap<String, Vec<u8>> = [
            ("/one.jar".to_string(), vec![0xA5u8; 32 * 1024]),
            ("/two.jar".to_string(), vec![0x3Cu8; 48 * 1024]),
            ("/three.jar".to_string(), vec![0xF0u8; 16 * 1024]),
        ]
        .into_iter()
        .collect();
        let (base, srv) = stub_server(bodies.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let planned: Vec<PlannedFile> = ["one.jar", "two.jar", "three.jar"]
            .iter()
            .map(|n| {
                let body = bodies.get(&format!("/{n}")).unwrap();
                planned_for(&base, dir.path(), n, body)
            })
            .collect();
        let (tx, mut rx) = mpsc::channel(256);
        let rx_task = tokio::spawn(async move {
            let mut out = Vec::new();
            while let Some(u) = rx.recv().await {
                out.push(u);
            }
            out
        });
        let summary = download_files(&build_client().unwrap(), &planned, 2, 1, Some(tx), None)
            .await
            .expect("stub downloads succeed");
        assert_eq!(
            (
                summary.total_files,
                summary.downloaded_files,
                summary.skipped_files
            ),
            (3, 3, 0)
        );
        let updates: Vec<ProgressUpdate> = rx_task.await.unwrap();
        assert!(!updates.is_empty(), "progress channel must carry updates");
        // Counters only move forward; the run ends complete (the payload
        // layer turns this into fraction 1.0).
        let mut last_files = 0usize;
        let mut last_bytes = 0u64;
        for u in &updates {
            assert_eq!(u.total_files, 3);
            assert!(u.completed_files >= last_files);
            assert!(u.completed_bytes >= last_bytes);
            last_files = u.completed_files;
            last_bytes = u.completed_bytes;
        }
        let last = updates.last().unwrap();
        assert_eq!(
            (last.completed_files, last.completed_bytes),
            (3, last.total_bytes)
        );
        assert!(last.total_bytes > 0);
        srv.abort();
        // Files landed intact on disk.
        for n in ["one.jar", "two.jar", "three.jar"] {
            let body = bodies.get(&format!("/{n}")).unwrap();
            assert_eq!(&std::fs::read(dir.path().join(n)).unwrap(), body);
        }
    }

    #[tokio::test]
    async fn download_files_emits_chunk_progress_with_real_filename() {
        // Un seul fichier multi-Mo : la barre doit grimper PENDANT le flux
        // (émissions intermédiaires au vrai nom), pas sauter à la fin.
        let size = 3 * 1024 * 1024;
        let body = vec![0x5Au8; size];
        let bodies: std::collections::HashMap<String, Vec<u8>> =
            [("/big.jar".to_string(), body.clone())]
                .into_iter()
                .collect();
        let (base, srv) = stub_server(bodies).await;
        let dir = tempfile::tempdir().unwrap();
        let planned = vec![planned_for(&base, dir.path(), "big.jar", &body)];
        let (tx, mut rx) = mpsc::channel(256);
        let rx_task = tokio::spawn(async move {
            let mut out = Vec::new();
            while let Some(u) = rx.recv().await {
                out.push(u);
            }
            out
        });
        let summary = download_files(&build_client().unwrap(), &planned, 1, 1, Some(tx), None)
            .await
            .expect("stub download succeeds");
        assert_eq!((summary.total_files, summary.downloaded_files), (1, 1));
        let updates: Vec<ProgressUpdate> = rx_task.await.unwrap();
        srv.abort();
        assert!(!updates.is_empty());
        // Chaque émission porte le VRAI nom du fichier écrit.
        for u in &updates {
            assert_eq!(u.current_file, "big.jar");
            assert_eq!(u.total_bytes, size as u64);
        }
        // Octets monotones (en-vol inclus), jamais au-delà du total.
        let mut last_bytes = 0u64;
        for u in &updates {
            assert!(u.completed_bytes >= last_bytes, "bytes went backwards");
            assert!(u.completed_bytes <= u.total_bytes);
            last_bytes = u.completed_bytes;
        }
        // Au moins une émission intermédiaire (0 < en-vol < taille).
        let intermediates = updates
            .iter()
            .filter(|u| u.completed_bytes > 0 && u.completed_bytes < u.total_bytes)
            .count();
        assert!(
            intermediates >= 1,
            "bar must climb during the file, got {} updates",
            updates.len()
        );
        // Limité (pas une émission par morceau TCP : ~12 attendues à 256 Kio)
        // mais pas réduit à la seule émission terminale.
        assert!(
            updates.len() <= 40,
            "event flood: {} updates for 3 Mio",
            updates.len()
        );
        // Terminal exact : compteurs au total, 0 octet en vol.
        let last = updates.last().unwrap();
        assert_eq!(last.completed_bytes, size as u64);
        assert_eq!(last.completed_files, 1);
        assert_eq!(last.current_bytes, 0);
        assert!(!last.skipped);
        assert_eq!(&std::fs::read(dir.path().join("big.jar")).unwrap(), &body);
    }

    #[test]
    fn executable_bit_rule_without_per_file_allocation() {
        // Garde anti-régression de `should_make_executable` sans allocation :
        // mêmes verdicts que l'ancienne forme `Vec<String>` par fichier.
        assert!(should_make_executable(Path::new("/g/java/bin/java")));
        assert!(should_make_executable(Path::new("/g/java/bin/javaw.exe")));
        assert!(!should_make_executable(Path::new("/g/java/lib/tools.jar")));
        assert!(!should_make_executable(Path::new("/g/libraries/a.jar")));
        assert!(should_make_executable(Path::new("/g/natives/liblwjgl.so")));
        assert!(!should_make_executable(Path::new("/g/natives/readme.txt")));
        assert!(!should_make_executable(Path::new("/g/mods/a.jar")));
    }
}
