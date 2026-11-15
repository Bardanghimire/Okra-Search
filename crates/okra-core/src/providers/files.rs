use super::everything::{EsCli, EverythingSdk};
use super::{Action, Item, Kind, Provider};
use crate::config::{Config, FileBackendKind};
use crate::matcher::{char_mask, is_subsequence, lc, score, score_with_lower, Query};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use walkdir::WalkDir;

/// A way of finding files by name. `None` means "unavailable right now, try the next backend".
trait FileBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn search(&self, q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>>;
}

impl FileBackend for EverythingSdk {
    fn name(&self) -> &'static str {
        "Everything (SDK)"
    }
    fn search(&self, q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>> {
        EverythingSdk::search(self, q, max as u32)
    }
}

impl FileBackend for EsCli {
    fn name(&self) -> &'static str {
        "Everything (es.exe)"
    }
    fn search(&self, q: &str, max: usize) -> Option<Vec<(PathBuf, bool)>> {
        EsCli::search(self, q, max as u32)
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
        || matches!(name, "node_modules" | "target" | "__pycache__" | "$RECYCLE.BIN" | "AppData" | "Library")
}

/// Walks `roots` and hands the entries found to `sink` in batches (the sink drains each batch).
fn scan(roots: &[PathBuf], max_depth: usize, sink: &mut dyn FnMut(&mut Vec<FileEntry>)) {
    for root in roots {
        let mut batch: Vec<FileEntry> = Vec::with_capacity(4096);
        let walker = WalkDir::new(root)
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
                sink(&mut batch);
            }
        }
        sink(&mut batch);
    }
}

// The index is saved between runs, so the launcher can search the moment it starts instead of
// waiting for a full scan. The file is plain text: a header line, then `F<TAB>path` or `D<TAB>path`.
const CACHE_MAGIC: &str = "okra-index-v1";
/// How long the resident app waits before scanning again to pick up new and deleted files.
const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(30 * 60);

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("okra-brown-search").join("file-index.tsv"))
}

/// Identifies what the cache was built from; a cache for other folders or another depth is ignored.
fn cache_header(roots: &[PathBuf], max_depth: usize) -> String {
    let roots: Vec<String> = roots.iter().map(|r| r.to_string_lossy().into_owned()).collect();
    format!("{CACHE_MAGIC}\t{max_depth}\t{}", roots.join("|"))
}

fn serialize_index(data: &IndexData, roots: &[PathBuf], max_depth: usize) -> String {
    let mut text = cache_header(roots, max_depth);
    text.push('\n');
    for entry in &data.entries {
        // Names that could not survive a text line are simply left out of the cache.
        let Some(path) = entry.path.to_str() else { continue };
        if path.contains(['\t', '\n', '\r']) {
            continue;
        }
        text.push(if entry.is_dir { 'D' } else { 'F' });
        text.push('\t');
        text.push_str(path);
        text.push('\n');
    }
    text
}

/// `None` when the text is not a cache for exactly these roots and depth, or is damaged.
fn parse_index(text: &str, roots: &[PathBuf], max_depth: usize) -> Option<IndexData> {
    let mut lines = text.lines();
    if lines.next()? != cache_header(roots, max_depth) {
        return None;
    }
    let mut entries: Vec<FileEntry> = Vec::new();
    for line in lines {
        let (flag, path) = line.split_once('\t')?;
        let path = PathBuf::from(path);
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        entries.push(FileEntry { lower: name.chars().map(lc).collect(), name, path, is_dir: flag == "D" });
    }
    let mut data = IndexData::default();
    data.extend(entries);
    Some(data)
}

/// Writes next to the real file and renames, so a crash never leaves half a cache behind.
fn write_cache(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temp = path.with_extension("tmp");
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}

fn build_index(roots: Vec<PathBuf>, max_depth: usize) -> Index {
    let index: Index = Arc::new(RwLock::new(IndexData::default()));
    let shared = index.clone();
    std::thread::spawn(move || {
        let cache_file = cache_path();
        // A saved index answers searches right away; the scan below then replaces it.
        let mut warm = false;
        if let Some(path) = &cache_file {
            if let Some(data) = std::fs::read_to_string(path).ok().and_then(|text| parse_index(&text, &roots, max_depth)) {
                *shared.write().unwrap() = data;
                warm = true;
            }
        }
        loop {
            let mut fresh = IndexData::default();
            scan(&roots, max_depth, &mut |batch: &mut Vec<FileEntry>| {
                if warm {
                    // Keep serving the old index until the new one is complete.
                    fresh.extend(batch.drain(..));
                } else {
                    // First ever scan: results become searchable as they are found.
                    shared.write().unwrap().extend(batch.drain(..));
                }
            });
            if warm {
                *shared.write().unwrap() = fresh;
            }
            warm = true;
            if let Some(path) = &cache_file {
                let text = serialize_index(&shared.read().unwrap(), &roots, max_depth);
                write_cache(path, &text);
            }
            std::thread::sleep(RESCAN_EVERY);
        }
    });
    index
}

