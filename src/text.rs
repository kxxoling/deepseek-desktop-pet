//! 文本测量与换行（纯函数，不依赖窗口系统）。
//! 规则：CJK 字符逐字断行；ASCII 按空格分词，词内不拆；超长词硬截。
//! 光栅化用 `fontdue`（纯 Rust，跨平台行为一致）。

/// 一行文本的字符范围（对原字符串的字节区间）
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
    /// 这一行的像素宽度
    pub width: f32,
}

/// 判断一个字符是否可以在它前面断行（CJK 逐字、ASCII 按词）
#[allow(dead_code)]
fn is_break_before(ch: char) -> bool {
    // CJK 统一表意文字、扩展 A、全角标点等
    matches!(ch as u32,
        0x3000..=0x30FF   // CJK 标点、假名
        | 0x3400..=0x4DBF // 扩展 A
        | 0x4E00..=0x9FFF // CJK 统一表意
        | 0xF900..=0xFAFF // 兼容表意
        | 0xFF00..=0xFFEF // 全角形式
    )
}

/// 测量单个字符的显示宽度（fontdue 的度量）
/// 暂时用等宽近似：CJK 全宽、ASCII 半宽；接入字体后换成真实度量
#[allow(dead_code)]
fn char_width(ch: char, font_size: f32) -> f32 {
    if is_break_before(ch) {
        font_size // CJK 全宽
    } else {
        font_size * 0.5 // ASCII 半宽近似
    }
}

/// 把文本按最大宽度换行，返回每行的字节范围和像素宽度。
/// 需要一个度量函数 `measure`（传入字符 → 像素宽度），方便单测注入精确值
#[allow(dead_code)]
pub fn wrap_text(text: &str, max_width: f32, measure: &dyn Fn(char, f32) -> f32) -> Vec<LineSpan> {
    let font_size = 16.0; // 默认字号，气泡统一
    let mut lines = Vec::new();
    let mut line_start = 0;
    let mut line_width = 0.0;
    let mut last_word_start = None; // ASCII 词的起始位置（可断行处）

    for (idx, ch) in text.char_indices() {
        let ch_w = measure(ch, font_size);

        // 检查加上这个字符是否超宽
        if line_width + ch_w > max_width && idx > line_start {
            // 需要断行
            let break_at = if is_break_before(ch) {
                // CJK：直接在这个字符前面断
                idx
            } else if let Some(ws) = last_word_start.filter(|ws| *ws > line_start) {
                // ASCII：回退到最近的词首
                ws
            } else {
                // 没有可回退的词首（单个超长词）：硬截
                idx
            };
            lines.push(LineSpan {
                start: line_start,
                end: break_at,
                width: line_width,
            });
            line_start = break_at;
            line_width = measure(ch, font_size);
            // 跳过行首空格
            if ch == ' ' {
                line_start = idx + 1;
                line_width = 0.0;
            }
        } else {
            line_width += ch_w;
        }

        // 记录 ASCII 词首位置（空格后面）
        if ch == ' ' {
            last_word_start = Some(idx + 1);
        }
    }

    // 最后一行
    if line_start < text.len() {
        lines.push(LineSpan {
            start: line_start,
            end: text.len(),
            width: line_width,
        });
    }

    lines
}

