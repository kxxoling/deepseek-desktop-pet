//! 气泡窗口的创建与渲染接线（GUI 胶水，需真实窗口与 GPU，不在覆盖率口径）。
//! 布局、分页、光栅化等纯逻辑在 `bubble.rs`。

use std::sync::Arc;
use std::time::Instant;

use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId, WindowLevel};

use crate::bubble::{
    BUBBLE_DURATION_SECS, BUBBLE_MAX_WIDTH, BUBBLE_PADDING, bubble_position, bubble_size, paginate,
    render_bubble_pixels,
};
use crate::render::{Gpu, PetSurface};

/// 气泡窗口：在所属宠物上方显示文字，长文分页、点击翻页，超时由 main 关闭
pub struct BubbleWindow {
    window: Arc<Window>,
    surface: PetSurface,
    gpu: Arc<Gpu>,
    pet_id: WindowId,
    text: String,
    pages: Vec<Vec<crate::text::LineSpan>>,
    page_idx: usize,
    font_size: f64,
    /// 窗口逻辑尺寸（跟随宠物时复算位置用）
    size: (f64, f64),
    /// 上次设置的窗口位置，位置没有变化就不重复调用 `set_outer_position`
    last_pos: (f64, f64),
    expires_at: Instant,
}

impl BubbleWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_loop: &ActiveEventLoop,
        gpu: &Arc<Gpu>,
        pet_id: WindowId,
        pet_pos: (f64, f64),
        pet_size: (f64, f64),
        text: &str,
        duration_secs: f64,
    ) -> Self {
        let lines =
            crate::text::wrap_default(text, (BUBBLE_MAX_WIDTH - BUBBLE_PADDING * 2.0) as f32);
        let pages = paginate(&lines);
        let font_size = 16.0;
        // 窗口高度按最大一页算：翻页只换内容，尺寸不变，避免窗口跳动
        let max_lines = pages.iter().map(Vec::len).max().unwrap_or_default();
        let (w, h) = bubble_size(max_lines, font_size);
        let (x, y) = bubble_position(pet_pos, pet_size, (w, h));
        let duration = if duration_secs > 0.0 {
            duration_secs
        } else {
            BUBBLE_DURATION_SECS
        };

        let attrs = Window::default_attributes()
            .with_title("ds-pet-bubble")
            .with_inner_size(LogicalSize::new(w, h))
            .with_position(LogicalPosition::new(x, y))
            .with_transparent(true)
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_active(false);

        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        #[cfg(not(target_os = "windows"))]
        let surface = gpu
            .create_surface(window.clone())
            .expect("气泡 surface 创建失败");
        #[cfg(target_os = "windows")]
        let surface = gpu.create_surface(&window);

        let bubble = Self {
            window,
            surface,
            gpu: gpu.clone(),
            pet_id,
            text: text.to_string(),
            pages,
            page_idx: 0,
            font_size,
            size: (w, h),
            last_pos: (x, y),
            expires_at: Instant::now() + std::time::Duration::from_secs_f64(duration),
        };
        bubble.render_page();
        bubble
    }

    /// 把当前页画成像素并上传到 GPU
    fn render_page(&self) {
        let pixels = render_bubble_pixels(&self.text, &self.pages[self.page_idx], self.font_size);
        self.surface.upload(&self.gpu, &pixels);
        self.window.request_redraw();
    }

    /// 翻到下一页；已在末页返回 false，由调用方关闭窗口
    pub fn next_page(&mut self) -> bool {
        if self.page_idx + 1 >= self.pages.len() {
            return false;
        }
        self.page_idx += 1;
        self.render_page();
        true
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 气泡所属宠物的窗口 id（跟随移动时定位用）
    pub fn pet_id(&self) -> WindowId {
        self.pet_id
    }

    /// 跟随所属宠物移动；目标位置没有变化就不重复挪动窗口
    pub fn follow(&mut self, pet_pos: (f64, f64), pet_size: (f64, f64)) {
        let (x, y) = bubble_position(pet_pos, pet_size, self.size);
        if (x, y) != self.last_pos {
            self.last_pos = (x, y);
            self.window.set_outer_position(LogicalPosition::new(x, y));
        }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// 渲染到屏幕（RedrawRequested 时由 main 调用）
    // Windows 分层窗口路径待接入，方法体为空，self 暂未使用
    #[cfg_attr(target_os = "windows", allow(clippy::unused_self))]
    pub fn draw(&mut self) {
        #[cfg(not(target_os = "windows"))]
        {
            let _ = self.surface.render(&self.gpu);
        }
        #[cfg(target_os = "windows")]
        {
            // Windows 分层窗口路径需要 hwnd 和 bitmap，待后续接入
        }
    }
}
