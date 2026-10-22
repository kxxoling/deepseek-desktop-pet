//! 气泡窗口的创建与渲染接线（GUI 胶水，需真实窗口与 GPU，不在覆盖率口径）。
//! 布局、分页、光栅化等纯逻辑在 `bubble.rs`。

use std::sync::Arc;
use std::time::Instant;

use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId, WindowLevel};

use crate::bubble::{
    BUBBLE_DURATION_SECS, BUBBLE_MAX_WIDTH, BUBBLE_PADDING, bubble_position, bubble_size,
    render_bubble_pixels,
};
use crate::render::{Gpu, PetSurface};

/// 气泡窗口：在宠物上方显示文字，超时后由 main 关闭
pub struct BubbleWindow {
    window: Arc<Window>,
    surface: PetSurface,
    gpu: Arc<Gpu>,
    expires_at: Instant,
}

impl BubbleWindow {
    pub fn new(
        event_loop: &ActiveEventLoop,
        gpu: &Arc<Gpu>,
        pet_pos: (f64, f64),
        pet_size: (f64, f64),
        text: &str,
    ) -> Self {
        let lines =
            crate::text::wrap_default(text, (BUBBLE_MAX_WIDTH - BUBBLE_PADDING * 2.0) as f32);
        let font_size = 16.0;
        let (w, h) = bubble_size(lines.len(), font_size);
        let (x, y) = bubble_position(pet_pos, pet_size, (w, h));

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
            expires_at: Instant::now() + std::time::Duration::from_secs_f64(BUBBLE_DURATION_SECS),
        };
        bubble.show_text(text, &lines, font_size);
        bubble
    }

    /// 把文字画成像素并上传到 GPU
    fn show_text(&self, text: &str, lines: &[crate::text::LineSpan], font_size: f64) {
        let pixels = render_bubble_pixels(text, lines, font_size);
        self.surface.upload(&self.gpu, &pixels);
        self.window.request_redraw();
    }

    pub fn window_id(&self) -> WindowId {
        self.window.id()
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
