#!/usr/bin/env bash
# Package the built update-binary into a minimal flashable ZIP:
#
#   update-binary-<sha8>.zip
#   └── META-INF/com/google/android/
#       ├── update-binary
#       └── updater-script
#
# Usage: ./package.sh [binary] [out_dir] [name_suffix]
set -euo pipefail

BIN="${1:-target/aarch64-linux-android/release/update-binary}"
OUT_DIR="${2:-dist}"
SUFFIX="${3:-}"

[ -f "$BIN" ] || { echo "no such binary: $BIN" >&2; exit 1; }
need=(zip sha256sum)
for t in "${need[@]}"; do command -v "$t" >/dev/null || { echo "missing: $t" >&2; exit 1; }; done

sha=$(sha256sum "$BIN" | cut -d' ' -f1)
# Name the package after the commit (matches CI release naming); fall back to
# the binary hash when there is no git checkout.
default_name=$(git rev-parse --short=8 HEAD 2>/dev/null || true)
name="${SUFFIX:-${default_name:-${sha:0:8}}}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
base="$work/META-INF/com/google/android"
mkdir -p "$base"
cp "$BIN" "$base/update-binary"
chmod 755 "$base/update-binary"

printf 'zhlhlf_flow\n' > "$base/updater-script"

mkdir -p "$OUT_DIR"
zip_path="$OUT_DIR/update-binary-${name}.zip"
rm -f "$zip_path"
( cd "$work" && zip -q -X -9 -r "$OLDPWD/$zip_path" META-INF )

echo "binary : $BIN"
echo "  sha256 $sha"
echo "zip    : $zip_path"
echo "  sha256 $(sha256sum "$zip_path" | cut -d' ' -f1)"
echo "  size   $(stat -c%s "$zip_path") bytes"
unzip -l "$zip_path"
