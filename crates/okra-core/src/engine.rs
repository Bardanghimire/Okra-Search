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
}

impl Engine {
    pub fn new(cfg: &Config) -> Self {
        // Okra Brown Search is intentionally a focused, built-in Windows search tool:
        // apps, Everything-backed files, Windows Settings, and open windows.
        //
        // Construction never blocks on slow discovery: the app list comes from an on-disk cache
        // and is refreshed on a background thread (see `set_on_index_refresh`).
        let on_index_refresh: Arc<OnceLock<RefreshCallback>> = Arc::new(OnceLock::new());
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(CalcProvider),
            Box::new(AppsProvider::start(on_index_refresh.clone())),
            Box::new(FilesProvider::new(cfg)),
            Box::new(SettingsProvider),
            Box::new(WindowsProvider),
        ];
        let mut engine = Self::with_providers(providers, History::load(), cfg.max_results);
        engine.on_index_refresh = on_index_refresh;
        engine
    }

    pub fn with_providers(providers: Vec<Box<dyn Provider>>, history: History, max_results: usize) -> Self {
        Self { providers, history: Mutex::new(history), max_results, on_index_refresh: Arc::new(OnceLock::new()) }
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

    pub fn search(&self, raw: &str) -> Vec<Item> {
        if raw.trim().is_empty() {
            return self.suggestions();
        }
        let (providers, query) = self.route(raw.trim_start());
        let mut items: Vec<Item> = providers.iter().flat_map(|p| p.query(query, self.max_results)).collect();
        {
            let history = self.history.lock().unwrap();
            for item in &mut items {
                item.score += Self::kind_bias(item.kind) + history.boost_for_query(&item.id, raw);
            }
        }
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        let mut seen = HashSet::new();
        items.retain(|item| seen.insert(item.id.clone()));
        items.truncate(self.max_results);
        items
    }

    /// What to show before anything is typed: the apps used most and most recently, topped up
    /// with a few well-known ones so a fresh install is never an empty box.
    fn suggestions(&self) -> Vec<Item> {
        const COUNT: usize = 7;
        const DEFAULTS: [&str; 10] = [
            "File Explorer",
            "Google Chrome",
            "Microsoft Edge",
            "Firefox",
            "Visual Studio Code",
            "Notepad",
            "Calculator",
            "Terminal",
            "Settings",
            "Task Manager",
        ];
        let mut apps: Vec<Item> = self
            .providers
            .iter()
            .filter(|p| p.name() == "Applications")
            .flat_map(|p| p.query("", usize::MAX))
            .collect();
        {
            let history = self.history.lock().unwrap();
            for item in &mut apps {
                item.score = history.boost(&item.id);
            }
        }
        apps.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        let mut out: Vec<Item> = apps.iter().filter(|item| item.score > 0).take(COUNT).cloned().collect();
        for wanted in DEFAULTS {
            if out.len() >= COUNT {
                break;
            }
            if let Some(item) = apps.iter().find(|item| item.score <= 0 && item.title.eq_ignore_ascii_case(wanted)) {
                out.push(item.clone());
            }
        }
        // Nothing used yet and none of the usual suspects installed: still show something.
        if out.len() < COUNT {
            let missing: Vec<Item> = apps
                .iter()
                .filter(|item| !out.iter().any(|picked| picked.id == item.id))
                .take(COUNT - out.len())
                .cloned()
                .collect();
            out.extend(missing);
        }
        out.extend(recent_files(4));
        out
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

/// The files Windows lists under "Recent", newest first. Opening the shortcut opens the file.
fn recent_files(limit: usize) -> Vec<Item> {
    let Some(dir) = dirs::data_dir().map(|d| d.join("Microsoft").join("Windows").join("Recent")) else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf)> = read
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let is_lnk = path.extension().and_then(|x| x.to_str()).map_or(false, |x| x.eq_ignore_ascii_case("lnk"));
            if !is_lnk {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    entries
        .into_iter()
        .take(limit)
        .map(|(_, path)| Item {
            id: format!("recent:{}", path.display()),
            title: path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            subtitle: "Recently opened".to_string(),
            kind: Kind::File,
            action: Action::OpenPath(path),
            score: 0,
        })
        .collect()
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
}
