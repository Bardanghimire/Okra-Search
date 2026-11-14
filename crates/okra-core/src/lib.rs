//! Search engine for Okra Brown Search.
//! Built-in sources: applications, Everything-backed files, Windows Settings,
//! and open desktop windows. The engine has no third-party plugin mechanism.

pub mod config;
pub mod engine;
pub mod history;
pub mod matcher;
pub mod providers;

pub use config::{Config, FileBackendKind, ThemeKind};
pub use engine::{Engine, Outcome};
pub use providers::{Action, Item, Kind, Provider};
