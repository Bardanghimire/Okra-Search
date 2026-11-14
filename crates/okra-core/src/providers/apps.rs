use super::{Action, Item, Kind, Provider};
use crate::matcher::fuzzy;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

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
pub struct AppsProvider {
    apps: Vec<AppEntry>,
}

impl AppsProvider {
    pub fn scan() -> Self {
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
        Self { apps }
    }

    pub fn len(&self) -> usize {
        self.apps.len()
    }
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
        let mut items: Vec<Item> = self.apps
            .iter()
            .filter_map(|app| {
                let score = if q.is_empty() { 0 } else { fuzzy(q, &app.name)?.score + 30 };
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
                Some(Item { id, title: app.name.clone(), subtitle, kind: Kind::App, action, score })
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

    #[test]
    fn exec_cleanup() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
        assert_eq!(clean_exec("code --new-window %F"), "code --new-window");
        assert_eq!(clean_exec("sh -c 'echo 100%%'"), "sh -c 'echo 100%'");
    }
}
