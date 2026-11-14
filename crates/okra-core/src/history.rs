use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
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
        Self { data, path }
    }

    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Frequency plus recency, independent of what was typed.
    pub fn boost(&self, id: &str) -> i64 {
        let frequency = self.data.counts.get(id).copied().unwrap_or(0).min(25) as i64 * 8;
        let recency = match self.data.last.get(id) {
            Some(&when) => {
                let age = now_secs().saturating_sub(when);
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
        let mut total = self.boost(id);
        let query = normalize_query(query);
        let chars: Vec<char> = query.chars().collect();
        let mut best = 0u32;
        for end in 1..=chars.len() {
            let prefix: String = chars[..end].iter().collect();
            if let Some(count) = self.data.learned.get(&prefix).and_then(|m| m.get(id)) {
                best = best.max(*count);
            }
        }
        total += best.min(10) as i64 * 35;
        total
    }

    pub fn record(&mut self, id: &str, query: &str) {
        *self.data.counts.entry(id.to_string()).or_insert(0) += 1;
        self.data.last.insert(id.to_string(), now_secs());
        let query = normalize_query(query);
        if !query.is_empty() && query.chars().count() <= 40 {
            *self.data.learned.entry(query).or_default().entry(id.to_string()).or_insert(0) += 1;
        }
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string(&self.data) {
                let _ = std::fs::write(path, json);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
