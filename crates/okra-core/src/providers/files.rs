use super::{Action, Item, Kind, Provider};
use crate::config::Config;
use crate::matcher::{char_mask, is_subsequence, lc, score, score_with_lower, Query};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use walkdir::WalkDir;

/// A way of finding files by name. `None` means "unavailable right now, try the next backend".
trait FileBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn search(&self, q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>>;
}

/// Health of the optional elevated NTFS helper, as reported over its named pipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperStatus {
    /// No helper owns the pipe (not installed, not started yet, or stopped).
    NotRunning,
    /// The helper owns the pipe but another request is being served right now.
    Busy,
    /// Running, but still building or resuming its index; searches fall through to the next backend.
    Starting,
    /// Serving searches from an up-to-date index.
    Ready {
        entries: u32,
        last_sync_age_secs: u64,
        pid: u32,
        failed_syncs: u32,
    },
    /// Running, but its sync loop has not completed a pass recently; it refuses searches.
    Stale { last_sync_age_secs: u64 },
    /// Shutting down.
    Stopping,
    /// Something answered on the pipe but did not speak this protocol version
    /// (for example an older helper that needs restarting after an upgrade).
    Incompatible,
}

/// Same threshold the helper uses before it refuses searches.
const HELPER_STALE_AFTER_SECS: u64 = 60;
const HELPER_STATUS_MAGIC: &[u8; 4] = b"OKT1";
const HELPER_STATUS_LEN: usize = 28;

impl HelperStatus {
    /// True when searches are currently being answered by the helper.
    pub fn is_serving(&self) -> bool {
        matches!(self, HelperStatus::Ready { .. } | HelperStatus::Busy)
    }

    /// One-line human-readable summary.
    pub fn describe(&self) -> String {
        match self {
            HelperStatus::NotRunning => "NTFS helper: not running".to_string(),
            HelperStatus::Busy => "NTFS helper: running (busy with another request)".to_string(),
            HelperStatus::Starting => "NTFS helper: starting (building or resuming its index)".to_string(),
            HelperStatus::Ready { entries, last_sync_age_secs, pid, failed_syncs } => {
                let mut line = format!(
                    "NTFS helper: ready, {entries} paths, last synchronized {last_sync_age_secs}s ago (pid {pid})"
                );
                if *failed_syncs > 0 {
                    line.push_str(&format!("; {failed_syncs} consecutive failed sync(s)"));
                }
                line
            }
            HelperStatus::Stale { last_sync_age_secs } => format!(
                "NTFS helper: stale (no successful sync for {last_sync_age_secs}s); searches fall back to the next backend"
            ),
            HelperStatus::Stopping => "NTFS helper: stopping".to_string(),
            HelperStatus::Incompatible => {
                "NTFS helper: running a different protocol version; restart it".to_string()
            }
        }
    }
}

/// Decode a 28-byte `OKT1` status reply. `now_unix` is the caller's clock, so staleness is
/// judged locally and a helper that stopped updating cannot vouch for itself.
pub fn decode_helper_status(reply: &[u8; HELPER_STATUS_LEN], now_unix: u64) -> HelperStatus {
    if &reply[..4] != HELPER_STATUS_MAGIC {
        return HelperStatus::Incompatible;
    }
    let u16_at = |at: usize| u16::from_le_bytes([reply[at], reply[at + 1]]);
    let u32_at =
        |at: usize| u32::from_le_bytes([reply[at], reply[at + 1], reply[at + 2], reply[at + 3]]);
    let last_ok = u64::from_le_bytes(reply[16..24].try_into().unwrap());
    match u16_at(4) {
        0 => HelperStatus::Starting,
        2 => HelperStatus::Stopping,
        1 => {
            let age = now_unix.saturating_sub(last_ok);
            if age > HELPER_STALE_AFTER_SECS {
                HelperStatus::Stale {
                    last_sync_age_secs: age,
                }
            } else {
                HelperStatus::Ready {
                    entries: u32_at(8),
                    last_sync_age_secs: age,
                    pid: u32_at(12),
                    failed_syncs: u32_at(24),
                }
            }
        }
        _ => HelperStatus::Incompatible,
    }
}

