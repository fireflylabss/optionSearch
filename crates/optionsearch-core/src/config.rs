//! Local-first configuration under `~/.option/search`.
//!
//! The canonical config is `~/.option/search/config.toml` (honors
//! `$OPTION_HOME` via the SDK). Older XDG locations are still honored as
//! read-only fallbacks so the needle → optionSearch rename loses nothing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Directories that are excluded from the *watcher* by default: they are still
/// indexed, but they burn inotify watches for churn nobody searches for.
pub const DEFAULT_WATCH_EXCLUDES: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    ".cache",
    ".venv",
    "__pycache__",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Roots to index. Defaults to `$HOME`.
    pub roots: Vec<PathBuf>,
    /// Directory *names* skipped by the scanner. Empty by default: the index is
    /// meant to be complete, like Everything's.
    pub scan_excludes: Vec<String>,
    /// Absolute paths skipped by the scanner.
    pub exclude_paths: Vec<PathBuf>,
    /// Directory names the watcher will not watch (still indexed).
    pub watch_excludes: Vec<String>,
    /// Maximum hits returned to the UI.
    pub max_results: usize,
    /// Upper bound on inotify watches the watcher may take.
    pub watch_budget: usize,
    /// Debounce window for filesystem events, in milliseconds.
    pub watch_debounce_ms: u64,
    pub db_path: PathBuf,
    pub cache_dir: PathBuf,
}

/// The optionSearch app identity under `~/.option/search`.
///
/// Built with [`option_sdk::App::new`] rather than a family constant so it
/// compiles against published optionSDK releases (which may predate a
/// dedicated `SEARCH` constant). Matches the `search`/`⌕`/`optionSearch`
/// identity and `io.option.search` bundle id.
pub fn search_app() -> option_sdk::App {
    option_sdk::App::new("search", "⌕", "optionSearch")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: vec![option_sdk::home_dir()],
            scan_excludes: Vec::new(),
            exclude_paths: Vec::new(),
            watch_excludes: DEFAULT_WATCH_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            max_results: 500,
            watch_budget: 400_000,
            watch_debounce_ms: 200,
            db_path: search_app().path("index.sqlite3"),
            cache_dir: search_app().cache_dir(),
        }
    }
}

impl Config {
    /// Loads the canonical SDK config, falling back to older XDG locations.
    ///
    /// First existing wins: `~/.option/search/config.toml` →
    /// `~/.config/optionsearch/config.toml` → `~/.config/needle/config.toml` →
    /// built-in defaults.
    pub fn load() -> Result<Config> {
        Self::load_from_chain(&Self::fallback_chain())
    }

    /// Ordered candidate paths for [`Config::load`]: canonical first.
    pub fn fallback_chain() -> Vec<PathBuf> {
        let home = option_sdk::home_dir();
        vec![
            Self::default_path(),
            home.join(".config")
                .join("optionsearch")
                .join("config.toml"),
            home.join(".config").join("needle").join("config.toml"),
        ]
    }

    /// First existing path wins; returns defaults when none exists.
    /// Testable with explicit paths — never touches the real home itself.
    pub fn load_from_chain(paths: &[PathBuf]) -> Result<Config> {
        for path in paths {
            if path.exists() {
                return Self::load_from(path);
            }
        }
        Ok(Config::default())
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::default_path())
    }

    /// Atomically writes the config to `path` (temp-file + fsync + rename).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        option_sdk::atomic_write(path, text.as_bytes())
            .with_context(|| format!("writing config {}", path.display()))?;
        Ok(())
    }

    /// Canonical config path: `~/.option/search/config.toml` (honors `$OPTION_HOME`).
    pub fn default_path() -> PathBuf {
        search_app().config_toml()
    }

    /// Creates the directories the engine writes to.
    pub fn ensure_dirs(&self) -> Result<()> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&self.cache_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_sdk_layout() {
        let c = Config::default();
        assert_eq!(
            c.db_path.file_name().and_then(|s| s.to_str()),
            Some("index.sqlite3")
        );
        assert_eq!(c.db_path, search_app().path("index.sqlite3"));
        assert_eq!(c.cache_dir, search_app().cache_dir());
        assert_eq!(c.max_results, 500);
        assert_eq!(Config::default_path(), search_app().config_toml());
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let c = Config::default();
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.roots, c.roots);
        assert_eq!(back.watch_excludes, c.watch_excludes);
    }

    #[test]
    fn partial_config_fills_defaults() {
        let c: Config = toml::from_str("max_results = 42").unwrap();
        assert_eq!(c.max_results, 42);
        assert_eq!(c.watch_debounce_ms, 200);
    }

    #[test]
    fn atomic_save_roundtrips_to_temp_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("config.toml");
        let c = Config {
            max_results: 123,
            roots: vec![tmp.path().join("music")],
            ..Config::default()
        };
        c.save_to(&path).unwrap();
        assert!(path.exists());
        let back = Config::load_from(&path).unwrap();
        assert_eq!(back.max_results, 123);
        assert_eq!(back.roots, c.roots);
        assert_eq!(
            back.db_path.file_name().and_then(|s| s.to_str()),
            Some("index.sqlite3")
        );
    }

    #[test]
    fn chain_prefers_first_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("a.toml");
        let second = tmp.path().join("b.toml");
        let missing = tmp.path().join("missing.toml");
        std::fs::write(&first, "max_results = 11").unwrap();
        std::fs::write(&second, "max_results = 22").unwrap();
        let c = Config::load_from_chain(&[missing, first.clone(), second.clone()]).unwrap();
        assert_eq!(c.max_results, 11);
        let c = Config::load_from_chain(std::slice::from_ref(&second)).unwrap();
        assert_eq!(c.max_results, 22);
    }

    #[test]
    fn chain_falls_back_to_defaults_when_nothing_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Config::load_from_chain(&[tmp.path().join("nope.toml")]).unwrap();
        assert_eq!(c.max_results, 500);
        let c = Config::load_from_chain(&[]).unwrap();
        assert_eq!(c.max_results, 500);
    }

    #[test]
    fn fallback_chain_ordering() {
        let chain = Config::fallback_chain();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], search_app().config_toml());
        assert!(chain[1].ends_with(".config/optionsearch/config.toml"));
        assert!(chain[2].ends_with(".config/needle/config.toml"));
    }
}
