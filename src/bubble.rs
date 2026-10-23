//! 气泡：在宠物上方显示一条文字消息，超时自动关闭。
//! 纯逻辑（队列、分页、定位、光栅化）在本文件；窗口创建与渲染接线在 `bubble_window.rs`。

/// 气泡显示时长（秒）
pub const BUBBLE_DURATION_SECS: f64 = 5.0;
/// 气泡最大宽度（逻辑点）
pub const BUBBLE_MAX_WIDTH: f64 = 280.0;
/// 气泡内边距（逻辑点）
pub const BUBBLE_PADDING: f64 = 12.0;
/// 每页最多显示的行数（超出分页）
pub const BUBBLE_LINES_PER_PAGE: usize = 5;
/// 配图较长边的基础显示尺寸（逻辑像素），config `bubble_image_scale` 以它为基准缩放
pub const BUBBLE_IMAGE_BASE: f64 = 160.0;
/// 配图缩放系数的取值范围
pub const BUBBLE_IMAGE_SCALE_MIN: f64 = 0.5;
pub const BUBBLE_IMAGE_SCALE_MAX: f64 = 3.0;

/// 消息优先级：高的先显示
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)]
pub enum Priority {
    Low,
    Normal,
    High,
}

/// 一条气泡消息的内容
#[derive(Debug, Clone)]
pub struct BubbleContent {
    pub media: BubbleMedia,
    /// 显示秒数，0 用默认值
    pub duration_secs: f64,
    pub priority: Priority,
}

/// 气泡显示的内容：一句话，或一张 png 图片的字节
#[derive(Debug, Clone)]
pub enum BubbleMedia {
    Text(String),
    Image(Vec<u8>),
}

/// 队列里的一条（带序号，同优先级按 FIFO）
#[derive(Debug, Clone)]
struct Queued<T> {
    item: T,
    priority: Priority,
    seq: u64,
}

/// 气泡队列：一次显示一条，优先级高的排前面，同优先级按 FIFO。
/// 队列项是泛型，调用方可以附带自己的上下文（如来源宠物的窗口 id）
pub struct BubbleQueue<T> {
    pending: Vec<Queued<T>>,
    next_seq: u64,
}

impl<T> BubbleQueue<T> {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            next_seq: 0,
        }
    }

    /// 入队一条；优先级显式传入，队列按它排序
    pub fn push(&mut self, priority: Priority, item: T) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push(Queued {
            item,
            priority,
            seq,
        });
        // 按优先级降序 + 序号升序排列：高优先级在前，同级 FIFO
        self.pending
            .sort_by(|a, b| b.priority.cmp(&a.priority).then(a.seq.cmp(&b.seq)));
    }

    /// 取出下一条要显示的；队列空返回 None
    pub fn pop(&mut self) -> Option<T> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0).item)
        }
    }

    /// 队列长度
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

impl<T> Default for BubbleQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// 把换行后的行数组按每页容量分页；末页只有孤行时从前一页匀一行过来
pub fn paginate(lines: &[crate::text::LineSpan]) -> Vec<Vec<crate::text::LineSpan>> {
    if lines.is_empty() {
        return Vec::new();
    }
    let mut pages: Vec<Vec<crate::text::LineSpan>> = lines
        .chunks(BUBBLE_LINES_PER_PAGE)
        .map(<[crate::text::LineSpan]>::to_vec)
        .collect();
    // 末页只有 1 行且前一页有富余：匀一行过去，避免末页孤零零一行
    if pages.len() >= 2 {
        let prev_idx = pages.len() - 2;
        let needs_rebalance = pages[prev_idx].len() > 2 && pages[prev_idx + 1].len() == 1;
        if needs_rebalance
            && let Some(l) = pages[prev_idx].pop()
            && let Some(last) = pages.last_mut()
        {
            last.insert(0, l);
        }
    }
    pages
}