/// Ask the helper for its health. Never prompts for elevation and never starts anything.
#[cfg(windows)]
pub fn ntfs_helper_status() -> HelperStatus {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};

    const PIPE_PATH: &str = r"\\.\pipe\okra-brown-search-v1";
    const ERROR_PIPE_BUSY: i32 = 231;

    let mut pipe = match OpenOptions::new().read(true).write(true).open(PIPE_PATH) {
        Ok(pipe) => pipe,
        Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => return HelperStatus::Busy,
        Err(_) => return HelperStatus::NotRunning,
    };
    if pipe.write_all(b"OKS1\0\0\0\0").is_err() {
        return HelperStatus::Incompatible;
    }
    // Read the 8-byte prefix first: a Stage 3 helper answers an unknown request with an
    // 8-byte `OKR1` refusal and would otherwise leave a full-length read hanging on EOF.
    let mut reply = [0u8; HELPER_STATUS_LEN];
    if pipe.read_exact(&mut reply[..8]).is_err() || &reply[..4] != HELPER_STATUS_MAGIC {
        return HelperStatus::Incompatible;
    }
    if pipe.read_exact(&mut reply[8..]).is_err() {
        return HelperStatus::Incompatible;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    decode_helper_status(&reply, now)
}

#[cfg(not(windows))]
pub fn ntfs_helper_status() -> HelperStatus {
    HelperStatus::NotRunning
}

