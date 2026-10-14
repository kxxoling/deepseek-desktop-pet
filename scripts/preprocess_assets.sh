#!/usr/bin/env bash
# 资产预处理：源 webm → clip.zst（zstd 压缩的连续 RGBA 帧流，320×180@15fps）
# 用法：scripts/preprocess_assets.sh [JOBS]（默认并行 8；已有 clip.zst 自动跳过，可重跑）
# 依赖：ffmpeg、zstd
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/third_party/dsh-pet-indesktop/assets/characters/shenshen/videos"
OUT="$ROOT/assets/frames"
META="$ROOT/assets/meta"
JOBS="${1:-8}"

if [ ! -d "$SRC" ]; then
  echo "缺少源资产 submodule：$SRC" >&2
  echo "先执行：git submodule update --init" >&2
  exit 1
fi
command -v ffmpeg >/dev/null || { echo "缺少 ffmpeg 命令（macOS: brew install ffmpeg / Debian: apt install ffmpeg）" >&2; exit 1; }
command -v zstd >/dev/null || { echo "缺少 zstd 命令（brew install zstd / apt install zstd）" >&2; exit 1; }

mkdir -p "$OUT" "$META"
# 元数据清单（manifest / 步幅曲线 / 文字不镜像名单）
cp "$SRC/manifest.json" "$SRC/move_strides.json" "$SRC/text_clips.json" "$META/"

convert_one() {
  local rel name dest
  rel="${1#"$SRC"/}"
  name="${rel%.webm}"
  dest="$OUT/$name"
  if [ -f "$dest/clip.zst" ]; then
    echo "跳过 $name"
    return 0
  fi
  mkdir -p "$dest"
  # 抠像后的原始 RGBA 帧流直接管道进 zstd
  if ! ffmpeg -y -v error -i "$1" \
      -vf "fps=15,scale=320:180:flags=lanczos,colorkey=0x000000:0.10:0.15" \
      -f rawvideo -pix_fmt rgba pipe:1 \
      | zstd -19 -q -T0 -o "$dest/clip.zst"; then
    echo "失败: $rel" >&2
    rm -rf "$dest"
    return 1
  fi
}
export -f convert_one
export SRC OUT

find "$SRC" -name '*.webm' -print0 | xargs -0 -n1 -P "$JOBS" bash -c 'convert_one "$0"'

# 应用图标：idle 首帧放大到 1024×1024（打包阶段仅用 sips/iconutil 处理，免 ffmpeg 依赖）
IDLE_ZST=$(find "$OUT/idle" -name 'clip.zst' | head -1 || true)
if [ -n "$IDLE_ZST" ] && [ ! -f "$META/app-icon.png" ]; then
  zstd -d -c "$IDLE_ZST" 2>/dev/null | head -c $((320 * 180 * 4)) > "$META/icon-first.rgba" || true
  ffmpeg -y -v error -f rawvideo -pix_fmt rgba -video_size 320x180 \
    -i "$META/icon-first.rgba" -vf "crop=180:180:70:0,scale=1024:1024:flags=lanczos,format=rgba" \
    -frames:v 1 "$META/app-icon.png" 2>/dev/null || true
  rm -f "$META/icon-first.rgba"
fi
[ -f "$META/app-icon.png" ] || echo "警告：应用图标未生成（打包将无图标）" >&2

clips=$(find "$OUT" -name 'clip.zst' | wc -l | tr -d ' ')
size=$(du -sk "$OUT" | cut -f1)
# 产物总量校验：正常应数百 MB，过小说明转码源异常（防止静默空包）
if [ "$size" -lt 10240 ]; then
  echo "资产产物仅 ${size}KB（正常应 >100MB），转码源可能异常" >&2
  exit 1
fi
echo "完成：${clips} 个 clip.zst，共 $(du -sh "$OUT" | cut -f1)，输出于 ${OUT}"
