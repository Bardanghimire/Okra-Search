use super::everything::{EsCli, EverythingSdk};
use super::{Action, Item, Kind, Provider};
use crate::config::{Config, FileBackendKind};
use crate::matcher::{fuzzy, is_subsequence, lc, normalize};
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

type Index = Arc<RwLock<Vec<FileEntry>>>;

fn skip(name: &str) -> bool {
    name.starts_with('.')
        || matches!(name, "node_modules" | "target" | "__pycache__" | "$RECYCLE.BIN" | "AppData" | "Library")
}

fn build_index(roots: Vec<PathBuf>, max_depth: usize) -> Index {
    let index: Index = Arc::new(RwLock::new(Vec::new()));
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
        let qc = normalize(q);
        let idx = self.index.get_or_init(|| build_index(self.roots.clone(), self.depth)).read().unwrap();
        let mut hits: Vec<(i64, &FileEntry)> = Vec::new();
        for e in idx.iter() {
            if is_subsequence(&qc, &e.lower) {
                if let Some(m) = fuzzy(q, &e.name) {
                    hits.push((m.score, e));
                }
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        hits.truncate(max);
        Some(hits.into_iter().map(|(_, e)| (e.path.clone(), e.is_dir)).collect())
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
            if let Some(sdk) = EverythingSdk::load(cfg.everything_dll.as_deref()) {
                backends.push(Box::new(sdk));
            }
            if let Some(es) = EsCli::find(cfg.es_exe.as_deref()) {
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
        if normalize(q).len() < 2 {
            return vec![];
        }
        for b in &self.backends {
            let Some(hits) = b.search(q, 200) else { continue };
            *self.active.lock().unwrap() = b.name();
            let mut items: Vec<Item> = hits
                .into_iter()
                .map(|(path, is_dir)| {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
                    // Everything also matches on path / by operators, so fall back to weaker scores.
                    let score = fuzzy(q, &name)
                        .map(|m| m.score)
                        .or_else(|| fuzzy(q, &path.display().to_string()).map(|m| m.score - 40))
                        .unwrap_or(-60)
                        - 10
                        + if is_dir { 5 } else { 0 };
                    Item {
                        id: format!("file:{}", path.display()),
                        subtitle: path.parent().map(|p| p.display().to_string()).unwrap_or_default(),
                        title: name,
                        kind: if is_dir { Kind::Folder } else { Kind::File },
                        action: Action::OpenPath(path),
                        score,
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
