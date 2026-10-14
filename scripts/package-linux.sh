#!/bin/bash
# 打包 Linux 完整分发包：二进制 + assets → tar.gz
# 用法：scripts/package-linux.sh <target-triple> <version>
set -euo pipefail

TARGET="${1:?用法：$0 <target-triple> <version>}"
VERSION="${2:?用法：$0 <target-triple> <version>}"
ARCH="${TARGET%%-*}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/ds-pet-${VERSION}-linux-${ARCH}.tar.gz"

PKG="$ROOT/package"
rm -rf "$PKG" "$OUT"
mkdir -p "$PKG"
cp "$ROOT/target/${TARGET}/release/ds-pet" "$PKG/"
cp -R "$ROOT/assets" "$PKG/assets"
tar -czf "$OUT" -C "$PKG" ds-pet assets
rm -rf "$PKG"
echo "产物：$OUT"