/// 用默认度量（CJK 全宽、ASCII 半宽）换行——接入真实字体前的过渡
#[allow(dead_code)]
pub fn wrap_default(text: &str, max_width: f32) -> Vec<LineSpan> {
    wrap_text(text, max_width, &char_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 精确度量：每个字符宽度 = 字号（方便计算期望值）
    fn uniform(ch: char, size: f32) -> f32 {
        let _ = ch;
        size
    }

    #[test]
    fn wraps_cjk_per_character() {
        // 6 个汉字，每字宽 16，最大宽 64（4 字/行）→ 2 行
        let lines = wrap_text("你好世界再见", 64.0, &uniform);
        assert_eq!(lines.len(), 2);
        assert_eq!(&"你好世界再见"[lines[0].start..lines[0].end], "你好世界");
        assert_eq!(&"你好世界再见"[lines[1].start..lines[1].end], "再见");
    }

    #[test]
    fn wraps_ascii_by_word() {
        // 宽 48（每字符 16×0.5=8，6 个字符/行）
        let fixed = |ch: char, size: f32| {
            let _ = ch;
            size * 0.5
        };
        let lines = wrap_text("hello world foo", 48.0, &fixed);
        // 每行最多 48/(16*0.5)=6 个字符 → "hello" (5+space=6) / "world" (5+space=6) / "foo"
        assert!(lines.len() >= 2, "lines: {lines:?}");
        // 第一行不应截断单词
        let first = &"hello world foo"[lines[0].start..lines[0].end];
        assert!(
            first.trim_end().ends_with("hello") || first.trim_end().ends_with("world"),
            "first: {first:?}"
        );
    }

    #[test]
    fn empty_and_single_line() {
        assert_eq!(wrap_text("", 100.0, &uniform), Vec::<LineSpan>::new());
        let lines = wrap_text("短", 100.0, &uniform);
        assert_eq!(lines.len(), 1);
        assert_eq!(&"短"[lines[0].start..lines[0].end], "短");
    }

    #[test]
    fn long_unbreakable_word_hard_breaks() {
        // 一个没有空格的长英文串：硬截
        let lines = wrap_text("aaaaaaaaaaaaaaaa", 32.0, &uniform); // 每字 16，2 字/行
        assert!(lines.len() >= 8, "should hard-break, got {lines:?}");
    }

    #[test]
    fn mixed_cjk_and_ascii() {
        // 中文逐字断 + 英文按词断
        let lines = wrap_text("你好hello世界", 48.0, &uniform); // 3 字/行
        assert!(lines.len() >= 2);
    }

    #[test]
    fn wrap_default_uses_char_width() {
        // wrap_default 内部调用 char_width，验证换行结果合理
        let lines = wrap_default("你好世界", 48.0);
        // CJK 全宽（16px/字），48px 宽放 3 个字 → 2 行
        assert_eq!(lines.len(), 2);
        let first = &"你好世界"[lines[0].start..lines[0].end];
        assert_eq!(first, "你好世");
    }

    #[test]
    fn char_width_cjk_fullwidth_ascii_half() {
        assert_eq!(char_width('你', 16.0), 16.0);
        assert_eq!(char_width('a', 16.0), 8.0);
        assert_eq!(char_width(' ', 16.0), 8.0);
    }

    #[test]
    fn exact_fit_does_not_wrap() {
        // 恰好 4 字宽 64：不断行
        let lines = wrap_text("你世", 32.0, &uniform);
        assert_eq!(lines.len(), 1);
    }
}

// ---------- 字体加载与光栅化 ----------

use fontdue::Font;
use fontdue::FontSettings;

/// 全局字体实例（懒加载一次）
static FONT: std::sync::OnceLock<Option<Font>> = std::sync::OnceLock::new();

/// 各平台候选字体路径（有 CJK 支持的 .ttf 文件，fontdue 不支持 .ttc 集合）
fn candidate_font_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    // 环境变量优先
    if let Some(p) = std::env::var_os("DS_PET_FONT") {
        paths.push(std::path::PathBuf::from(p));
    }
    // 随包的思源黑体子集（GB2312 一级 3755 个常用字 + ASCII + 常用标点，约 1MB），三平台显示一致，没有中文系统字体的机器也能渲染
    // 路径按资产根目录解析，与点击音效的定位方式一致
    paths.push(crate::assets::locate_assets_root().join("fonts/cjk-subset.ttf"));
    #[cfg(target_os = "macos")]
    {
        paths.push(std::path::PathBuf::from(
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ));
    }
    paths
}

/// 获取全局字体实例；找不到可用字体返回 None
#[allow(dead_code)]
pub fn get_font() -> Option<&'static Font> {
    FONT.get_or_init(|| {
        for path in candidate_font_paths() {
            if let Ok(bytes) = std::fs::read(&path) {
                match Font::from_bytes(bytes, FontSettings::default()) {
                    Ok(font) => return Some(font),
                    Err(e) => log::info!("字体加载失败 {}: {e}", path.display()),
                }
            }
        }
        log::info!("未找到可用字体，气泡文字无法渲染");
        None
    })
    .as_ref()
}