/// One raw search round trip to the elevated NTFS helper (no scoring, no fallback). `None`
/// when the helper is missing, not ready, or the request failed. Used by the benchmarks.
#[cfg(windows)]
pub fn ntfs_helper_search(q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>> {
    search_ntfs_pipe(q, max)
}

#[cfg(not(windows))]
pub fn ntfs_helper_search(_q: &str, _max: usize) -> Option<Vec<(PathBuf, bool)>> {
    None
}

/// Current-user named-pipe client for the separately elevated Okra NTFS helper.
/// A missing helper is not an error: the provider chain continues to the built-in index.
#[cfg(windows)]
struct NtfsPipe;

#[cfg(windows)]
impl FileBackend for NtfsPipe {
    fn name(&self) -> &'static str {
        "Okra NTFS helper"
    }
    fn search(&self, q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>> {
        search_ntfs_pipe(q, max)
    }
}

#[cfg(windows)]
fn search_ntfs_pipe(q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>> {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};

    const PIPE_PATH: &str = r"\\.\pipe\okra-brown-search-v1";
    const REQUEST_MAGIC: &[u8; 4] = b"OKQ1";
    const RESPONSE_MAGIC: &[u8; 4] = b"OKR1";
    const MAX_QUERY_BYTES: usize = 1024;
    const MAX_RESULTS: usize = 200;
    const MAX_PATH_BYTES: usize = 128 * 1024;

    let query = q.as_bytes();
    if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.iter().any(|byte| *byte < 0x20) {
        return None;
    }
    let limit = max.clamp(1, MAX_RESULTS);
    // The helper serves one client at a time on a single pipe instance, so a client that arrives
    // between two requests sees ERROR_PIPE_BUSY for a moment. Retry briefly instead of giving
    // up (and dropping to a slower backend) on the first busy answer.
    const OPEN_BUSY: i32 = 231;
    let mut busy_retries = 0;
    let mut pipe = loop {
        match OpenOptions::new().read(true).write(true).open(PIPE_PATH) {
            Ok(pipe) => break pipe,
            Err(error) if error.raw_os_error() == Some(OPEN_BUSY) && busy_retries < 40 => {
                busy_retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(_) => return None,
        }
    };
    let mut request = Vec::with_capacity(8 + query.len());
    request.extend_from_slice(REQUEST_MAGIC);
    request.extend_from_slice(&(limit as u16).to_le_bytes());
    request.extend_from_slice(&(query.len() as u16).to_le_bytes());
    request.extend_from_slice(query);
    pipe.write_all(&request).ok()?;

    let mut header = [0u8; 8];
    pipe.read_exact(&mut header).ok()?;
    if &header[..4] != RESPONSE_MAGIC {
        return None;
    }
    let status = u16::from_le_bytes([header[4], header[5]]);
    let count = u16::from_le_bytes([header[6], header[7]]) as usize;
    if status != 0 || count > limit {
        return None;
    }

    let mut results = Vec::with_capacity(count);
    for _ in 0..count {
        let mut entry_header = [0u8; 5];
        pipe.read_exact(&mut entry_header).ok()?;
        let is_dir = match entry_header[0] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let path_len = u32::from_le_bytes(entry_header[1..5].try_into().ok()?) as usize;
        if path_len == 0 || path_len > MAX_PATH_BYTES {
            return None;
        }
        let mut path = vec![0u8; path_len];
        pipe.read_exact(&mut path).ok()?;
        let path = String::from_utf8(path).ok()?;
        results.push((PathBuf::from(path), is_dir));
    }
    // An empty scoped NTFS snapshot is not proof that the complete configured search space has
    // no match (for example, another configured root may live on a different volume). Let the
    // existing walker backend try before reporting no results.
    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

// ---------------------------------------------------------------- built-in fallback index

struct FileEntry {
    name: String,
    lower: String,
    path: PathBuf,
    is_dir: bool,
}

/// `masks[i]` is the [`char_mask`] of `entries[i].lower`. Kept in its own contiguous array so the
/// first filtering pass streams 8 bytes per file instead of touching every file's strings.
#[derive(Default)]
struct IndexData {
    entries: Vec<FileEntry>,
    masks: Vec<u64>,
}

impl IndexData {
    fn extend(&mut self, batch: impl IntoIterator<Item = FileEntry>) {
        for e in batch {
            self.masks.push(char_mask(e.lower.chars()));
            self.entries.push(e);
        }
    }
}

type Index = Arc<RwLock<IndexData>>;

fn skip(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "target" | "__pycache__" | "$RECYCLE.BIN" | "AppData" | "Library"
        )
}

fn build_index(roots: Vec<PathBuf>, max_depth: usize) -> Index {
    let index: Index = Arc::new(RwLock::new(IndexData::default()));
    let i2 = index.clone();
    std::thread::spawn(move || {
        for root in roots {
            let mut batch: Vec<FileEntry> = Vec::with_capacity(4096);
            let walker = WalkDir::new(&root)
                .max_depth(max_depth)
                .into_iter()
                .filter_entry(|e| e.depth() == 0 || !skip(&e.file_name().to_string_lossy()));
            for e in walker.flatten() {
                if e.depth() == 0 {
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_string();
                batch.push(FileEntry {
                    lower: name.chars().map(lc).collect(),
                    name,
                    path: e.path().to_path_buf(),
                    is_dir: e.file_type().is_dir(),
                });
                if batch.len() >= 4096 {
                    i2.write().unwrap().extend(batch.drain(..));
                }
            }
            i2.write().unwrap().extend(batch.drain(..));
        }
    });
    index
}

/// Okra-owned walker index. It warms in the background as a fallback to the NTFS index.
struct Builtin {
    roots: Vec<PathBuf>,
    depth: usize,
    index: OnceLock<Index>,
}

impl FileBackend for Builtin {
    fn name(&self) -> &'static str {
        "built-in index"
    }
    fn search(&self, q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>> {
        let query = Query::new(q);
        let idx = self
            .index
            .get_or_init(|| build_index(self.roots.clone(), self.depth))
            .read()
            .unwrap();
        // (score, position in index). Higher score first; equal scores keep index order.
        let mut hits: Vec<(i64, usize)> = Vec::new();
        let qm = query.mask();
        for (pos, &mask) in idx.masks.iter().enumerate() {
            if mask & qm != qm {
                continue; // lacks one of the query's characters: cannot match
            }
            let e = &idx.entries[pos];
            if is_subsequence(query.chars(), &e.lower) {
                if let Some(s) = score_with_lower(&query, &e.name, &e.lower) {
                    hits.push((s, pos));
                }
            }
        }
        // Only the best `max` are needed: select them in O(n) and sort just those, instead of
        // sorting every hit (a short query like "ch" can match a large share of the index).
        let order = |a: &(i64, usize), b: &(i64, usize)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
        if max > 0 && hits.len() > max {
            hits.select_nth_unstable_by(max - 1, order);
            hits.truncate(max);
        }
        hits.sort_by(order);
        hits.truncate(max);
        Some(
            hits.into_iter()
                .map(|(_, pos)| (idx.entries[pos].path.clone(), idx.entries[pos].is_dir))
                .collect(),
        )
    }
}

// ---------------------------------------------------------------- provider

pub struct FilesProvider {
    backends: Vec<Box<dyn FileBackend>>,
    active: Mutex<&'static str>,
    notice: Option<String>,
}

impl FilesProvider {
    pub fn new(cfg: &Config) -> Self {
        Self::with_ntfs_helper(cfg, true)
    }

    /// Construct the unprivileged walker-only backend for isolated benchmarks.
    #[doc(hidden)]
    pub fn walker_only(cfg: &Config) -> Self {
        Self::with_ntfs_helper(cfg, false)
    }

    fn with_ntfs_helper(cfg: &Config, use_ntfs_helper: bool) -> Self {
        let builtin = Builtin {
            roots: cfg.file_roots.clone(),
            depth: cfg.file_max_depth,
            index: OnceLock::new(),
        };

        // Warm Okra's own fallback index in the background even when the NTFS helper is healthy.
        // That way a helper outage or unsupported volume does not make the first fallback query
        // pay the cost of starting the filesystem walk.
        builtin
            .index
            .get_or_init(|| build_index(builtin.roots.clone(), builtin.depth));

        let mut backends: Vec<Box<dyn FileBackend>> = Vec::new();
        // The elevated Okra helper is the primary fast path on Windows. Other platforms and
        // helper outages use the built-in walker index; no external search service is consulted.
        if use_ntfs_helper {
            #[cfg(windows)]
            backends.push(Box::new(NtfsPipe));
        }
        backends.push(Box::new(builtin));

        Self {
            backends,
            active: Mutex::new("none"),
            notice: None,
        }
    }

    /// Startup notice, reserved for actionable indexing diagnostics.
    pub fn startup_notice(&self) -> Option<String> {
        self.notice.clone()
    }

    /// Current health of the optional elevated NTFS helper (a quick local pipe round trip).
    pub fn helper_status(&self) -> HelperStatus {
        ntfs_helper_status()
    }

    /// Which backend answered the most recent query (for diagnostics / status lines).
    pub fn active_backend(&self) -> &'static str {
        *self.active.lock().unwrap()
    }
}

impl Provider for FilesProvider {
    fn name(&self) -> &'static str {
        "Files"
    }
    fn prefix(&self) -> Option<&'static str> {
        Some("f")
    }
    fn query(&self, q: &str, limit: usize) -> Vec<Item> {
        let query = Query::new(q);
        if query.len() < 2 {
            return vec![];
        }
        for b in &self.backends {
            // Ask for a few times what can be shown rather than a flat 200: every extra hit is
            // fetched, scored and sorted on each keystroke.
            let want = limit.saturating_mul(3).clamp(60, 200);
            let Some(hits) = b.search(q, want) else {
                continue;
            };
            *self.active.lock().unwrap() = b.name();
            let mut items: Vec<Item> = hits
                .into_iter()
                .map(|(path, is_dir)| {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.display().to_string());
                    // Helper results are ranked by basename; use the full path as a secondary
                    // match so folder names can still help a result surface.
                    let rank = score(&query, &name)
                        .or_else(|| score(&query, &path.display().to_string()).map(|s| s - 40))
                        .unwrap_or(-60)
                        - 10
                        + if is_dir { 5 } else { 0 };
                    Item {
                        id: format!("file:{}", path.display()),
                        subtitle: path
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        title: name,
                        kind: if is_dir { Kind::Folder } else { Kind::File },
                        action: Action::OpenPath(path),
                        score: rank,
                    }
                })
                .collect();
            items.sort_by_key(|a| std::cmp::Reverse(a.score));
            items.truncate(limit);
            return items;
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 33
    }

    fn synth(n: usize) -> Builtin {
        const WORDS: &[&str] = &[
            "report", "invoice", "final", "draft", "thesis", "turbine", "heat", "transfer",
            "chapter", "notes", "Visual", "Studio", "Code", "Chrome", "Firefox", "setup", "backup",
            "photo", "IMG", "scan", "lecture", "design", "gear", "shaft", "fluent", "ansys",
            "model", "v2", "copy", "new",
        ];
        const EXT: &[&str] = &[
            "pdf", "docx", "xlsx", "txt", "png", "py", "rs", "dll", "exe", "zip",
        ];
        let mut seed = 42u64;
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let words = 1 + (lcg(&mut seed) % 4) as usize;
            let mut name = String::new();
            for w in 0..words {
                if w > 0 {
                    name.push(['_', '-', ' '][(lcg(&mut seed) % 3) as usize]);
                }
                name.push_str(WORDS[(lcg(&mut seed) % WORDS.len() as u64) as usize]);
            }
            name.push('.');
            name.push_str(EXT[(lcg(&mut seed) % EXT.len() as u64) as usize]);
            entries.push(FileEntry {
                lower: name.chars().map(lc).collect(),
                path: PathBuf::from(format!("/synthetic/{i}/{name}")),
                name,
                is_dir: false,
            });
        }
        let b = Builtin {
            roots: vec![],
            depth: 0,
            index: OnceLock::new(),
        };
        let mut data = IndexData::default();
        data.extend(entries);
        let _ = b.index.set(Arc::new(RwLock::new(data)));
        b
    }

    /// `cargo test --release -p okra-core -- --ignored --nocapture bench_builtin`
    #[test]
    #[ignore]
    fn bench_builtin() {
        let b = synth(300_000);
        let queries = [
            "ch",
            "chr",
            "chrome",
            "vsc",
            "turb",
            "heat tr",
            "report fin",
            "invoice pdf",
        ];
        let _ = b.search("warm", 200);
        let t = std::time::Instant::now();
        let mut total = 0usize;
        for q in &queries {
            total += b.search(q, 200).unwrap().len();
        }
        let ms = t.elapsed().as_secs_f64() * 1e3 / queries.len() as f64;
        println!("built-in index, 300k entries: {ms:.2} ms / query (returned {total} rows)");
    }
}

