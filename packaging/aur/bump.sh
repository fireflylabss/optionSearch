#!/usr/bin/env bash
# Bump packaging/aur for a tagged release.
# Usage: ./packaging/aur/bump.sh v0.2.0m-stable
#        ./packaging/aur/bump.sh v0.2.0
#        ./packaging/aur/bump.sh 0.2.0
# Optional env: OPTIONSDK_VER=0.1.3 (defaults to _optionsdk_ver in PKGBUILD)
#
# Channeled tags (Option mixed/stable, e.g. v0.2.0m-stable) map to a numeric
# pkgver (0.2.0); the exact tag is kept in _tag and used for the source URL.
# .SRCINFO is regenerated with `makepkg --printsrcinfo` when available,
# otherwise a synced hand-written fallback is used.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PKGBUILD="$ROOT/packaging/aur/PKGBUILD"
SRCINFO="$ROOT/packaging/aur/.SRCINFO"

RAW="${1:?usage: bump.sh <version|vVersion> (e.g. v0.2.0m-stable, v0.2.0, 0.2.0)}"
# Exact tag, always with leading v.
TAG="$RAW"
[[ "$TAG" == v* ]] || TAG="v$TAG"
# Numeric pkgver: strip leading v, the mixed `m` marker + channel suffix
# (v0.2.0m-stable -> 0.2.0), and any bare -alpha/-beta/-stable suffix.
VER="${TAG#v}"
VER="${VER%%m*}"
VER="$(printf '%s' "$VER" | sed -E 's/-(alpha|beta|stable)$//')"
if [[ -z "$VER" ]]; then
  echo "Could not derive numeric pkgver from tag: $TAG" >&2
  exit 1
fi

OSEARCH_URL="https://github.com/fireflylabss/optionSearch/archive/refs/tags/${TAG}.tar.gz"

SDK_VER="${OPTIONSDK_VER:-}"
if [[ -z "$SDK_VER" ]]; then
  SDK_VER="$(sed -n 's/^_optionsdk_ver=//p' "$PKGBUILD" | head -1)"
fi
if [[ -z "$SDK_VER" ]]; then
  echo "Could not resolve optionSDK version" >&2
  exit 1
fi
SDK_URL="https://github.com/fireflylabss/optionSDK/archive/refs/tags/v${SDK_VER}.tar.gz"

wait_url() {
  local url="$1"
  echo "==> waiting for $url"
  for _ in $(seq 1 12); do
    if curl -fsI "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep 5
  done
  echo "tarball not reachable: $url" >&2
  exit 1
}

wait_url "$OSEARCH_URL"
wait_url "$SDK_URL"

echo "==> hashing tarballs"
OSEARCH_SHA="$(curl -fsSL "$OSEARCH_URL" | sha256sum | awk '{print $1}')"
SDK_SHA="$(curl -fsSL "$SDK_URL" | sha256sum | awk '{print $1}')"
echo "    optionsearch sha256=$OSEARCH_SHA"
echo "    optionSDK    sha256=$SDK_SHA"

echo "==> updating PKGBUILD → $VER (tag $TAG, optionSDK $SDK_VER)"
sed -i "s/^pkgver=.*/pkgver=${VER}/" "$PKGBUILD"
sed -i "s/^pkgrel=.*/pkgrel=1/" "$PKGBUILD"
if grep -q '^_tag=' "$PKGBUILD"; then
  sed -i "s/^_tag=.*/_tag=${TAG}/" "$PKGBUILD"
else
  sed -i "s/^pkgrel=.*/pkgrel=1\n_tag=${TAG}/" "$PKGBUILD"
fi
sed -i "s/^_optionsdk_ver=.*/_optionsdk_ver=${SDK_VER}/" "$PKGBUILD"
perl -i -0pe "s/sha256sums=\(\s*(?:'[^']*'|\n|\s)*\)/sha256sums=(\n  '${OSEARCH_SHA}'\n  '${SDK_SHA}'\n)/s" "$PKGBUILD"

echo "==> writing .SRCINFO"
if command -v makepkg >/dev/null 2>&1; then
  (cd "$(dirname "$PKGBUILD")" && makepkg --printsrcinfo) > "$SRCINFO"
else
  echo "    (makepkg not found — using hand-written fallback)"
  cat > "$SRCINFO" <<EOF
pkgbase = optionsearch
	pkgdesc = Instant local-first file search for Linux (Option family)
	pkgver = ${VER}
	pkgrel = 1
	url = https://github.com/fireflylabss/optionSearch
	arch = x86_64
	license = Apache-2.0
	makedepends = cargo
	depends = gcc-libs
	depends = glibc
	depends = gtk4
	depends = libadwaita
	optdepends = poppler: PDF text previews via pdftotext
	provides = needle
	provides = nld
	conflicts = needle
	replaces = needle
	options = !lto
	source = optionsearch-${VER}.tar.gz::https://github.com/fireflylabss/optionSearch/archive/refs/tags/${TAG}.tar.gz
	source = optionSDK-${SDK_VER}.tar.gz::https://github.com/fireflylabss/optionSDK/archive/refs/tags/v${SDK_VER}.tar.gz
	sha256sums = ${OSEARCH_SHA}
	sha256sums = ${SDK_SHA}

pkgname = optionsearch
EOF
fi

echo "==> done (packaging/aur ready for AUR push)"