/// Own walker-based index. Started lazily, so with Everything available it costs nothing.
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
        let idx = self.index.get_or_init(|| build_index(self.roots.clone(), self.depth)).read().unwrap();
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
        Some(hits.into_iter().map(|(_, pos)| (idx.entries[pos].path.clone(), idx.entries[pos].is_dir)).collect())
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
        let builtin = || Builtin { roots: cfg.file_roots.clone(), depth: cfg.file_max_depth, index: OnceLock::new() };
        let mut backends: Vec<Box<dyn FileBackend>> = Vec::new();
        let mut notice: Option<String> = None;
        let use_everything = !matches!(cfg.file_backend, FileBackendKind::Builtin);
        if use_everything {
            // es.exe talks to the same running Everything instance as the SDK DLL, so it can only
            // help when the DLL is missing. Probing it spawns processes, so skip that when the
            // DLL loaded: it keeps startup fast and avoids a ~50x slower per-query path.
            if let Some(sdk) = EverythingSdk::load(cfg.everything_dll.as_deref()) {
                backends.push(Box::new(sdk));
            } else if let Some(es) = EsCli::find(cfg.es_exe.as_deref()) {
                backends.push(Box::new(es));
                notice = Some(
                    "Everything64.dll not found: using the slower es.exe for files".to_string(),
                );
            } else {
                notice = Some(
                    match cfg.file_backend {
                        FileBackendKind::Everything => "Everything not found: file search is unavailable",
                        _ => "Everything not found: using the built-in file index (slower)",
                    }
                    .to_string(),
                );
            }
        }
        match cfg.file_backend {
            FileBackendKind::Auto => {
                let b = builtin();
                if backends.is_empty() {
                    // No Everything on this machine: index right away instead of on first query.
                    b.index.get_or_init(|| build_index(b.roots.clone(), b.depth));
                }
                backends.push(Box::new(b)); // otherwise stays dormant until Everything fails
            }
            FileBackendKind::Builtin => {
                let b = builtin();
                b.index.get_or_init(|| build_index(b.roots.clone(), b.depth)); // eager
                backends.push(Box::new(b));
            }
            FileBackendKind::Everything => {}
        }
        Self { backends, active: Mutex::new("none"), notice }
    }

    /// Set when the fast Everything path is not available, so the app can tell the user once.
    pub fn startup_notice(&self) -> Option<String> {
        self.notice.clone()
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
            let Some(hits) = b.search(q, want) else { continue };
            *self.active.lock().unwrap() = b.name();
            let mut items: Vec<Item> = hits
                .into_iter()
                .map(|(path, is_dir)| {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
                    // Everything also matches on path / by operators, so fall back to weaker scores.
                    let rank = score(&query, &name)
                        .or_else(|| score(&query, &path.display().to_string()).map(|s| s - 40))
                        .unwrap_or(-60)
                        - 10
                        + if is_dir { 5 } else { 0 };
                    Item {
                        id: format!("file:{}", path.display()),
                        subtitle: path.parent().map(|p| p.display().to_string()).unwrap_or_default(),
                        title: name,
                        kind: if is_dir { Kind::Folder } else { Kind::File },
                        action: Action::OpenPath(path),
                        score: rank,
                    }
                })
                .collect();
            items.sort_by(|a, b| b.score.cmp(&a.score));
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
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 33
    }

    fn synth(n: usize) -> Builtin {
        const WORDS: &[&str] = &[
            "report", "invoice", "final", "draft", "thesis", "turbine", "heat", "transfer", "chapter", "notes",
            "Visual", "Studio", "Code", "Chrome", "Firefox", "setup", "backup", "photo", "IMG", "scan", "lecture",
            "design", "gear", "shaft", "fluent", "ansys", "model", "v2", "copy", "new",
        ];
        const EXT: &[&str] = &["pdf", "docx", "xlsx", "txt", "png", "py", "rs", "dll", "exe", "zip"];
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
        let b = Builtin { roots: vec![], depth: 0, index: OnceLock::new() };
        let mut data = IndexData::default();
        data.extend(entries);
        let _ = b.index.set(Arc::new(RwLock::new(data)));
        b
    }

    #[test]
    fn saved_index_round_trips_and_rejects_other_settings() {
        let dir = std::env::temp_dir().join(format!("okra-index-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), "y").unwrap();

        let roots = vec![dir.clone()];
        let mut scanned = IndexData::default();
        scan(&roots, 8, &mut |batch: &mut Vec<FileEntry>| scanned.extend(batch.drain(..)));
        assert_eq!(scanned.entries.len(), 3); // a.txt, sub, sub/b.txt

        let text = serialize_index(&scanned, &roots, 8);
        let loaded = parse_index(&text, &roots, 8).unwrap();
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.masks.len(), 3);
        assert!(loaded.entries.iter().any(|e| e.name == "b.txt" && !e.is_dir));
        assert!(loaded.entries.iter().any(|e| e.name == "sub" && e.is_dir));

        assert!(parse_index(&text, &roots, 3).is_none()); // other depth
        assert!(parse_index(&text, &[PathBuf::from("elsewhere")], 8).is_none()); // other folders
        assert!(parse_index("garbage", &roots, 8).is_none());
        let damaged = format!("{}\nno tab on this line\n", cache_header(&roots, 8));
        assert!(parse_index(&damaged, &roots, 8).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `cargo test --release -p okra-core -- --ignored --nocapture bench_builtin`
    #[test]
    #[ignore]
    fn bench_builtin() {
        let b = synth(300_000);
        let queries = ["ch", "chr", "chrome", "vsc", "turb", "heat tr", "report fin", "invoice pdf"];
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
