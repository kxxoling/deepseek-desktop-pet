#!/usr/bin/env bash
# 资产预处理：源 webm → clip.zst（zstd 压缩的连续 RGBA 帧流，320×180@15fps）
# 用法：scripts/preprocess_assets.sh [源角色的videos目录] [输出目录]
#   不带参数时处理 submodule 里的内置 shenshen，输出到仓库 assets/（默认行为不变）
#   接入新角色：scripts/preprocess_assets.sh <角色的videos目录> <输出目录>，产出 <输出目录>/frames 与 <输出目录>/meta，放进系统的外部角色目录即可被运行时发现
#   并行度用环境变量 JOBS 控制（默认 8）；已有的 clip.zst 自动跳过，可以重复执行
# 依赖：ffmpeg、zstd
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${1:-$ROOT/third_party/dsh-pet-indesktop/assets/characters/shenshen/videos}"
CHAR_ROOT="${2:-$ROOT/assets}"
OUT="$CHAR_ROOT/frames"
META="$CHAR_ROOT/meta"
JOBS="${JOBS:-8}"

if [ ! -d "$SRC" ]; then
  echo "源角色目录不存在：$SRC" >&2
  if [ "$SRC" = "$ROOT/third_party/dsh-pet-indesktop/assets/characters/shenshen/videos" ]; then
    echo "内置角色在 submodule 里，先执行：git submodule update --init" >&2
  fi
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
# 产物总量校验：每个 clip 正常应有数百 KB 到数 MB，总量明显偏小说明转码源异常（防止静默空包）
# 阈值按 clip 数缩放，这样只转码小子集的测试也能用同一条规则
if [ "$clips" -eq 0 ] || [ "$size" -lt $((clips * 512)) ]; then
  echo "资产产物 ${size}KB / ${clips} 个 clip，明显偏小，转码源可能异常" >&2
  exit 1
fi
echo "完成：${clips} 个 clip.zst，共 $(du -sh "$OUT" | cut -f1)，输出于 ${OUT}"