/// 计算气泡窗口的位置：默认在宠物上方水平居中，上方空间不够翻到下方
pub fn bubble_position(
    pet_pos: (f64, f64),
    pet_size: (f64, f64),
    bubble_size: (f64, f64),
) -> (f64, f64) {
    let gap = 8.0;
    let x = pet_pos.0 + (pet_size.0 - bubble_size.0) / 2.0;
    let y = if pet_pos.1 - bubble_size.1 - gap >= 0.0 {
        pet_pos.1 - bubble_size.1 - gap
    } else {
        pet_pos.1 + pet_size.1 + gap
    };
    (x.max(0.0), y.max(0.0))
}

/// 计算气泡窗口的尺寸：根据行数和字号
pub fn bubble_size(line_count: usize, font_size: f64) -> (f64, f64) {
    let line_height = font_size * 1.3;
    (
        BUBBLE_MAX_WIDTH,
        line_count as f64 * line_height + BUBBLE_PADDING * 2.0,
    )
}

/// 生成气泡的完整像素缓冲：背景半透明白色，文字深灰色
pub fn render_bubble_pixels(
    text: &str,
    lines: &[crate::text::LineSpan],
    font_size: f64,
) -> Vec<u8> {
    let line_height = (font_size as f32 * 1.3).ceil() as u32;
    let width = BUBBLE_MAX_WIDTH as u32;
    let height = (lines.len() as f64 * font_size * 1.3 + BUBBLE_PADDING * 2.0) as u32;

    let mut pixels = vec![0u8; (width * height * 4) as usize];
    // 背景半透明白色
    for px in pixels.chunks_mut(4) {
        px[0] = 255;
        px[1] = 255;
        px[2] = 255;
        px[3] = 220;
    }
    // 把每一行的文字画上去
    for (i, line) in lines.iter().enumerate() {
        let line_text = &text[line.start..line.end];
        if let Some((glyphs, gw, gh)) = crate::text::rasterize_line(line_text, font_size as f32) {
            let y_base = BUBBLE_PADDING as usize + i * line_height as usize;
            let x_base = BUBBLE_PADDING as usize;
            for gy in 0..gh as usize {
                let py = y_base + gy;
                if py >= height as usize {
                    continue;
                }
                for gx in 0..gw as usize {
                    let px = x_base + gx;
                    if px >= width as usize {
                        continue;
                    }
                    let src = (gy * gw as usize + gx) * 4;
                    let alpha = glyphs[src + 3];
                    if alpha > 0 {
                        let dst = (py * width as usize + px) * 4;
                        pixels[dst] = glyphs[src];
                        pixels[dst + 1] = glyphs[src + 1];
                        pixels[dst + 2] = glyphs[src + 2];
                        pixels[dst + 3] = alpha;
                    }
                }
            }
        }
    }
    pixels
}

// ---------- 配图 ----------

/// 图片的显示尺寸：较长边缩到基准尺寸再乘配置系数（系数超出 0.5–3.0 时按边界值），宽高比保持不变
pub fn fit_image_size(w: u32, h: u32, scale: f64) -> (u32, u32) {
    let scale = scale.clamp(BUBBLE_IMAGE_SCALE_MIN, BUBBLE_IMAGE_SCALE_MAX);
    let (w, h) = (f64::from(w.max(1)), f64::from(h.max(1)));
    let ratio = BUBBLE_IMAGE_BASE / w.max(h);
    let tw = (w * ratio * scale).round().max(1.0) as u32;
    let th = (h * ratio * scale).round().max(1.0) as u32;
    (tw, th)
}

