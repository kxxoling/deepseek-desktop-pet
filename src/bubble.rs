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
    pub text: String,
    /// 显示秒数，0 用默认值
    pub duration_secs: f64,
    pub priority: Priority,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, priority: Priority) -> BubbleContent {
        BubbleContent {
            text: text.to_string(),
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
        assert_eq!(q.pop().unwrap().text, "first");
        assert_eq!(q.pop().unwrap().text, "second");
        assert_eq!(q.pop().unwrap().text, "third");
        assert!(q.pop().is_none());
    }

    #[test]
    fn high_priority_jumps_queue() {
        let mut q: BubbleQueue<BubbleContent> = BubbleQueue::new();
        enqueue(&mut q, "low", Priority::Low);
        enqueue(&mut q, "normal", Priority::Normal);
        enqueue(&mut q, "urgent", Priority::High);
        assert_eq!(q.pop().unwrap().text, "urgent");
        assert_eq!(q.pop().unwrap().text, "normal");
        assert_eq!(q.pop().unwrap().text, "low");
    }

    #[test]
    fn queue_length_and_empty() {
        let mut q: BubbleQueue<BubbleContent> = BubbleQueue::new();
        assert!(q.is_empty());
        enqueue(&mut q, "a", Priority::Normal);
        assert_eq!(q.len(), 1);
        q.pop();
        assert!(q.is_empty());
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
