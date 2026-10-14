#!/bin/bash
# 组装 macOS .app（含资产与图标）→ 签名（scripts/sign-macos.sh，无凭据时 ad-hoc）→ ditto zip
# 用法：scripts/package-macos.sh <target-triple> <version>
# 产物：ds-pet-<version>-macos-<arch>.zip（解压即 ds-pet.app，双击运行）
set -euo pipefail

TARGET="${1:?用法：$0 <target-triple> <version>}"
VERSION="${2:?用法：$0 <target-triple> <version>}"
ARCH="${TARGET%%-*}" # aarch64 / x86_64
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/ds-pet.app"
OUT="$ROOT/ds-pet-${VERSION}-macos-${ARCH}.zip"

rm -rf "$APP" "$OUT"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/${TARGET}/release/ds-pet" "$APP/Contents/MacOS/ds-pet"
cp -R "$ROOT/assets" "$APP/Contents/Resources/assets"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ds-pet</string>
  <key>CFBundleDisplayName</key><string>ds-pet</string>
  <key>CFBundleExecutable</key><string>ds-pet</string>
  <key>CFBundleIdentifier</key><string>com.ds-pet.desktop</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# 图标：预处理阶段生成的 1024 PNG → icns（sips/iconutil 为系统自带；失败不阻塞）
ICON_PNG="$ROOT/assets/meta/app-icon.png"
if [ -f "$ICON_PNG" ]; then
  TMP=$(mktemp -d)
  mkdir -p "$TMP/icon.iconset"
  for s in 16 32 128 256 512; do
    sips -z $s $s "$ICON_PNG" --out "$TMP/icon.iconset/icon_${s}x${s}.png" >/dev/null 2>&1 || true
    sips -z $((s * 2)) $((s * 2)) "$ICON_PNG" --out "$TMP/icon.iconset/icon_${s}x${s}@2x.png" >/dev/null 2>&1 || true
  done
  iconutil -c icns "$TMP/icon.iconset" -o "$APP/Contents/Resources/AppIcon.icns" 2>/dev/null || true
  rm -rf "$TMP"
else
  echo "警告：缺少 assets/meta/app-icon.png，包内将无图标" >&2
fi

"$ROOT/scripts/sign-macos.sh" "$APP"
# ditto 保留可执行位与符号链接（zip(1) 会丢）
ditto -c -k --keepParent "$APP" "$OUT"
echo "产物：$OUT"
