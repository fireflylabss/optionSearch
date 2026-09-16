# Changelog

We follow [Semantic Versioning](https://semver.org/) and [Keep a Changelog](https://keepachangelog.com/). CLI and Desktop are versioned together (one shared workspace version).

<details>
<summary>To see more about versioning, expand this.</summary>

Every version string starts with `v` (required), e.g. `v0.2.0m-stable`, `v0.1.0`.

Here the installable surfaces are **CLI** and **Desktop**.

| Part | What you install | Example |
| --- | --- | --- |
| **CLI** | `optionsearch` / alias `nld` in the terminal | `v0.2.0m-stable` |
| **Desktop** | `optionsearch-gtk` / alias `needle`, the visual app | `v0.2.0m-stable` |

A new CLI does not always mean a new Desktop app, and the other way around.

Sometimes one cut ships **both** surfaces. That is a **mixed release**: one tag with an `m` before the channel (ex: `v0.2.0m-stable`), and the notes break out each surface so a small touch on one side does not look equal to a large cut on the other.

Each release heading is the version and date (`## v0.2.0m-stable · 16/09/2026`); under it, a short summary ends with a plain sentence like: “This version was made for both desktop and CLI with a stable release channel on 16/09/2026 (v0.2.0m-stable).”

### What the suffix means

| Suffix | In plain words |
| --- | --- |
| **-alpha** | Very early. Expect missing pieces and lots of bugs. |
| **-beta** | Mostly there, but still rough. Fine to try; not the “official” install. |
| **-stable** | Ready for daily use. This is what we put on GitHub Releases and the AUR. |

We only call something **stable** when we mean it; pre-release cuts stay **beta**.

The original `v0.1.0` cut had no channel suffix. From `v0.2.0m-stable` on, every mixed version includes one.

</details>

## v0.2.0m-stable · 16/09/2026

Rename to optionSearch across both surfaces, with shared state migration and canonical search paths. This version was made for both desktop and CLI with a stable release channel on 16/09/2026 (v0.2.0m-stable).

### Desktop

- Bundle identifier is now `io.option.search` (was `io.option.needle`); the `needle` desktop binary is kept as a compat alias for `optionsearch-gtk`.
- Canonical state under `~/.option/search`; first GUI launch migrates `~/.option/needle` once.
- Config canonical at `~/.option/search/config.toml` with `~/.config/optionsearch` → `~/.config/needle` fallbacks; index DB default `index.db` → `index.sqlite3`; config writes are atomic.
- Various other small tweaks

### CLI

- Binary renamed `needle` → `optionsearch` (`nld` compat alias kept).
- Shared `prepare_state()` migrates `~/.option/needle` once and surfaces failures instead of continuing unverified.
- Same config/DB path changes as Desktop (`~/.option/search/config.toml`, `index.sqlite3`); config written atomically.

## v0.1.0 · 12/09/2026

Needle begins: SQLite local index with instant search. This version was made for CLI on 12/09/2026 (v0.1.0).

- SQLite-backed local filesystem index and instant path search.
- Recursive inotify watcher, safe previews and newline-delimited JSON output.
- No account, sync, telemetry, daemon or remote API.
