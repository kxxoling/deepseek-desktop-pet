#!/usr/bin/env bash
# 下载思源黑体（TrueType 版，OFL）并生成子集 assets/fonts/cjk-subset.ttf，产物在 .gitignore 里。
# 本地开发跑一次即可，CI 由 composite action 缓存复用。
# 需要 python3 和网络，fonttools 缺失时自动安装。
set -euo pipefail
# Windows 上 Python 往管道写中文默认走本地代码页，会触发 UnicodeEncodeError；强制 UTF-8
export PYTHONUTF8=1
cd "$(git rev-parse --show-toplevel)"

OUT="assets/fonts/cjk-subset.ttf"
if [ -f "$OUT" ]; then
  echo "字体子集已存在，跳过：$OUT"
  exit 0
fi

PY="$(command -v python3 || command -v python)"
if ! "$PY" -c 'import fontTools' >/dev/null 2>&1; then
  # 部分发行版的 pip 受 PEP 668 管控，需要 --break-system-packages
  "$PY" -m pip install --quiet --user fonttools \
    || "$PY" -m pip install --quiet --user --break-system-packages fonttools
fi

mkdir -p assets/fonts
"$PY" - "$OUT" <<'PYEOF'
import sys
import urllib.request
import zipfile

# 固定 Release tag，保证产物可复现；升级字体时改这里
TAG = "2.005-2.003-1.002-R"
URL = f"https://github.com/Pal3love/Source-Han-TrueType/releases/download/{TAG}/SourceHanSansCN.zip"

out_path = sys.argv[1]

# 字符集：GB2312 一级汉字（3755 个常用字）+ ASCII 可打印 + 常用中文标点
chars = set()
for hi in range(0xB0, 0xD8):
    for lo in range(0xA1, 0xFF):
        try:
            chars.add(bytes([hi, lo]).decode("gb2312"))
        except UnicodeDecodeError:
            pass
chars.update(chr(c) for c in range(0x20, 0x7F))
chars.update("，。！？、；：“”‘’（）《》〈〉【】…—·￥％＋－×÷＝～")

req = urllib.request.Request(URL, headers={"User-Agent": "ds-pet-font-fetch"})
with urllib.request.urlopen(req) as resp:
    zip_bytes = resp.read()
print(f"下载完成：{len(zip_bytes) / 1e6:.1f} MB")

with zipfile.ZipFile(__import__("io").BytesIO(zip_bytes)) as zf:
    regular = zf.read("SourceHanSansCN-Regular.ttf")

from fontTools import subset

opts = subset.Options()
opts.no_hinting = True
opts.name_IDs = [1, 2, 3, 4, 6]
opts.drop_tables += ["FFTM"]
opts.recalc_bounds = True
font = subset.load_font(__import__("io").BytesIO(regular), opts)
subsetter = subset.Subsetter(opts)
subsetter.populate(text="".join(sorted(chars)))
subsetter.subset(font)
subset.save_font(font, out_path, opts)

import os

size = os.path.getsize(out_path)
if size < 500_000:
    sys.exit(f"子集异常地小（{size} 字节），疑似出错")
print(f"子集完成：{out_path}（{size / 1e6:.2f} MB，{len(chars)} 字符）")
PYEOF
