use std::path::PathBuf;

pub mod apps;
pub mod calc;
pub mod everything;
pub mod files;
pub mod settings;
pub mod windows;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    App,
    File,
    Folder,
    Setting,
    Window,
    Calculation,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::App => "Application",
            Kind::File => "File",
            Kind::Folder => "Folder",
            Kind::Setting => "Windows setting",
            Kind::Window => "Open window",
            Kind::Calculation => "Calculation",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    Launch { path: PathBuf, exec: Option<String> },
    LaunchAppId(String),
    Run { program: String, args: Vec<String> },
    OpenPath(PathBuf),
    OpenUri(String),
    Copy(String),
    FocusWindow(isize),
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub kind: Kind,
    pub action: Action,
    pub score: i64,
}

/// A source searched by the local engine. Search sources are built into the app;
/// Okra Brown Search deliberately has no third-party plugin system.
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Exclusive trigger: `"f report"` routes only to the file provider.
    fn prefix(&self) -> Option<&'static str> {
        None
    }
    /// Whether this source participates in un-prefixed searches.
    fn in_default(&self) -> bool {
        true
    }
    fn query(&self, q: &str, limit: usize) -> Vec<Item>;
}
