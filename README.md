# ⌕ optionSearch

**optionSearch** is an instant, local-first file search tool for Linux
(renamed from Needle; `nld` remains as the CLI compat alias and `needle` as
the desktop compat alias).

It persists its index in SQLite, then searches a compact in-memory index of
names and paths — so the database never sits in the keystroke path. It has no
service, account, sync layer, network API or cloud copy of your files.
`inotify` keeps an active index current as files change.

```text
⌕ optionsearch invoice
application/pdf    42.1 KB  1723400312  /home/firefly/Documents/invoice.pdf
text/markdown       3.4 KB  1723394011  /home/firefly/Notes/invoice.md
```

## Install

### Build from source

Requires Rust 1.85+ and the sibling `optionSDK` checkout when developing the
family locally.

```bash
cargo build --release
cargo install --path crates/optionsearch-cli
cargo install --path crates/optionsearch-gui
```

Binaries: `optionsearch` (CLI, primary), `nld` (CLI compat), `optionsearch-gtk`
(desktop, primary), `needle` (desktop compat).

## Usage

```bash
# Index your home directory once.
optionsearch index

# Keep it current in a user-service or terminal.
optionsearch watch

# Instant lookup (the default command).
optionsearch invoice
optionsearch search "project brief" --limit 50

# Program-friendly JSON lines.
optionsearch --json invoice

# Safe terminal previews.
optionsearch preview ~/Documents/brief.md
optionsearch preview ~/Pictures/photo.png
optionsearch preview ~/Documents/report.pdf

optionsearch stats
optionsearch clear        # clears only the index, never your files
```

`optionsearch watch ~/Projects ~/Documents` narrows the indexed roots. The watcher
uses Linux inotify through `notify`; it is intentionally a foreground process
so it can be supervised by the user or a systemd user unit.

Compat: `nld` accepts the same subcommands as `optionsearch`.

## Data and privacy

optionSearch's canonical state is `~/.option/search/` (or the equivalent
under `$OPTION_HOME`): config at `~/.option/search/config.toml`, index DB at
`~/.option/search/index.sqlite3`. Config writes are atomic. A legacy
`~/.option/needle/` tree is migrated once on the first CLI **or** GUI run;
legacy XDG configs (`~/.config/optionsearch/config.toml`, then
`~/.config/needle/config.toml`) are honored as fallbacks when the canonical
config is missing. The preview/index cache lives under
`~/.option/search/cache`; XDG cache paths are no longer written.

The index stores path metadata: path, filename, MIME type, byte size and
modification timestamp. File contents are not indexed.

Previews read the requested file only at preview time. Text and source files
are rendered in the terminal; images return safe metadata; PDF text extraction
uses optional `pdftotext` when installed.

## Architecture

```text
crates/
  optionsearch-core/  in-memory index, SQLite persistence, parallel matcher, watcher, previews
  optionsearch-cli/   optionsearch + nld compat
  optionsearch-gui/   optionsearch-gtk + needle compat (GTK/libadwaita window)
packaging/aur/  Arch package metadata
```

optionSearch is the canonical Option-family search app. It uses `optionSDK` for
the shared local storage convention and its `io.option.search` bundle identity
(legacy `~/.option/needle` migrated once via `migrate_dir`).

The `optionsearch` AUR package (`packaging/aur/`) ships both binaries plus the
`io.option.search` desktop entry.

## License

Apache-2.0. See [LICENSE](LICENSE).