#[cfg(test)]
mod helper_status_tests {
    use super::*;

    fn reply(
        state: u16,
        entries: u32,
        pid: u32,
        last_ok: u64,
        failures: u32,
    ) -> [u8; HELPER_STATUS_LEN] {
        let mut out = [0u8; HELPER_STATUS_LEN];
        out[..4].copy_from_slice(HELPER_STATUS_MAGIC);
        out[4..6].copy_from_slice(&state.to_le_bytes());
        out[8..12].copy_from_slice(&entries.to_le_bytes());
        out[12..16].copy_from_slice(&pid.to_le_bytes());
        out[16..24].copy_from_slice(&last_ok.to_le_bytes());
        out[24..28].copy_from_slice(&failures.to_le_bytes());
        out
    }

    #[test]
    fn decodes_each_state() {
        assert_eq!(
            decode_helper_status(&reply(0, 0, 1, 0, 0), 100),
            HelperStatus::Starting
        );
        assert_eq!(
            decode_helper_status(&reply(2, 5, 1, 90, 0), 100),
            HelperStatus::Stopping
        );
        assert_eq!(
            decode_helper_status(&reply(1, 70_799, 4242, 95, 3), 100),
            HelperStatus::Ready {
                entries: 70_799,
                last_sync_age_secs: 5,
                pid: 4242,
                failed_syncs: 3
            }
        );
        assert_eq!(
            decode_helper_status(&reply(9, 0, 1, 0, 0), 100),
            HelperStatus::Incompatible
        );
    }