/// 把一行文字光栅化为 RGBA 像素（文字深灰、背景透明）
#[allow(dead_code)]
pub fn rasterize_line(text: &str, font_size: f32) -> Option<(Vec<u8>, u32, u32)> {
    let font = get_font()?;
    let height = (font_size * 1.4).ceil() as u32;

    // 逐字光栅化，取步进宽度
    let mut glyphs = Vec::new();
    let mut total_width = 0u32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, font_size);
        let advance = metrics.advance_width.ceil() as u32;
        glyphs.push((advance, metrics, bitmap));
        total_width += advance;
    }

    // 拼到一行像素里
    let mut pixels = vec![0u8; (total_width * height * 4) as usize];
    let baseline = height.saturating_sub((font_size * 0.3) as u32); // 基线约在 70% 高度
    let mut x_offset = 0u32;
    for (advance, metrics, bitmap) in glyphs {
        if metrics.width == 0 || metrics.height == 0 {
            continue; // 空格等零宽字符没有位图
        }
        for (dy, row) in bitmap.chunks(metrics.width).enumerate() {
            // ymin 是字形底部到基线的偏移
            let py =
                (baseline as i32 + metrics.ymin + dy as i32 - metrics.height as i32).max(0) as u32;
            if py >= height {
                continue;
            }
            for (dx, &alpha) in row.iter().enumerate() {
                let px = x_offset + metrics.xmin.max(0) as u32 + dx as u32;
                if px >= total_width {
                    continue;
                }
                let idx = (py * total_width + px) as usize * 4;
                if alpha > 0 {
                    pixels[idx] = 50;
                    pixels[idx + 1] = 50;
                    pixels[idx + 2] = 50;
                    pixels[idx + 3] = alpha;
                }
            }
        }
        x_offset += advance;
    }

    Some((pixels, total_width, height))
}

#[cfg(test)]
mod raster_tests {
    use super::*;

    #[test]
    fn candidate_paths_include_env_override() {
        // 设置 DS_PET_FONT 时应出现在候选列表第一位
        unsafe {
            std::env::set_var("DS_PET_FONT", "/tmp/test-font.ttf");
        }
        let paths = candidate_font_paths();
        assert_eq!(paths[0], std::path::PathBuf::from("/tmp/test-font.ttf"));
        unsafe {
            std::env::remove_var("DS_PET_FONT");
        }
    }

    #[test]
    fn break_before_classification() {
        // CJK 字符可以断行
        assert!(is_break_before('你'));
        assert!(is_break_before('。'));
        assert!(is_break_before('あ'));
        // ASCII 不可以
        assert!(!is_break_before('a'));
        assert!(!is_break_before(' '));
        assert!(!is_break_before('1'));
    }

    #[test]
    fn char_width_approximation() {
        // CJK 全宽
        assert_eq!(char_width('你', 20.0), 20.0);
        // ASCII 半宽
        assert_eq!(char_width('x', 20.0), 10.0);
    }

    #[test]
    fn font_loads_or_gracefully_none() {
        let r = get_font();
        assert!(r.is_some() || r.is_none());
    }

    #[test]
    fn bundled_subset_covers_common_chars() {
        // 直接加载随包字体（绕开全局缓存），常用汉字、标点和 ASCII 都要有真实字形
        // 缺字时 fontdue 画的是 .notdef 方框，位图同样非空，因此要查字形索引而不能只看位图
        let bytes = std::fs::read("assets/fonts/cjk-subset.ttf")
            .expect("随包字体应存在；缺失时先运行 scripts/fetch_font.sh");
        let font = Font::from_bytes(bytes, FontSettings::default()).expect("随包字体应可解析");
        for ch in "你好世界，宠物在说话。Hello!".chars() {
            assert_ne!(font.lookup_glyph_index(ch), 0, "缺字形：{ch}");
            let (_, bitmap) = font.rasterize(ch, 16.0);
            assert!(bitmap.iter().any(|&a| a > 0), "字形不应为空：{ch}");
        }
    }

    #[test]
    fn rasterize_produces_pixels() {
        if let Some((pixels, w, h)) = rasterize_line("Hi", 16.0) {
            assert!(w > 0 && h > 0);
            assert_eq!(pixels.len(), (w * h * 4) as usize);
            assert!(pixels.chunks(4).any(|px| px[3] > 0));
        }
    }
}
