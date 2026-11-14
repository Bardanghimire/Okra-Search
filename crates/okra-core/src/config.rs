use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FileBackendKind {
    /// Everything SDK -> es.exe -> built-in index.
    #[default]
    Auto,
    /// Only Everything (no fallback index).
    Everything,
    /// Never use Everything.
    Builtin,
}

impl FileBackendKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic (recommended)",
            Self::Everything => "Everything only",
            Self::Builtin => "Built-in index only",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeKind {
    #[default]
    WarmLight,
    Dark,
}

impl ThemeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::WarmLight => "Light",
            Self::Dark => "Dark",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub file_backend: FileBackendKind,
    /// Optional explicit path to Everything64.dll / Everything32.dll (from the Everything SDK).
    pub everything_dll: Option<PathBuf>,
    /// Optional explicit path to es.exe.
    pub es_exe: Option<PathBuf>,
    pub file_roots: Vec<PathBuf>,
    pub file_max_depth: usize,
    pub max_results: usize,
    pub theme: ThemeKind,
    pub start_with_windows: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            file_backend: FileBackendKind::Auto,
            everything_dll: None,
            es_exe: None,
            file_roots: dirs::home_dir().into_iter().collect(),
            file_max_depth: 8,
            max_results: 40,
            theme: ThemeKind::WarmLight,
            start_with_windows: false,
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("okra-brown-search").join("config.json"))
    }

    /// Load the user's config, writing the defaults on first run so they can be edited.
    pub fn load_or_default() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str(&text) {
                return cfg;
            }
        }
        let cfg = Self::default();
        let _ = cfg.save();
        cfg
    }

    pub fn save(&self) -> Result<(), String> {
        let Some(path) = Self::path() else {
            return Err("Could not resolve the application config directory".into());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Could not create settings folder: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("Could not serialize settings: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("Could not save settings: {e}"))
    }
}