    #[test]
    fn staleness_is_judged_on_the_launchers_clock() {
        let r = reply(1, 10, 1, 1_000, 0);
        assert!(matches!(
            decode_helper_status(&r, 1_000 + HELPER_STALE_AFTER_SECS),
            HelperStatus::Ready { .. }
        ));
        assert_eq!(
            decode_helper_status(&r, 1_001 + HELPER_STALE_AFTER_SECS),
            HelperStatus::Stale {
                last_sync_age_secs: HELPER_STALE_AFTER_SECS + 1
            }
        );
        // A helper clock ahead of ours must not underflow.
        assert!(matches!(
            decode_helper_status(&r, 500),
            HelperStatus::Ready {
                last_sync_age_secs: 0,
                ..
            }
        ));
    }

    #[test]
    fn wrong_magic_is_incompatible_and_serving_flags_are_right() {
        let mut r = reply(1, 1, 1, 100, 0);
        r[..4].copy_from_slice(b"OKR1");
        assert_eq!(decode_helper_status(&r, 100), HelperStatus::Incompatible);
        assert!(HelperStatus::Ready {
            entries: 1,
            last_sync_age_secs: 0,
            pid: 1,
            failed_syncs: 0
        }
        .is_serving());
        assert!(HelperStatus::Busy.is_serving());
        for s in [
            HelperStatus::NotRunning,
            HelperStatus::Starting,
            HelperStatus::Stopping,
            HelperStatus::Incompatible,
            HelperStatus::Stale {
                last_sync_age_secs: 61,
            },
        ] {
            assert!(!s.is_serving());
        }
    }

    #[test]
    fn descriptions_mention_the_key_facts() {
        let line = HelperStatus::Ready {
            entries: 70_799,
            last_sync_age_secs: 2,
            pid: 7,
            failed_syncs: 1,
        }
        .describe();
        assert!(line.contains("70799") && line.contains("2s ago") && line.contains("failed"));
        assert!(HelperStatus::Stale {
            last_sync_age_secs: 99
        }
        .describe()
        .contains("stale"));
    }
}
