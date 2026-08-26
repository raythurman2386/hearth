#!/usr/bin/env bash
# Write updater metadata into a directory of release artifacts:
#   latest.txt     — bare version string
#   manifest.json  — { version, files: { name → { sha256 } } }
#
# Updater-facing artifacts included when present:
#   hearth-<ver>-linux-<arch>.tar.gz
#   hearth-<ver>-macos-<arch>.tar.gz
#   hearth-<ver>-macos-<arch>-app.tar.gz
#   hearth-<ver>-macos-<arch>.dmg
#
# Usage:
#   scripts/write-manifest.sh <artifacts-dir> <version>
#   VERSION=0.2.3 scripts/write-manifest.sh ./artifacts
set -euo pipefail

DIR="${1:?usage: write-manifest.sh <artifacts-dir> [version]}"
VERSION="${2:-${VERSION:-}}"
if [[ -z "$VERSION" ]]; then
  echo "write-manifest: version required (arg 2 or VERSION=)" >&2
  exit 1
fi
VERSION="${VERSION#v}"

cd "$DIR"
shopt -s nullglob

unique=()
for f in hearth-"$VERSION"-*; do
  [[ -f "$f" ]] || continue
  case "$f" in
    hearth-"$VERSION"-linux-*.tar.gz) ;;
    hearth-"$VERSION"-macos-*-app.tar.gz) ;;
    hearth-"$VERSION"-macos-*.tar.gz) ;;
    hearth-"$VERSION"-macos-*.dmg) ;;
    *) continue ;;
  esac
  unique+=("$f")
done

if [[ ${#unique[@]} -eq 0 ]]; then
  echo "write-manifest: no updater artifacts matching hearth-$VERSION-{linux,macos}-* in $DIR" >&2
  exit 1
fi

{
  echo '{'
  echo "  \"version\": \"$VERSION\","
  echo '  "files": {'
  first=1
  for f in "${unique[@]}"; do
    hash="$(sha256sum "$f" | awk '{print $1}')"
    [[ $first -eq 1 ]] || echo ','
    first=0
    printf '    "%s": { "sha256": "%s" }' "$f" "$hash"
  done
  echo
  echo '  }'
  echo '}'
} >manifest.json

printf '%s\n' "$VERSION" >latest.txt

echo "wrote $DIR/manifest.json + latest.txt (${#unique[@]} file(s))"
cat manifest.json