/// 双线性插值把 RGBA 源图缩放到目标尺寸，四个通道同步插值
fn scale_bilinear(src: &[u8], sw: u32, sh: u32, tw: u32, th: u32) -> Vec<u8> {
    let (sw, sh, tw, th) = (sw as usize, sh as usize, tw as usize, th as usize);
    let sample = |data: &[u8], w: usize, x: usize, y: usize, c: usize| -> f32 {
        f32::from(data[(y * w + x) * 4 + c])
    };
    let mut dst = vec![0u8; tw * th * 4];
    for ty in 0..th {
        for tx in 0..tw {
            // 目标像素中心映射回源图坐标，越界时取边缘值
            let fx = ((tx as f32 + 0.5) * sw as f32 / tw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let fy = ((ty as f32 + 0.5) * sh as f32 / th as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
            let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
            let (x1, y1) = ((x0 + 1).min(sw - 1), (y0 + 1).min(sh - 1));
            let (dx, dy) = (fx - x0 as f32, fy - y0 as f32);
            let d = (ty * tw + tx) * 4;
            for c in 0..4 {
                let top = sample(src, sw, x0, y0, c) * (1.0 - dx) + sample(src, sw, x1, y0, c) * dx;
                let bottom =
                    sample(src, sw, x0, y1, c) * (1.0 - dx) + sample(src, sw, x1, y1, c) * dx;
                dst[d + c] = (top * (1.0 - dy) + bottom * dy) as u8;
            }
        }
    }
    dst
}

/// 把 png 解码出的 8-bit 颜色布局统一成 RGBA；调色板图经过解码变换后不会出现，万一出现就按失败处理
fn normalize_rgba(raw: &[u8], ct: png::ColorType) -> Option<Vec<u8>> {
    match ct {
        png::ColorType::Rgba => Some(raw.to_vec()),
        png::ColorType::Rgb => Some(
            raw.chunks(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
        ),
        png::ColorType::GrayscaleAlpha => Some(
            raw.chunks(2)
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
        ),
        png::ColorType::Grayscale => Some(
            raw.chunks(1)
                .flat_map(|p| [p[0], p[0], p[0], 255])
                .collect(),
        ),
        png::ColorType::Indexed => None,
    }
}

/// 解码 png 并缩放到给定尺寸（尺寸由调用方先用 `fit_image_size` 算好），
/// 居中放进带内边距的气泡像素缓冲；背景用与文字气泡相同的半透明白
pub fn render_bubble_image_pixels(png_bytes: &[u8], target: (u32, u32)) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    // 展开调色板和透明通道，16-bit 降到 8-bit，让输出统一是 RGBA
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().ok()?;
    let mut raw = vec![0u8; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut raw).ok()?;
    let rgba = normalize_rgba(&raw, frame.color_type)?;
    let scaled = scale_bilinear(&rgba, frame.width, frame.height, target.0, target.1);

    let pad = BUBBLE_PADDING as u32;
    let (w, h) = (target.0 + pad * 2, target.1 + pad * 2);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for py in pixels.chunks_mut(4) {
        py[0] = 255;
        py[1] = 255;
        py[2] = 255;
        py[3] = 220;
    }
    let off = (pad * w + pad) as usize * 4;
    for y in 0..target.1 as usize {
        let src_row = y * target.0 as usize * 4;
        let dst_row = off + y * w as usize * 4;
        pixels[dst_row..dst_row + target.0 as usize * 4]
            .copy_from_slice(&scaled[src_row..src_row + target.0 as usize * 4]);
    }
    Some(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, priority: Priority) -> BubbleContent {
        BubbleContent {
            media: BubbleMedia::Text(text.to_string()),
            duration_secs: 0.0,
            priority,
        }
    }

    fn enqueue(q: &mut BubbleQueue<BubbleContent>, text: &str, priority: Priority) {
        let content = msg(text, priority);
        q.push(content.priority, content);
    }

    #[test]
    fn queue_fifo_within_same_priority() {
        let mut q: BubbleQueue<BubbleContent> = BubbleQueue::new();
        enqueue(&mut q, "first", Priority::Normal);
        enqueue(&mut q, "second", Priority::Normal);
        enqueue(&mut q, "third", Priority::Normal);
        assert!(matches!(q.pop().unwrap().media, BubbleMedia::Text(ref t) if t == "first"));
        assert!(matches!(q.pop().unwrap().media, BubbleMedia::Text(ref t) if t == "second"));
        assert!(matches!(q.pop().unwrap().media, BubbleMedia::Text(ref t) if t == "third"));
        assert!(q.pop().is_none());
    }

    #[test]
    fn high_priority_jumps_queue() {
        let mut q: BubbleQueue<BubbleContent> = BubbleQueue::new();
        enqueue(&mut q, "low", Priority::Low);
        enqueue(&mut q, "normal", Priority::Normal);
        enqueue(&mut q, "urgent", Priority::High);
        assert!(matches!(q.pop().unwrap().media, BubbleMedia::Text(ref t) if t == "urgent"));
        assert!(matches!(q.pop().unwrap().media, BubbleMedia::Text(ref t) if t == "normal"));
        assert!(matches!(q.pop().unwrap().media, BubbleMedia::Text(ref t) if t == "low"));
    }

    #[test]
    fn queue_length_and_empty() {
        let mut q: BubbleQueue<BubbleContent> = BubbleQueue::default();
        assert!(q.is_empty());
        enqueue(&mut q, "a", Priority::Normal);
        assert_eq!(q.len(), 1);
        q.pop();
        assert!(q.is_empty());
    }

    fn encode_png_as(color: png::ColorType, width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, width, height);
            enc.set_color(color);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        out
    }

    fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        encode_png_as(png::ColorType::Rgba, width, height, rgba)
    }

    fn pixel_at(pixels: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = (y * w + x) as usize * 4;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }

    #[test]
    fn image_fit_keeps_ratio_and_scales_longer_side() {
        let base = BUBBLE_IMAGE_BASE as u32;
        // 长边缩到基准，短边按比例；横图竖图各验一次
        assert_eq!(fit_image_size(base * 2, base, 1.0), (base, base / 2));
        assert_eq!(fit_image_size(base, base * 2, 1.0), (base / 2, base));
        // 系数乘在两维上
        assert_eq!(fit_image_size(base * 2, base, 2.0), (base * 2, base));
        // 系数超出范围时按边界值
        assert_eq!(fit_image_size(100, 50, 10.0), fit_image_size(100, 50, 3.0));
        assert_eq!(fit_image_size(100, 50, 0.0), fit_image_size(100, 50, 0.5));
    }

    #[test]
    fn image_render_centers_image_on_background() {
        // 4×4 纯色 png 缩到 2×2：中心两行是图像，四周内边距是背景色
        let png = encode_png(4, 4, &[200u8, 30, 30, 255].repeat(4 * 4));
        let pixels = render_bubble_image_pixels(&png, (2, 2)).expect("应能解码渲染");
        let pad = BUBBLE_PADDING as u32;
        let w = 2 + pad * 2;
        assert_eq!(pixels.len(), (w * w * 4) as usize);
        assert_eq!(pixel_at(&pixels, w, pad, pad), [200, 30, 30, 255]);
        assert_eq!(pixel_at(&pixels, w, 0, 0), [255, 255, 255, 220]);
        // 图像 alpha 原样保留（无黑底）
        assert_eq!(pixel_at(&pixels, w, pad, pad)[3], 255);
    }

    #[test]
    fn image_render_expands_non_rgba_colors() {
        // 同一个灰度值 / RGB / 灰度+alpha 的 1×1 png 渲染后都应展开成对应的 RGBA
        let cases: [(png::ColorType, Vec<u8>, [u8; 4]); 3] = [
            (png::ColorType::Rgb, vec![1, 2, 3], [1, 2, 3, 255]),
            (png::ColorType::Grayscale, vec![77], [77, 77, 77, 255]),
            (
                png::ColorType::GrayscaleAlpha,
                vec![77, 200],
                [77, 77, 77, 200],
            ),
        ];
        for (color, data, want) in cases {
            let png = encode_png_as(color, 1, 1, &data);
            let pixels = render_bubble_image_pixels(&png, (1, 1)).expect("应能解码渲染");
            let pad = BUBBLE_PADDING as u32;
            let w = 1 + pad * 2;
            assert_eq!(pixel_at(&pixels, w, pad, pad), want);
        }
    }

    #[test]
    fn image_render_rejects_garbage_bytes() {
        assert!(render_bubble_image_pixels(b"not a png", (2, 2)).is_none());
    }

    #[test]
    fn bilinear_solid_color_stays_solid() {
        // 纯色图任意缩放后仍是同一颜色
        let solid = [10u8, 20, 30, 255].repeat(2 * 2);
        assert_eq!(
            scale_bilinear(&solid, 2, 2, 4, 4),
            [10u8, 20, 30, 255].repeat(4 * 4)
        );
        // 黑白各半的 2×1 图放大到 4×1：第二格落在黑白之间 1/4 处，颜色按权重取值
        let src = [[0u8, 0, 0, 255], [255, 255, 255, 255]].concat();
        let scaled = scale_bilinear(&src, 2, 1, 4, 1);
        assert_eq!(pixel_at(&scaled, 4, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel_at(&scaled, 4, 1, 0)[0], (255.0 * 0.25) as u8);
        assert_eq!(pixel_at(&scaled, 4, 3, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn paginate_empty() {
        assert_eq!(paginate(&[]), Vec::<Vec<crate::text::LineSpan>>::new());
    }

    #[test]
    fn paginate_short_text_single_page() {
        let lines = vec![crate::text::LineSpan {
            start: 0,
            end: 5,
            width: 100.0,
        }];
        let pages = paginate(&lines);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].len(), 1);
    }

    #[test]
    fn paginate_many_lines_splits() {
        let lines: Vec<_> = (0..12)
            .map(|i| crate::text::LineSpan {
                start: i * 10,
                end: i * 10 + 10,
                width: 100.0,
            })
            .collect();
        let pages = paginate(&lines);
        // 12 行 / 每页 5 行 = 3 页（5 + 5 + 2，末页 2 行不需要匀）
        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].len(), 5);
        assert_eq!(pages[1].len(), 5);
        assert_eq!(pages[2].len(), 2);
    }

    #[test]
    fn paginate_orphan_line_rebalanced() {
        // 6 行 = 5 + 1 → 末页孤行从前一页匀一行 → 4 + 2
        let lines: Vec<_> = (0..6)
            .map(|i| crate::text::LineSpan {
                start: i * 10,
                end: i * 10 + 10,
                width: 100.0,
            })
            .collect();
        let pages = paginate(&lines);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].len(), 4);
        assert_eq!(pages[1].len(), 2);
    }

    #[test]
    fn bubble_above_pet_when_space_available() {
        let (x, y) = bubble_position((100.0, 200.0), (320.0, 180.0), (280.0, 60.0));
        assert_eq!(y, 200.0 - 60.0 - 8.0);
        assert_eq!(x, 100.0 + (320.0 - 280.0) / 2.0);
    }

    #[test]
    fn bubble_below_pet_when_no_space_above() {
        let (_, y) = bubble_position((100.0, 20.0), (320.0, 180.0), (280.0, 60.0));
        assert_eq!(y, 20.0 + 180.0 + 8.0);
    }

    #[test]
    fn bubble_position_never_negative() {
        let (x, y) = bubble_position((0.0, 100.0), (100.0, 100.0), (280.0, 50.0));
        assert!(x >= 0.0 && y >= 0.0);
    }

    #[test]
    fn bubble_size_scales_with_lines() {
        let (w, h) = bubble_size(1, 16.0);
        assert_eq!(w, BUBBLE_MAX_WIDTH);
        assert!((h - (16.0 * 1.3 + 24.0)).abs() < 0.01);
    }
}
#[test]
fn render_bubble_pixels_has_background_and_correct_size() {
    let text = "你好";
    let lines = crate::text::wrap_default(text, 200.0);
    let pixels = render_bubble_pixels(text, &lines, 16.0);
    let height = (lines.len() as f64 * 16.0 * 1.3 + BUBBLE_PADDING * 2.0) as usize;
    assert_eq!(pixels.len(), BUBBLE_MAX_WIDTH as usize * height * 4);
    // 左上角背景像素是半透明白色
    assert_eq!(&pixels[0..4], &[255, 255, 255, 220]);
}

#[test]
fn render_bubble_pixels_contains_text_pixels() {
    let text = "Hi";
    let lines = crate::text::wrap_default(text, 200.0);
    let pixels = render_bubble_pixels(text, &lines, 16.0);
    // 至少有一个非背景像素（文字笔画）
    let has_text = pixels.chunks(4).any(|px| px[3] != 220 || px[0] < 255);
    assert!(has_text, "像素里应该有文字，不全是背景色");
}
