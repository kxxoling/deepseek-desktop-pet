#!/bin/bash
# zig 交叉编译 wrapper（macOS 上静态检查 Windows 目标用；CI 为 msvc，本地用 gnu 等价）：
#   CC_x86_64_pc_windows_gnu=$PWD/scripts/zigcc-win-cross.sh \
#   AR_x86_64_pc_windows_gnu="zig ar" \
#   cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
# 1) 把 Rust triple 映射为 zig triple（= 与分离形式都处理）
# 2) rustc 调 linker 时不带 --target（它假定 linker 原生支持目标平台），
#    zig 必须显式指定，否则回落到宿主目标
args=()
have_target=0
for a in "$@"; do
  case "$a" in
    --target=x86_64-pc-windows-gnu)
      a="--target=x86_64-windows-gnu"
      have_target=1
      ;;
    -target | --target)
      have_target=1
      ;;
    x86_64-pc-windows-gnu)
      a="x86_64-windows-gnu"
      have_target=1
      ;;
  esac
  args+=("$a")
done
if [ "$have_target" = 0 ]; then
  set -- -target x86_64-windows-gnu "${args[@]}"
else
  set -- "${args[@]}"
fi
exec zig cc "$@"
