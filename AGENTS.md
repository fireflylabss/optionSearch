# AGENTS.md — optionSearch

Guidance for coding agents working on this repo.

## Product

**optionSearch** — instant, local-first Linux file search. SQLite-backed index plus a compact in-memory index of names and paths, so the database never sits in the keystroke path.

Two surfaces, one shared workspace version: **CLI** (`optionsearch` canonical · `nld` compat alias) and **Desktop** (`optionsearch-gtk` canonical · `needle` compat alias, bundle `io.option.search`).

Local-first. No network. No daemon. No account, sync, telemetry, or remote API. State and config via `optionSDK` under `~/.option/search`.

## After every change

When you finish a task that touches code (features, fixes, UI, deps):

1. **Format, test, lint, build** — always verify with the workspace:

   ```bash
   cargo fmt --all
   cargo test --release -p optionsearch-core
   cargo clippy --release --all-targets -- -D warnings
   cargo build --release --workspace
   ```

2. **Install to PATH** — always refresh the local binaries so `optionsearch` / `nld` and `optionsearch-gtk` / `needle` match the working tree:

   ```bash
   cargo install --path crates/optionsearch-cli --force --offline
   cargo install --path crates/optionsearch-gui --force --offline
   ```

   Use `--offline` when deps are already fetched; drop it if the lockfile needs network.

Do **not** leave the user on stale `~/.cargo/bin/optionsearch` / `optionsearch-gtk` binaries after finishing work.

## Stack notes

- Workspace crates: `crates/optionsearch-core` (in-memory index, SQLite persistence, parallel matcher, watcher, previews) + `crates/optionsearch-cli` (`optionsearch` + `nld`) + `crates/optionsearch-gui` (`optionsearch-gtk` + `needle`, GTK4/libadwaita window)
- `optionSDK` via workspace path `../optionSDK` (`App::new`, `ensure()`, `migrate_dir`, atomic writes); optionSearch adds `search_app()` (identity) and `prepare_state()` (migrate + ensure) shared by CLI and GUI
- State: `~/.option/search/` (canonical), honored via `$OPTION_HOME`; one-time migration of legacy `~/.option/needle` on first CLI **or** GUI run
- Config canonical at `~/.option/search/config.toml` with `~/.config/optionsearch` → `~/.config/needle` fallbacks; index DB at `~/.option/search/index.sqlite3`; config writes are atomic
- Watcher uses Linux inotify through `notify` as an intentional foreground process (supervisable via a systemd user unit)

## Release channels

See [VERSIONING.md](VERSIONING.md) and the blurb in [CHANGELOG.md](CHANGELOG.md). Short rules for agents:

- CLI and Desktop share one workspace version; changelog headings use Option style (`## v0.2.0m-stable · DD/MM/YYYY`, with `m` when both surfaces change, heavier surface section first).
- `Cargo.toml` keeps the numeric version (`0.2.0`); release tags and changelog headings carry the Option channel (`v0.2.0m-stable`), while AUR `pkgver` stays numeric.
- Do **not** label something `stable` unless it is actually release-ready.
- Prefer **beta** for pre-release cuts; use **alpha** only for brand-new / half-built surfaces.
- Alpha/beta cuts are normally changelog + local artifacts — not GitHub Release / AUR — unless explicitly promoted to **stable**.

When documenting uncommitted work, prepend or expand the matching `vX.Y.Z[-m]-<channel>` entry rather than inventing a parallel scheme.

## Don’t

- Commit or push unless the user asks
- Force-push / skip hooks / amend pushed commits
- Bypass `prepare_state()` / `search_app().ensure()` or write outside `~/.option/search` except the documented config fallbacks
- Regress the local-first contract (no network, daemon, telemetry, or remote API) without intent

## GTK verification

- After GTK changes, install the desktop binaries too: `cargo install --path crates/optionsearch-gui --force --offline`.
- GTK must follow the system color scheme and configured icon theme by default. Do not force named icon themes, hardcoded palettes, fonts, or window-control styling. Keep app CSS limited to layout and theme-provided semantic colors.
