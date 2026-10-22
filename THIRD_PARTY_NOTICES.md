# Third Party Notices

## dsh-pet-indesktop

本项目的动画资产（webm 源素材，及由其衍生的帧资产）来自
[dsh-pet-indesktop](https://github.com/MerZlin/dsh-pet-indesktop)。

MIT License

Copyright (c) MerZlin

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## 思源黑体（Source Han Sans）

分发包中的 `assets/fonts/cjk-subset.ttf` 由 `scripts/fetch_font.sh` 在构建期生成，内容是 [Source Han Sans](https://github.com/adobe-fonts/source-han-sans)（思源黑体，Adobe）Regular 字重的子集，取 GB2312 一级汉字、ASCII 与常用标点；TrueType 版本来自 [Source-Han-TrueType](https://github.com/Pal3love/Source-Han-TrueType) 的转换。许可证全文见 `assets/fonts/OFL.txt`。

SIL Open Font License 1.1

## Rust 依赖

传递依赖的许可以 `Cargo.lock` 中各 crate 自带的许可证声明为准；
许可证白名单由 CI 的 cargo-deny 审计（`deny.toml`）。
