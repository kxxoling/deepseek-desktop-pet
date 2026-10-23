//! 气泡窗口的创建与渲染接线（GUI 胶水，需真实窗口与 GPU，不在覆盖率口径）。
//! 布局、分页、光栅化等纯逻辑在 `bubble.rs`。

use std::sync::Arc;
use std::time::Instant;

use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId, WindowLevel};

use crate::bubble::{
    BUBBLE_DURATION_SECS, BUBBLE_MAX_WIDTH, BUBBLE_PADDING, bubble_position, bubble_size,
    fit_image_size, paginate, render_bubble_image_pixels, render_bubble_pixels,
};
use crate::render::{Gpu, PetSurface};

/// 气泡窗口：在所属宠物上方显示文字或配图，长文分页、点击翻页，超时由 main 关闭
pub struct BubbleWindow {
    window: Arc<Window>,
    surface: PetSurface,
    gpu: Arc<Gpu>,
    pet_id: WindowId,
    /// 预渲染的每一页像素与尺寸，文字页与配图页结构相同
    pages: Vec<(Vec<u8>, u32, u32)>,
    page_idx: usize,
    /// 窗口逻辑尺寸（跟随宠物时复算位置用）
    size: (f64, f64),
    /// 上次设置的窗口位置，位置没有变化就不重复调用 `set_outer_position`
    last_pos: (f64, f64),
    expires_at: Instant,
}

impl BubbleWindow {
    /// 文字气泡：换行分页后逐页光栅化
    pub fn new_text(
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
        let font_size = 16.0;
        let pages: Vec<(Vec<u8>, u32, u32)> = paginate(&lines)
            .into_iter()
            .map(|page| {
                let pixels = render_bubble_pixels(text, &page, font_size);
                let (w, h) = bubble_size(page.len(), font_size);
                (pixels, w as u32, h as u32)
            })
            .collect();
        Self::from_pages(
            event_loop,
            gpu,
            pet_id,
            pet_pos,
            pet_size,
            pages,
            duration_secs,
        )
    }

    /// 配图气泡：按 config `bubble_image_scale` 等比缩放后铺在气泡底色上；解码失败返回 None
    pub fn new_image(
        event_loop: &ActiveEventLoop,
        gpu: &Arc<Gpu>,
        pet_id: WindowId,
        pet_pos: (f64, f64),
        pet_size: (f64, f64),
        png: &[u8],
        duration_secs: f64,
    ) -> Option<Self> {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let reader = decoder.read_info().ok()?;
        let (iw, ih) = (reader.info().width, reader.info().height);
        let scale = crate::config::get_f64("bubble_image_scale").unwrap_or(1.0);
        let target = fit_image_size(iw, ih, scale);
        let pixels = render_bubble_image_pixels(png, target)?;
        let pad = BUBBLE_PADDING as u32;
        Some(Self::from_pages(
            event_loop,
            gpu,
            pet_id,
            pet_pos,
            pet_size,
            vec![(pixels, target.0 + pad * 2, target.1 + pad * 2)],
            duration_secs,
        ))
    }

    /// 窗口按最大一页的尺寸创建：翻页只换内容，尺寸不变，避免窗口跳动
    #[allow(clippy::too_many_arguments)]
    fn from_pages(
        event_loop: &ActiveEventLoop,
        gpu: &Arc<Gpu>,
        pet_id: WindowId,
        pet_pos: (f64, f64),
        pet_size: (f64, f64),
        pages: Vec<(Vec<u8>, u32, u32)>,
        duration_secs: f64,
    ) -> Self {
        let (w, h) = pages.iter().fold((0u32, 0u32), |(mw, mh), (_, pw, ph)| {
            (mw.max(*pw), mh.max(*ph))
        });
        let (w, h) = (f64::from(w), f64::from(h));
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
            pages,
            page_idx: 0,
            size: (w, h),
            last_pos: (x, y),
            expires_at: Instant::now() + std::time::Duration::from_secs_f64(duration),
        };
        bubble.render_page();
        bubble
    }

    /// 把当前页上传到 GPU
    fn render_page(&self) {
        let (pixels, _, _) = &self.pages[self.page_idx];
        self.surface.upload(&self.gpu, pixels);
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
