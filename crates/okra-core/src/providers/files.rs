use super::everything::{EsCli, EverythingSdk};
use super::{Action, Item, Kind, Provider};
use crate::config::{Config, FileBackendKind};
use crate::matcher::{char_mask, is_subsequence, lc, score, score_with_lower, Query};
use std::path::PathBuf;
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
}

impl FilesProvider {
    pub fn new(cfg: &Config) -> Self {
        let builtin = || Builtin { roots: cfg.file_roots.clone(), depth: cfg.file_max_depth, index: OnceLock::new() };
        let mut backends: Vec<Box<dyn FileBackend>> = Vec::new();
        let use_everything = !matches!(cfg.file_backend, FileBackendKind::Builtin);
        if use_everything {
            // es.exe talks to the same running Everything instance as the SDK DLL, so it can only
            // help when the DLL is missing. Probing it spawns processes, so skip that when the
            // DLL loaded: it keeps startup fast and avoids a ~50x slower per-query path.
            if let Some(sdk) = EverythingSdk::load(cfg.everything_dll.as_deref()) {
                backends.push(Box::new(sdk));
            } else if let Some(es) = EsCli::find(cfg.es_exe.as_deref()) {
                backends.push(Box::new(es));
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
        Self { backends, active: Mutex::new("none") }
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
