use crate::config::Config;
use crate::history::History;
use crate::providers::{
    apps::{AppsProvider, RefreshCallback},
    calc::CalcProvider,
    files::FilesProvider,
    settings::SettingsProvider,
    windows::WindowsProvider,
    Action, Item, Kind, Provider,
};
use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

/// What the UI must do after an item was activated.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    CopyText(String),
}

pub struct Engine {
    providers: Vec<Box<dyn Provider>>,
    history: Mutex<History>,
    max_results: usize,
    on_index_refresh: Arc<OnceLock<RefreshCallback>>,
    /// Something the user should know about from the start (for example: no Everything found).
    notice: Option<String>,
}

impl Engine {
    pub fn new(cfg: &Config) -> Self {
        // Okra Brown Search is intentionally a focused, built-in Windows search tool:
        // apps, Everything-backed files, Windows Settings, and open windows.
        //
        // Construction never blocks on slow discovery: the app list comes from an on-disk cache
        // and is refreshed on a background thread (see `set_on_index_refresh`).
        let on_index_refresh: Arc<OnceLock<RefreshCallback>> = Arc::new(OnceLock::new());
        let files = FilesProvider::new(cfg);
        let notice = files.startup_notice();
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(CalcProvider),
            Box::new(AppsProvider::start(on_index_refresh.clone())),
            Box::new(files),
            Box::new(SettingsProvider),
            Box::new(WindowsProvider::default()),
        ];
        let mut engine = Self::with_providers(providers, History::load(), cfg.max_results);
        engine.on_index_refresh = on_index_refresh;
        engine.notice = notice;
        engine
    }

    /// How many times an item was opened and when it last was (unix seconds), for the preview pane.
    pub fn usage(&self, id: &str) -> Option<(u32, u64)> {
        self.history.lock().unwrap().usage(id)
    }

    /// A one-line heads-up decided at startup (None when everything is as expected).
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn with_providers(providers: Vec<Box<dyn Provider>>, history: History, max_results: usize) -> Self {
        Self { providers, history: Mutex::new(history), max_results, on_index_refresh: Arc::new(OnceLock::new()), notice: None }
    }

    /// Register a callback fired (from a background thread) whenever the application list has been
    /// refreshed, so the UI can re-run the query that is currently displayed. Can be set once.
    pub fn set_on_index_refresh(&self, callback: impl Fn() + Send + Sync + 'static) {
        let _ = self.on_index_refresh.set(Box::new(callback));
    }

    /// `f invoice` -> only the provider whose prefix is `f`, query `invoice`.
    fn route<'a>(&'a self, raw: &'a str) -> (Vec<&'a dyn Provider>, &'a str) {
        for provider in &self.providers {
            if let Some(prefix) = provider.prefix() {
                if let Some(rest) = raw.strip_prefix(prefix).and_then(|r| r.strip_prefix(' ')) {
                    return (vec![provider.as_ref()], rest.trim());
                }
            }
        }
        (self.providers.iter().filter(|p| p.in_default()).map(|p| p.as_ref()).collect(), raw.trim())
    }

    /// Applications first: someone typing "chr" wants Chrome, not `chrome_installer.log`.
    fn kind_bias(kind: Kind) -> i64 {
        match kind {
            Kind::Calculation => 0,
            Kind::App => 70,
            Kind::Window => 40,
            Kind::Setting => 10,
            Kind::Folder | Kind::File => 0,
        }
    }

    /// Blocking search: the final, fully ranked list.
    pub fn search(&self, raw: &str) -> Vec<Item> {
        let mut all = Vec::new();
        self.search_progressive(raw, &mut |items| all = items, &|| false);
        all
    }

    /// Search that hands results over as soon as they exist. Instant sources (calculator, apps,
    /// settings, open windows) are ranked and emitted first; the file search, the only slow one,
    /// follows with the complete list. `stale` is polled between the phases so a query the user
    /// has already typed past never pays for the file search. An empty query emits nothing:
    /// the launcher shows only its search bar until something is typed.
    pub fn search_progressive(&self, raw: &str, emit: &mut dyn FnMut(Vec<Item>), stale: &dyn Fn() -> bool) {
        if raw.trim().is_empty() {
            emit(Vec::new());
            return;
        }
        let (providers, query) = self.route(raw.trim_start());
        let (slow, fast): (Vec<&dyn Provider>, Vec<&dyn Provider>) =
            providers.into_iter().partition(|p| p.name() == "Files");
        let mut items: Vec<Item> = fast.iter().flat_map(|p| p.query(query, self.max_results)).collect();
        if slow.is_empty() {
            emit(self.rank(items, raw));
            return;
        }
        if !items.is_empty() {
            emit(self.rank(items.clone(), raw));
        }
        if stale() {
            return;
        }
        for provider in &slow {
            items.extend(provider.query(query, self.max_results));
        }
        emit(self.rank(items, raw));
    }

    /// Apply usage history and kind bias, order, drop duplicate ids and cap the list.
    fn rank(&self, mut items: Vec<Item>, raw: &str) -> Vec<Item> {
        {
            let history = self.history.lock().unwrap();
            let boost = history.for_query(raw);
            for item in &mut items {
                item.score += Self::kind_bias(item.kind) + boost.boost(&item.id);
            }
        }
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        // Drop duplicate ids (first, i.e. best, wins) without cloning every id into a set.
        let keep: Vec<bool> = {
            let mut seen = HashSet::with_capacity(items.len());
            items.iter().map(|item| seen.insert(item.id.as_str())).collect()
        };
        let mut keep = keep.into_iter();
        items.retain(|_| keep.next().unwrap_or(true));
        items.truncate(self.max_results);
        items
    }

    pub fn activate(&self, item: &Item) -> Result<Outcome, String> {
        self.activate_for(item, "")
    }

    /// Like [`activate`], remembering which `query` led to this pick so it ranks higher next time.
    pub fn activate_for(&self, item: &Item, query: &str) -> Result<Outcome, String> {
        match &item.action {
            Action::Launch { path, exec } => {
                if let Some(command) = exec {
                    #[cfg(windows)]
                    {
                        let _ = command;
                        open::that_detached(path).map_err(|e| e.to_string())?;
                    }
                    #[cfg(not(windows))]
                    {
                        Command::new("sh")
                            .arg("-c")
                            .arg(command)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .map_err(|e| e.to_string())?;
                    }
                } else {
                    open::that_detached(path).map_err(|e| e.to_string())?;
                }
            }
            Action::LaunchAppId(app_id) => {
                #[cfg(windows)]
                {
                    Command::new("explorer.exe")
                        .arg(format!("shell:AppsFolder\\{app_id}"))
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(|e| e.to_string())?;
                }
                #[cfg(not(windows))]
                return Err("Packaged Windows applications can only be launched on Windows".into());
            }
            Action::Run { program, args } => {
                Command::new(program)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| format!("Could not start {program}: {e}"))?;
            }
            Action::OpenPath(path) => {
                open::that_detached(path).map_err(|e| e.to_string())?;
            }
            Action::OpenUri(uri) => {
                open::that_detached(uri).map_err(|e| format!("Could not open {uri}: {e}"))?;
            }
            Action::Copy(text) => {
                self.history.lock().unwrap().record(&item.id, query);
                return Ok(Outcome::CopyText(text.clone()));
            }
            Action::FocusWindow(hwnd) => {
                crate::providers::windows::WindowsProvider::focus(*hwnd)?;
            }
        }
        self.history.lock().unwrap().record(&item.id, query);
        Ok(Outcome::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::History;
    use crate::providers::Kind;

    struct Fake(&'static str, Option<&'static str>, bool);
    impl Provider for Fake {
        fn name(&self) -> &'static str { self.0 }
        fn prefix(&self) -> Option<&'static str> { self.1 }
        fn in_default(&self) -> bool { self.2 }
        fn query(&self, q: &str, _limit: usize) -> Vec<Item> {
            vec![Item {
                id: format!("{}:{}", self.0, q),
                title: format!("{}-{}", self.0, q),
                subtitle: String::new(),
                kind: Kind::App,
                action: Action::Copy(q.into()),
                score: 10,
            }]
        }
    }

    #[test]
    fn prefix_routes_exclusively() {
        let engine = Engine::with_providers(
            vec![Box::new(Fake("a", None, true)), Box::new(Fake("f", Some("f"), false))],
            History::in_memory(),
            10,
        );
        let results = engine.search("f report");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "f:report");
        let results = engine.search("editor");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a:editor");
    }

    #[test]
    fn fast_results_arrive_before_slow_ones_and_stale_queries_skip_the_slow_phase() {
        let engine = Engine::with_providers(
            vec![Box::new(Fake("a", None, true)), Box::new(Fake("Files", None, true))],
            History::in_memory(),
            10,
        );
        let mut batches: Vec<Vec<String>> = Vec::new();
        engine.search_progressive("x", &mut |items| batches.push(items.into_iter().map(|i| i.id).collect()), &|| false);
        assert_eq!(batches.len(), 2, "instant results first, then everything");
        assert_eq!(batches[0], vec!["a:x".to_string()]);
        assert_eq!(batches[1].len(), 2);

        let mut emitted = 0;
        engine.search_progressive("x", &mut |_| emitted += 1, &|| true);
        assert_eq!(emitted, 1, "a stale query only runs the instant phase");

        let mut idle = Vec::new();
        engine.search_progressive("   ", &mut |items| idle.push(items.len()), &|| false);
        assert_eq!(idle, vec![0], "an empty query shows nothing");
        assert!(engine.search("").is_empty());
    }

    #[test]
    fn history_boosts_ranking() {
        let engine = Engine::with_providers(
            vec![Box::new(Fake("a", None, true)), Box::new(Fake("b", None, true))],
            History::in_memory(),
            10,
        );
        let items = engine.search("x");
        let second = items[1].clone();
        for _ in 0..3 {
            assert_eq!(engine.activate(&second).unwrap(), Outcome::CopyText("x".into()));
        }
        assert_eq!(engine.search("x")[0].id, second.id);
    }

    struct Many(usize);
    impl Provider for Many {
        fn name(&self) -> &'static str { "many" }
        fn query(&self, q: &str, _limit: usize) -> Vec<Item> {
            (0..self.0)
                .map(|i| Item {
                    id: format!("item:{i}"),
                    title: format!("item {i} {q}"),
                    subtitle: String::new(),
                    kind: Kind::File,
                    action: Action::Copy(String::new()),
                    score: (i % 97) as i64,
                })
                .collect()
        }
    }

    /// `cargo test --release -p okra-core -- --ignored --nocapture bench_engine_overhead`
    #[test]
    #[ignore]
    fn bench_engine_overhead() {
        let mut history = History::in_memory();
        for n in 0..600 {
            history.record(&format!("item:{}", n % 300), &format!("query{n}"));
        }
        let engine = Engine::with_providers(vec![Box::new(Many(300))], history, 40);
        let provider = Many(300);
        let reps = 3000;
        let _ = engine.search("chromium");
        let t = std::time::Instant::now();
        for _ in 0..reps {
            std::hint::black_box(provider.query("chromium", 40));
        }
        let provider_us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;
        let t = std::time::Instant::now();
        for _ in 0..reps {
            std::hint::black_box(engine.search("chromium"));
        }
        let total_us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;
        println!("engine.search, 300 results: total {total_us:.1} us, provider alone {provider_us:.1} us, engine overhead {:.1} us", total_us - provider_us);
    }
}
