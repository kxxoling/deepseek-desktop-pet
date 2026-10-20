# ds-pet — 跨平台桌宠（纯 Rust）

透明置顶窗口里的桌面宠物，逐像素合成、无黑底矩形。动画资产来自
[dsh-pet-indesktop](https://github.com/MerZlin/dsh-pet-indesktop)（MIT）。

## 功能

- 透明无边框置顶窗口，逐像素合成（单只 CPU ~2%，多开每只约 +40MB）
- 动画状态机：待机 / 单次片段 / 拖拽（含松手惯性）/ 行走（步幅曲线驱动，步态不打滑）
- 每 20–40s 随机事件：随机动画或自主溜达，屏幕边缘折返
- 交互：单击回应、双击多开（上限 8 只，各窗独立动画相位）、右键菜单、透明区域点击穿透（macOS）
- 托盘：全部窗口关闭后保活，可随时召回
- 日志落盘（含 panic 与 backtrace），菜单「打开日志文件夹」一键定位，可作为 issue 附件

## 构建与运行

```bash
git clone --recurse-submodules <repo-url> && cd ds-pet
scripts/preprocess_assets.sh
cargo run --release
```

预处理依赖 `ffmpeg` 与 `zstd`，可重复执行（跳过已完成片段）。
外部角色接入：`scripts/preprocess_assets.sh <character-videos-dir> <output-root>` 产出
`<output-root>/frames` 与 `<output-root>/meta`，放入系统的外部角色目录即可被识别
（macOS `~/Library/Application Support/ds-pet/characters/<id>/`）。并行度用环境变量
`JOBS` 控制（默认 8）。

调试环境变量：`DS_PET_POS=x,y`（首窗位置）、`DS_ALPHA`（`post`/`pre`/`auto`）、
`DS_PET_ASSETS`（资产目录）、`DS_PET_LOG_DIR`（日志目录）、
`DS_PET_CONFIG`（配置文件路径）、`RUST_LOG`。

## 平台支持

| 平台 | 状态 |
|---|---|
| macOS 11+ | 主线，功能完整 |
| Windows | 可编译；点击穿透未实现 |
| Linux / X11 | 可编译；右键菜单与点击穿透未实现 |
| Linux / Wayland | 部分合成器不支持透明窗口 |

## 许可

本项目代码许可待定。资产来自上游 MIT 项目，依赖许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

