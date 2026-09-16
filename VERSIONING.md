# Versioning

We follow [Semantic Versioning](https://semver.org/) and [Keep a Changelog](https://keepachangelog.com/). CLI and Desktop are versioned together (one shared workspace version).

The public changelog is the source of truth for headings, mixed releases, and channel language — see the expandable blurb at the top of [CHANGELOG.md](CHANGELOG.md).

## Surfaces

| Surface | What you install | Artifact version today |
| --- | --- | --- |
| **CLI** | `optionsearch` / alias `nld` | `0.2.0` (`Cargo.toml` workspace) |
| **Desktop** | `optionsearch-gtk` / alias `needle` | `0.2.0` (`Cargo.toml` workspace) |

Both surfaces share the workspace version in the root `Cargo.toml`. The numeric version there never carries a channel suffix (`0.2.0`, not `0.2.0-beta`).

Changelog headings and release tags always include the leading `v`. Mixed cuts that change both surfaces insert an `m` before the channel (e.g. `v0.2.0m-stable`); AUR `pkgver` stays numeric (`0.2.0` — hyphens are not allowed there).

## Channels

| Suffix | Meaning |
| --- | --- |
| **-alpha** | Very early. Missing pieces and lots of bugs. |
| **-beta** | Mostly there, but still rough. Not the official install. |
| **-stable** | Ready for daily use — GitHub Releases / AUR. |

Do **not** label something `stable` unless it is release-ready. Prefer **beta** for pre-release cuts; only ship **stable** when the cut is genuinely release-ready.

Precedent: `v0.1.0` predates channel suffixes and stays a plain `## v0.1.0 · DD/MM/YYYY` heading. From `v0.2.0m-stable` on, every mixed version includes a channel.

Beta cuts are normally changelog + local artifacts — not GitHub Release / AUR — unless explicitly promoted to **stable**.
