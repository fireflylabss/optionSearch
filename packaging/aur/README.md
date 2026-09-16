# AUR

`PKGBUILD` targets release tarballs from the optionSearch repository. `bump.sh`
fills in the `SKIP` checksums and regenerates `.SRCINFO` (via
`makepkg --printsrcinfo` when available, hand-written fallback otherwise);
`publish.sh` is the local fallback for pushing to the AUR.

`needle` → `optionsearch` rename: the old `needle` package must be replaced,
not co-installed, since both own `/usr/bin/needle`
(`provides`/`conflicts`/`replaces: needle`). `nld` ships as a CLI compat
symlink. The desktop entry installs to
`/usr/share/applications/io.option.search.desktop`
(from `packaging/optionsearch.desktop`, `Exec=optionsearch-gtk`).

Versions: release tags carry the Option channel (`v0.2.0m-stable`), while
`pkgver` stays numeric (`0.2.0` — the AUR forbids hyphens). The exact tag is
kept in `_tag` and used for the source URL:
`.../archive/refs/tags/v0.2.0m-stable.tar.gz`. `bump.sh` accepts a channeled tag
(`v0.2.0m-stable`), a plain tag (`v0.2.0`), or a bare version (`0.2.0`).

Release order: the tag + GitHub release must exist **before** the AUR publish
— `bump.sh` hashes the release tarball, so an unpublished tag fails the
reachability check.

Beta policy: alpha/beta cuts are normally changelog + local artifacts — they
are only pushed to the AUR when promoted to a **stable** release.

Publishing prerequisites:

- This repository pushed to `github.com/fireflylabss/optionSearch` (local repo +
  `origin` are configured; no commit/tag has been pushed yet).
- The `optionsearch` package registered on the AUR (the first push creates it).
- `AUR_SSH_PRIVATE_KEY` set as a repository secret for the Publish AUR
  workflow.
- A tag + GitHub release (e.g. `v0.2.0m-stable`) — `bump.sh` hashes the release
  tarball, so the tag must exist before publishing.
