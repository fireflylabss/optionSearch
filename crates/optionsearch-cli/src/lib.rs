//! optionSearch CLI shared library.
//!
//! `optionsearch` is the thin binary over this crate (`nld` stays as a compat
//! alias); the desktop `optionsearch-gtk` binary also
//! routes any command-line arguments through it, so one binary keeps the
//! Everything-style feel (no arguments opens the search window).
//! routes any command-line arguments through it, so one binary keeps the
//! Everything-style feel (no arguments opens the search window).

pub mod cli;
pub mod commands;
