use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    /// Total activations per item id.
    counts: HashMap<String, u32>,
    /// Unix seconds of the last activation per item id.
    last: HashMap<String, u64>,
    /// What the user actually picked for a typed query: query -> (item id -> count).
    learned: HashMap<String, HashMap<String, u32>>,
}

/// Usage history: frequency, recency, and which result was chosen for which query. All of it only
/// nudges ranking, so a wrong guess is cheap and the typed text still decides what matches.
#[derive(Default)]
pub struct History {
    data: Data,
    path: Option<PathBuf>,
    /// Feeds the background writer thread (started on the first `record`).
    writer: Option<Sender<String>>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn normalize_query(query: &str) -> String {
    query.trim().to_lowercase()
}

impl History {
    pub fn load() -> Self {
        let path = dirs::data_dir().map(|d| d.join("okra-brown-search").join("history.json"));
        let text = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
        let data = match text {
            Some(text) => {
                // Older versions stored just `{ id: count }`; keep those counts.
                if let Ok(counts) = serde_json::from_str::<HashMap<String, u32>>(&text) {
                    Data { counts, ..Data::default() }
                } else {
                    serde_json::from_str::<Data>(&text).unwrap_or_default()
                }
            }
            None => Data::default(),
        };
        Self { data, path, writer: None }
    }

    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Frequency plus recency, independent of what was typed.
    pub fn boost(&self, id: &str) -> i64 {
        self.boost_at(id, now_secs())
    }

    fn boost_at(&self, id: &str, now: u64) -> i64 {
        let frequency = self.data.counts.get(id).copied().unwrap_or(0).min(25) as i64 * 8;
        let recency = match self.data.last.get(id) {
            Some(&when) => {
                let age = now.saturating_sub(when);
                if age < 3_600 {
                    80
                } else if age < 86_400 {
                    50
                } else if age < 7 * 86_400 {
                    25
                } else {
                    0
                }
            }
            None => 0,
        };
        frequency + recency
    }

    /// `boost` plus a bonus when this item was picked before for this query (or a prefix of it).
    pub fn boost_for_query(&self, id: &str, query: &str) -> i64 {
        self.for_query(query).boost(id)
    }

    /// Prepares scoring for one search: the clock is read once and the learned-query lookups are
    /// resolved once, so scoring each of hundreds of results allocates nothing.
    pub fn for_query(&self, query: &str) -> QueryBoost<'_> {
        let query = normalize_query(query);
        let mut learned = Vec::new();
        if !self.data.learned.is_empty() {
            let mut end = 0;
            for ch in query.chars() {
                end += ch.len_utf8();
                if let Some(picks) = self.data.learned.get(&query[..end]) {
                    learned.push(picks);
                }
            }
        }
        QueryBoost { history: self, now: now_secs(), learned }
    }

    pub fn record(&mut self, id: &str, query: &str) {
        *self.data.counts.entry(id.to_string()).or_insert(0) += 1;
        self.data.last.insert(id.to_string(), now_secs());
        let query = normalize_query(query);
        if !query.is_empty() && query.chars().count() <= 40 {
            *self.data.learned.entry(query).or_default().entry(id.to_string()).or_insert(0) += 1;
        }
        self.persist();
    }

    /// Serializes here (microseconds) and hands the file write to a background thread, so
    /// activating a result never waits on the disk (or an antivirus scan of it).
    fn persist(&mut self) {
        let Some(path) = &self.path else { return };
        let Ok(json) = serde_json::to_string(&self.data) else { return };
        let writer = self.writer.get_or_insert_with(|| spawn_writer(path.clone()));
        let _ = writer.send(json);
    }
}

/// Scoring prepared by [`History::for_query`].
pub struct QueryBoost<'a> {
    history: &'a History,
    now: u64,
    /// For each typed prefix that was ever picked from: item id -> times picked.
    learned: Vec<&'a HashMap<String, u32>>,
}

impl QueryBoost<'_> {
    pub fn boost(&self, id: &str) -> i64 {
        let best = self.learned.iter().filter_map(|picks| picks.get(id)).copied().max().unwrap_or(0);
        self.history.boost_at(id, self.now) + best.min(10) as i64 * 35
    }
}

/// One writer thread; if several saves queue up only the newest is written.
fn spawn_writer(path: PathBuf) -> Sender<String> {
    let (tx, rx) = channel::<String>();
    let _ = std::thread::Builder::new().name("okra-history-writer".into()).spawn(move || {
        while let Ok(mut json) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                json = newer;
            }
            write_atomic(&path, json.as_bytes());
        }
    });
    tx
}

/// Write-then-rename, so quitting mid-write can never leave a truncated (= lost) history file.
fn write_atomic(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-item implementation this replaced, as the oracle for `for_query`.
    fn reference_boost_for_query(h: &History, id: &str, query: &str) -> i64 {
        let mut total = h.boost(id);
        let query = normalize_query(query);
        let chars: Vec<char> = query.chars().collect();
        let mut best = 0u32;
        for end in 1..=chars.len() {
            let prefix: String = chars[..end].iter().collect();
            if let Some(count) = h.data.learned.get(&prefix).and_then(|m| m.get(id)) {
                best = best.max(*count);
            }
        }
        total += best.min(10) as i64 * 35;
        total
    }

    #[test]
    fn prepared_scoring_matches_the_original_algorithm() {
        let mut history = History::in_memory();
        let words = ["chrome", "chr", "Éclair", "ch", "c", "notepad", "ñandú", " code ", "x"];
        for round in 0..400usize {
            let id = format!("item:{}", round % 23);
            history.record(&id, words[round % words.len()]);
            if round % 7 == 0 {
                history.record(&id, &format!("{}{}", words[round % words.len()], round % 5));
            }
        }
        for query in ["", "c", "ch", "chr", "chro", "chrome", "Chrome ", "éc", "Éclair", "ñan", "ñandú!", "zzz", " code", "x"] {
            for n in 0..30 {
                let id = format!("item:{n}");
                assert_eq!(history.boost_for_query(&id, query), reference_boost_for_query(&history, &id, query), "id={id} query={query:?}");
            }
        }
    }

    #[test]
    fn record_does_not_wait_for_disk_and_persists_atomically() {
        let dir = std::env::temp_dir().join(format!("okra-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("history.json");
        let mut history = History { data: Data::default(), path: Some(path.clone()), writer: None };
        for _ in 0..3 {
            history.record("app:chrome", "chr");
        }
        // The newest state reaches the file shortly after, written by the background thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let counts = loop {
            let saved = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Data>(&t).ok());
            match saved {
                Some(d) if d.counts.get("app:chrome") == Some(&3) => break d.counts,
                _ if std::time::Instant::now() > deadline => panic!("history was never written"),
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        assert_eq!(counts["app:chrome"], 3);
        assert!(!path.with_extension("json.tmp").exists(), "temp file must not linger");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn learns_which_item_was_picked_for_a_query() {
        let mut history = History::in_memory();
        history.record("app:chrome", "chr");
        history.record("app:chrome", "chr");
        let learned = history.boost_for_query("app:chrome", "chro");
        let other = history.boost_for_query("app:chrome-beta", "chro");
        assert!(learned > other);
        assert!(history.boost("app:chrome") >= 80, "just-used items get the recency bonus");
    }
}
