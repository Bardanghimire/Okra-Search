use super::{Action, Item, Kind, Provider};
use crate::matcher::{score, Query};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use walkdir::WalkDir;

/// How often the app list is re-scanned while the launcher stays resident.
const RESCAN_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Called (from the scanner thread) whenever a fresh app list has replaced the previous one, so a
/// UI can re-run the query currently on screen.
pub type RefreshCallback = Box<dyn Fn() + Send + Sync>;

#[derive(Clone)]
enum AppTarget {
    Shortcut { path: PathBuf, exec: Option<String> },
    AppId(String),
}

struct AppEntry {
    name: String,
    target: AppTarget,
}

/// Installed applications from Start Menu shortcuts and Windows' registered app list.
///
/// Scanning is slow on Windows (it launches PowerShell for `Get-StartApps`), so it never happens
/// on the caller's thread: the previous scan is loaded from a small cache file so search works
/// the instant the launcher starts, and a background thread refreshes the list and the cache.
pub struct AppsProvider {
    apps: Arc<RwLock<Arc<Vec<AppEntry>>>>,
}

impl AppsProvider {
    /// Returns immediately. `on_refresh` fires after every background re-scan.
    pub fn start(on_refresh: Arc<OnceLock<RefreshCallback>>) -> Self {
        Self::start_with_cache(cache_path(), on_refresh)
    }

    fn start_with_cache(cache: Option<PathBuf>, on_refresh: Arc<OnceLock<RefreshCallback>>) -> Self {
        let initial = cache.as_deref().and_then(load_cache).unwrap_or_default();
        let shared: Arc<RwLock<Arc<Vec<AppEntry>>>> = Arc::new(RwLock::new(Arc::new(initial)));
        let background = shared.clone();
        let _ = std::thread::Builder::new().name("okra-app-scan".into()).spawn(move || loop {
            let fresh = scan_all();
            if let Some(path) = &cache {
                save_cache(path, &fresh);
            }
            *background.write().unwrap() = Arc::new(fresh);
            if let Some(callback) = on_refresh.get() {
                callback();
            }
            std::thread::sleep(RESCAN_INTERVAL);
        });
        Self { apps: shared }
    }

    pub fn len(&self) -> usize {
        self.apps.read().unwrap().len()
    }
}

fn scan_all() -> Vec<AppEntry> {
    let mut apps = Vec::new();
    if cfg!(target_os = "windows") {
        scan_windows(&mut apps);
    } else if cfg!(target_os = "macos") {
        scan_macos(&mut apps);
    } else {
        scan_linux(&mut apps);
    }
    apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    apps.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
    apps
}

// ---------------------------------------------------------------- on-disk cache

const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    apps: Vec<CachedApp>,
}

#[derive(Serialize, Deserialize)]
struct CachedApp {
    name: String,
    path: Option<String>,
    exec: Option<String>,
    app_id: Option<String>,
}

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("okra-brown-search").join("apps.json"))
}

fn save_cache(path: &Path, apps: &[AppEntry]) {
    let cached: Vec<CachedApp> = apps
        .iter()
        .filter_map(|app| match &app.target {
            // Paths that are not valid Unicode cannot round-trip through JSON; the next scan finds them again.
            AppTarget::Shortcut { path, exec } => Some(CachedApp {
                name: app.name.clone(),
                path: Some(path.to_str()?.to_string()),
                exec: exec.clone(),
                app_id: None,
            }),
            AppTarget::AppId(id) => Some(CachedApp { name: app.name.clone(), path: None, exec: None, app_id: Some(id.clone()) }),
        })
        .collect();
    let Ok(json) = serde_json::to_vec(&CacheFile { version: CACHE_VERSION, apps: cached }) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write-then-rename so a crash mid-write can never leave a truncated cache behind.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn load_cache(path: &Path) -> Option<Vec<AppEntry>> {
    let file: CacheFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if file.version != CACHE_VERSION {
        return None;
    }
    Some(
        file.apps
            .into_iter()
            .filter_map(|app| {
                let target = match (app.path, app.app_id) {
                    (Some(path), _) => AppTarget::Shortcut { path: PathBuf::from(path), exec: app.exec },
                    (None, Some(id)) => AppTarget::AppId(id),
                    (None, None) => return None,
                };
                Some(AppEntry { name: app.name, target })
            })
            .collect(),
    )
}

fn scan_linux(out: &mut Vec<AppEntry>) {
    let mut dirs: Vec<PathBuf> = vec![
        "/usr/share/applications".into(),
        "/usr/local/share/applications".into(),
        "/var/lib/flatpak/exports/share/applications".into(),
    ];
    if let Some(d) = dirs::data_dir() {
        dirs.push(d.join("applications"));
        dirs.push(d.join("flatpak/exports/share/applications"));
    }
    for d in dirs {
        for e in WalkDir::new(d).max_depth(2).into_iter().flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("desktop") {
                if let Some(a) = parse_desktop(p) {
                    out.push(a);
                }
            }
        }
    }
}

