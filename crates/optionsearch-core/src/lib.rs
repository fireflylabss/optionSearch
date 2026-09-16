//! optionSearch search engine core.
//!
//! The engine keeps every indexed name in memory (a byte arena plus columnar
//! metadata) and uses SQLite purely as persistence between runs. Searching is a
//! parallel scan over that in-memory index; full paths are only materialized for
//! the handful of results that are actually returned.

pub mod arena;
pub mod config;
pub mod db;
pub mod engine;
pub mod index;
pub mod model;
pub mod preview;
pub mod query;
pub mod scan;
pub mod search;
pub mod watch;

pub use config::Config;
pub use engine::{Engine, EngineStats, IndexEvent};
pub use index::Index;
pub use model::{Entry, Hit, SearchResults, flags};
pub use preview::{FileMeta, Preview, PreviewLimits, preview};
pub use query::Query;
pub use watch::{WatchOptions, WatchStatus, Watcher};
