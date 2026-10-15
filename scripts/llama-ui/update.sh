#!/usr/bin/env bash
# Refresh the bundled llama.cpp web UI under data/llama-ui/.
#
#   scripts/llama-ui/update.sh <bNNNN|latest>
#
# Downloads the pinned prebuilt UI archive from the ggml-org HuggingFace
# bucket, refuses it unless the published sha256 matches, records the
# concrete build number (read from build.json inside the archive, so
# `latest` never stores a floating version) and the sha in VERSION, then
# writes both files. Re-running with a build whose sha already matches
# VERSION changes nothing.

set -euo pipefail

BUCKET="https://huggingface.co/buckets/ggml-org/llama-ui/resolve"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEST_DIR="$ROOT/data/llama-ui"
ARCHIVE="dist.tar.gz"
VERSION_FILE="$DEST_DIR/VERSION"

usage() {
  echo "usage: $(basename "${BASH_SOURCE[0]}") <bNNNN|latest>" >&2
  exit 2
}

want="${1:-}"
[[ -z "$want" ]] && usage
[[ "$want" != "latest" && ! "$want" =~ ^b[0-9]+$ ]] && usage

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $want ..."
curl -fsSL "$BUCKET/$want/$ARCHIVE.sha256" -o "$tmp/$ARCHIVE.sha256"
expected="$(awk '{print $1}' "$tmp/$ARCHIVE.sha256")"
if [[ ! "$expected" =~ ^[0-9a-fA-F]{64}$ ]]; then
  echo "refusing: $ARCHIVE.sha256 did not start with 64 hex chars (got: ${expected:0:32})" >&2
  exit 1
fi

curl -fsSL "$BUCKET/$want/$ARCHIVE" -o "$tmp/$ARCHIVE"
actual="$(sha256_of "$tmp/$ARCHIVE")"
if [[ "$actual" != "$expected" ]]; then
  echo "refusing: sha256 mismatch (expected $expected, got $actual)" >&2
  exit 1
fi

# The archive member paths are all prefixed with ./.
build="$(tar xzf "$tmp/$ARCHIVE" ./build.json -O | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
if [[ ! "$build" =~ ^b[0-9]+$ ]]; then
  echo "refusing: build.json inside the archive has no usable version (got: $build)" >&2
  exit 1
fi

if [[ -f "$VERSION_FILE" ]] && [[ "$(awk -F= '/^sha256=/{print $2}' "$VERSION_FILE")" == "$actual" ]]; then
  echo "$build ($actual) already recorded in $VERSION_FILE; nothing to do"
  exit 0
fi

mkdir -p "$DEST_DIR"
cp "$tmp/$ARCHIVE" "$DEST_DIR/$ARCHIVE"
printf 'build=%s\nsha256=%s\n' "$build" "$actual" > "$VERSION_FILE"
echo "wrote $DEST_DIR/$ARCHIVE and $VERSION_FILE ($build, $actual)"