fn parse_desktop(p: &Path) -> Option<AppEntry> {
    let text = std::fs::read_to_string(p).ok()?;
    let mut in_entry = false;
    let (mut name, mut exec): (Option<String>, Option<String>) = (None, None);
    let (mut hidden, mut is_app) = (false, true);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "Name" => name = Some(v.trim().to_string()),
                "Exec" => exec = Some(clean_exec(v.trim())),
                "NoDisplay" | "Hidden" => {
                    if v.trim() == "true" {
                        hidden = true;
                    }
                }
                "Type" => is_app = v.trim() == "Application",
                _ => {}
            }
        }
    }
    if hidden || !is_app {
        return None;
    }
    // Linux .desktop launch support remains deliberately small in this Windows-focused build.
    Some(AppEntry { name: name?, target: AppTarget::Shortcut { path: p.to_path_buf(), exec: Some(exec?) } })
}

/// Strip freedesktop field codes (%f %U ...) from an Exec line.
fn clean_exec(s: &str) -> String {
    s.split_whitespace()
        .filter(|t| !(t.len() == 2 && t.starts_with('%') && *t != "%%"))
        .collect::<Vec<_>>()
        .join(" ")
        .replace("%%", "%")
}

fn scan_windows(out: &mut Vec<AppEntry>) {
    let rel = Path::new("Microsoft").join("Windows").join("Start Menu").join("Programs");
    for var in ["ProgramData", "APPDATA"] {
        let Some(base) = std::env::var_os(var) else { continue };
        for e in WalkDir::new(PathBuf::from(base).join(&rel)).max_depth(5).into_iter().flatten() {
            let p = e.path();
            let is_lnk = p.extension().and_then(|x| x.to_str()).map_or(false, |x| x.eq_ignore_ascii_case("lnk"));
            let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if is_lnk && !stem.to_lowercase().contains("uninstall") {
                out.push(AppEntry { name: stem, target: AppTarget::Shortcut { path: p.to_path_buf(), exec: None } });
            }
        }
    }

    // Get-StartApps also returns packaged/UWP applications that do not have ordinary .lnk files.
    let script = "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new(); Get-StartApps | Select-Object Name, AppID | ConvertTo-Json -Compress";
    let mut command = std::process::Command::new("powershell.exe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let output = command
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                let entries = value.as_array().cloned().unwrap_or_else(|| vec![value]);
                for row in entries {
                    let name = row.get("Name").and_then(|v| v.as_str()).unwrap_or_default().trim();
                    let app_id = row.get("AppID").and_then(|v| v.as_str()).unwrap_or_default().trim();
                    if !name.is_empty() && !app_id.is_empty() {
                        out.push(AppEntry { name: name.to_string(), target: AppTarget::AppId(app_id.to_string()) });
                    }
                }
            }
        }
    }
}

fn scan_macos(out: &mut Vec<AppEntry>) {
    let mut roots: Vec<PathBuf> = ["/Applications", "/Applications/Utilities", "/System/Applications", "/System/Applications/Utilities"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(h) = dirs::home_dir() {
        roots.push(h.join("Applications"));
    }
    for r in roots {
        let Ok(rd) = std::fs::read_dir(r) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x == "app") {
                let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                out.push(AppEntry { name, target: AppTarget::Shortcut { path: p, exec: None } });
            }
        }
    }
}

