use std::collections::HashMap;
use std::path::PathBuf;

/// Per-item activation counts; used to boost frequently opened items.
#[derive(Default)]
pub struct History {
    counts: HashMap<String, u32>,
    path: Option<PathBuf>,
}

impl History {
    pub fn load() -> Self {
        let path = dirs::data_dir().map(|d| d.join("okra-brown-search").join("history.json"));
        let counts = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self { counts, path }
    }

    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn boost(&self, id: &str) -> i64 {
        self.counts.get(id).copied().unwrap_or(0).min(25) as i64 * 8
    }

    pub fn record(&mut self, id: &str) {
        *self.counts.entry(id.to_string()).or_insert(0) += 1;
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string(&self.counts) {
                let _ = std::fs::write(path, json);
            }
        }
    }
}