impl Provider for AppsProvider {
    fn name(&self) -> &'static str {
        "Applications"
    }
    fn prefix(&self) -> Option<&'static str> {
        Some("a")
    }
    fn query(&self, q: &str, limit: usize) -> Vec<Item> {
        let query = Query::new(q);
        // Snapshot the current list; a background re-scan swaps in a new one without blocking us.
        let apps = self.apps.read().unwrap().clone();
        let mut items: Vec<Item> = apps
            .iter()
            .filter_map(|app| {
                let rank = if query.is_empty() { 0 } else { score(&query, &app.name)? + 30 };
                let (id, subtitle, action) = match &app.target {
                    AppTarget::Shortcut { path, exec } => (
                        format!("app:{}", path.display()),
                        path.display().to_string(),
                        Action::Launch { path: path.clone(), exec: exec.clone() },
                    ),
                    AppTarget::AppId(app_id) => (
                        format!("app-id:{app_id}"),
                        "Installed Windows app".to_string(),
                        Action::LaunchAppId(app_id.clone()),
                    ),
                };
                Some(Item { id, title: app.name.clone(), subtitle, kind: Kind::App, action, score: rank })
            })
            .collect();
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
        items.truncate(limit);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, target: AppTarget) -> AppEntry {
        AppEntry { name: name.into(), target }
    }

    #[test]
    fn cache_round_trips_and_serves_before_any_scan() {
        let dir = std::env::temp_dir().join(format!("okra-apps-cache-{}", std::process::id()));
        let path = dir.join("apps.json");
        let _ = std::fs::remove_dir_all(&dir);
        save_cache(
            &path,
            &[
                entry("Firefox", AppTarget::Shortcut { path: "/x/firefox.lnk".into(), exec: None }),
                entry("Terminal", AppTarget::Shortcut { path: "/x/t.desktop".into(), exec: Some("xterm".into()) }),
                entry("Calculator", AppTarget::AppId("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".into())),
            ],
        );
        let loaded = load_cache(&path).expect("cache loads");
        assert_eq!(loaded.len(), 3);
        assert!(matches!(&loaded[2].target, AppTarget::AppId(id) if id.starts_with("Microsoft.WindowsCalculator")));
        assert!(matches!(&loaded[1].target, AppTarget::Shortcut { exec: Some(e), .. } if e == "xterm"));
        assert!(!path.with_extension("json.tmp").exists(), "temp file must not linger");

        // Corrupt or wrong-version caches are ignored rather than crashing startup.
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_cache(&path).is_none());
        std::fs::write(&path, br#"{"version":999,"apps":[]}"#).unwrap();
        assert!(load_cache(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_is_instant_serves_cache_and_notifies_after_rescan() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = std::env::temp_dir().join(format!("okra-apps-start-{}", std::process::id()));
        let path = dir.join("apps.json");
        let _ = std::fs::remove_dir_all(&dir);
        save_cache(&path, &[entry("Cached Only App", AppTarget::Shortcut { path: "/x/c.lnk".into(), exec: None })]);

        let refreshes = Arc::new(AtomicUsize::new(0));
        let notify: Arc<OnceLock<RefreshCallback>> = Arc::new(OnceLock::new());
        let counter = refreshes.clone();
        let _ = notify.set(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

        let began = std::time::Instant::now();
        let provider = AppsProvider::start_with_cache(Some(path.clone()), notify);
        let startup = began.elapsed();
        // The cached entry is searchable right away, before/regardless of the background scan.
        assert!(startup < Duration::from_millis(250), "start() blocked for {startup:?}");
        let initial = provider.query("cached only", 5);
        let scanned_already = refreshes.load(Ordering::SeqCst) > 0;
        assert!(scanned_already || initial.iter().any(|i| i.title == "Cached Only App"));

        // The background scan completes and notifies.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while refreshes.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(refreshes.load(Ordering::SeqCst) >= 1, "refresh callback never fired");
        // ... and rewrote the cache from the fresh scan (which does not contain our fake app).
        let rewritten = load_cache(&path).expect("cache rewritten");
        assert!(rewritten.iter().all(|a| a.name != "Cached Only App"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exec_cleanup() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
        assert_eq!(clean_exec("code --new-window %F"), "code --new-window");
        assert_eq!(clean_exec("sh -c 'echo 100%%'"), "sh -c 'echo 100%'");
    }
}
