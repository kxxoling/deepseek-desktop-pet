//! 宠物窗口：渲染与交互（拖动/惯性、双击多开、单击、右键菜单、点击穿透、行走）

use std::sync::Arc;
use std::time::Instant;

use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{CursorIcon, Window, WindowId, WindowLevel};

use crate::UserEvent;
use crate::anim::{Anim, Mode, Tick, WalkDir, curve_at, random_pick};
use crate::input::{Action, InputState};
use crate::library::{ClipId, FRAME_BYTES, FRAME_H, FRAME_W, Library};
use crate::platform;
use crate::render::{Gpu, PetSurface};

/// 窗口逻辑尺寸（= 帧尺寸；整数倍缩放像素精确）
pub const WIN_W: f64 = 320.0;
pub const WIN_H: f64 = 180.0;
/// 点击穿透 alpha 阈值
const HIT_ALPHA: u8 = 24;
pub const MAX_PETS: usize = 8;

pub enum WindowRequest {
    /// 在指定物理坐标（窗口内）弹出右键菜单
    ShowMenu(i32, i32),
}

struct DragState {
    /// 按下时 光标全局位置 - 窗口位置（逻辑点）
    offset: (f64, f64),
}

pub struct PetWindow {
    pub window: Arc<Window>,
    surface: PetSurface,
    /// 共享 GPU 上下文（App 持有）
    gpu: Arc<Gpu>,
    #[cfg(target_os = "windows")]
    hwnd: Option<windows::Win32::Foundation::HWND>,
    #[cfg(target_os = "windows")]
    bitmap: Option<crate::platform::windows_layered::LayeredBitmap>,
    /// 镜像帧的临时翻转缓冲（避免每帧分配）
    mirror_scratch: Vec<u8>,
    /// Q 弹变形的输出缓冲
    squash_scratch: Vec<u8>,
    /// 点击 Q 弹起始时刻
    squash_start: Option<Instant>,
    pub anim: Anim,
    lib: Arc<Library>,
    proxy: EventLoopProxy<UserEvent>,

    drag: Option<DragState>,
    /// 拖动速度 EMA（逻辑 px/ms），松手后作惯性初速
    velocity: (f64, f64),
    last_drag_move: Option<(Instant, (f64, f64))>, // (时刻, 窗口逻辑位置)
    last_tick_at: Instant,

    /// 光标窗口内位置（逻辑点，左上原点）；被穿透忽略期间由轮询维护
    cursor_local: (f64, f64),
    /// 最近一次 `CursorMoved` 的物理坐标（右键菜单弹出示位置）
    cursor_phys: (f64, f64),
    ignoring_cursor: bool,

    /// 单击/双击/拖拽判定（纯状态机，见 input.rs）
    input: InputState,

    /// 行走状态（步幅已换算到逻辑像素：`stride_native` × 320/640）
    walk_stride: f64,
    walk_last_curve: f64,

    /// 窗口整体不透明度（Windows 提交分层窗口时随帧携带；macOS 一次设置即持续生效）
    #[cfg(target_os = "windows")]
    opacity: f64,
    /// 探头姿态：倾角与进入时刻（None = 非探头）
    probe: Option<(f64, Instant)>,
    /// 旋转渲染的输出缓冲
    rotate_scratch: Vec<u8>,
    /// 最近一次交互时刻（省电判定用）
    last_interaction: Instant,
    /// 锁定位置：不受理拖拽（点击/双击不受影响，config `lock_position`）
    lock_position: bool,
    /// 仅允许按住 SHIFT 拖动（config `shift_drag_only`）
    shift_drag_only: bool,
    /// 当前键盘修饰键状态（SHIFT 拖动模式判定用）
    modifiers: winit::keyboard::ModifiersState,
    /// 省电阈值秒数（0=关闭，config `power_saving_idle_secs`）
    power_save_idle_secs: u64,
    /// 已渲染帧计数（省电时隔帧渲染的除法基准）
    render_step: u64,
    /// 甩出力度的倍率（config `throw_strength`，读取时已钳制）
    throw_mult: f64,
}

impl PetWindow {
    pub fn new(
        event_loop: &ActiveEventLoop,
        lib: Arc<Library>,
        proxy: EventLoopProxy<UserEvent>,
        gpu: Arc<Gpu>,
        pos: Option<(f64, f64)>,
        frame_offset: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let attrs = Window::default_attributes()
            .with_title("ds-pet")
            .with_inner_size(LogicalSize::new(WIN_W, WIN_H))
            .with_transparent(true)
            .with_decorations(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_resizable(false)
            .with_active(false);
        let window: Arc<Window> = Arc::new(event_loop.create_window(attrs)?);
        // Windows：物理尺寸取整到帧尺寸整数倍（非整数 DPI 下 nearest 采样不均匀会发糊）
        #[cfg(target_os = "windows")]
        {
            let scale = window.scale_factor().max(1.0).round().max(1.0) as u32;
            let phys =
                winit::dpi::PhysicalSize::new((WIN_W as u32) * scale, (WIN_H as u32) * scale);
            let _ = window.request_inner_size(phys);
        }
        if let Some((x, y)) = pos {
            window.set_outer_position(LogicalPosition::new(x, y));
        }
        #[cfg(target_os = "windows")]
        let (hwnd, bitmap) = {
            let hwnd = crate::platform::windows_layered::hwnd(&window);
            if let Some(h) = hwnd {
                crate::platform::windows_layered::make_layered(h);
            }
            let size = window.inner_size();
            (
                hwnd,
                crate::platform::windows_layered::LayeredBitmap::new(
                    size.width.max(1),
                    size.height.max(1),
                ),
            )
        };
        #[cfg(not(target_os = "windows"))]
        let surface = gpu
            .create_surface(window.clone())
            .ok_or("surface 创建失败")?;
        #[cfg(target_os = "windows")]
        let surface = gpu.create_surface(&window);

        let idle = lib.find_idle().ok_or("缺少 idle 片段")?;
        let mut anim = Anim::new(
            &idle,
            lib.first_frame(&idle).cloned().expect("idle 首帧缺失"),
            lib.get_or_load(&idle, crate::clip_loader(&proxy)),
        );
        // config 的播放速率（缺省 1.0），越界钳制在 set_speed 内
        if let Some(speed) = crate::config::get_f64("playback_speed") {
            anim.set_speed(speed);
        }
        // trigger_clip 的候选片段池：events 类全部片段，首帧直接共享库缓存
        anim.set_event_clips(
            lib.clips_in_category("events")
                .into_iter()
                .filter_map(|id| lib.first_frame(&id).cloned().map(|f| (id, f)))
                .collect(),
        );
        // 多开时随机起始相位，避免所有宠物同帧同步
        if frame_offset > 0 && anim.is_loaded() {
            anim.frame_idx = frame_offset % anim.frame_len().max(1);
        }

        // config 的不透明度（缺省 1.0），钳制在 config 侧完成
        let opacity = crate::config::get_f64("opacity").map_or(1.0, crate::config::clamp_opacity);
        platform::set_window_opacity(&window, opacity);
        let mut pet = Self {
            window,
            surface,
            gpu,
            #[cfg(target_os = "windows")]
            hwnd,
            #[cfg(target_os = "windows")]
            bitmap,
            mirror_scratch: vec![0u8; FRAME_BYTES],
            squash_scratch: vec![0u8; FRAME_BYTES],
            squash_start: None,
            anim,
            lib,
            proxy,
            drag: None,
            velocity: (0.0, 0.0),
            last_drag_move: None,
            last_tick_at: Instant::now(),
            cursor_local: (0.0, 0.0),
            cursor_phys: (0.0, 0.0),
            ignoring_cursor: false,
            input: InputState::default(),
            walk_stride: 0.0,
            walk_last_curve: 0.0,
            #[cfg(target_os = "windows")]
            opacity,
            probe: None,
            rotate_scratch: vec![0u8; FRAME_BYTES],
            last_interaction: Instant::now(),
            lock_position: crate::config::get_bool("lock_position").unwrap_or(false),
            shift_drag_only: crate::config::get_bool("shift_drag_only").unwrap_or(false),
            modifiers: winit::keyboard::ModifiersState::default(),
            power_save_idle_secs: crate::config::get_i64("power_saving_idle_secs")
                .map_or(0, |v| v.max(0) as u64),
            render_step: 0,
            throw_mult: crate::config::get_f64("throw_strength")
                .map_or(1.0, crate::physics::clamp_throw_strength),
        };
        pet.blit();
        Ok(pet)
    }

    // ---------- 渲染 ----------

    /// 当前帧上传纹理并请求重绘
    fn blit(&mut self) {
        let frame = self.anim.current_frame();
        if self.anim.mirrored {
            mirror_blit(&mut self.mirror_scratch, frame);
        }
        let src: &[u8] = if self.anim.mirrored {
            &self.mirror_scratch
        } else {
            frame
        };
        // 点击 Q 弹期间按当前比例把整帧纵向压缩（身体压向画面底部），结束后自动恢复
        if let Some(t0) = self.squash_start {
            let t = t0.elapsed().as_secs_f64() * 1000.0;
            let scale = crate::render::squash_scale(t);
            if scale < 1.0 {
                squash_into(&mut self.squash_scratch, src, scale);
                // 探头姿态下压缩结果再整体倾斜
                if let Some((angle, _)) = self.probe {
                    crate::render::rotate_into(
                        &mut self.rotate_scratch,
                        &self.squash_scratch,
                        angle,
                        crate::physics::BodyBox::DEFAULT.center(),
                    );
                    self.surface.upload(&self.gpu, &self.rotate_scratch);
                } else {
                    self.surface.upload(&self.gpu, &self.squash_scratch);
                }
                self.window.request_redraw();
                return;
            }
            self.squash_start = None;
        }
        // 探头姿态：整帧绕身体中心倾斜
        if let Some((angle, _)) = self.probe {
            crate::render::rotate_into(
                &mut self.rotate_scratch,
                src,
                angle,
                crate::physics::BodyBox::DEFAULT.center(),
            );
            self.surface.upload(&self.gpu, &self.rotate_scratch);
        } else {
            self.surface.upload(&self.gpu, src);
        }
        self.window.request_redraw();
    }

    pub fn render(&mut self) {
        #[cfg(not(target_os = "windows"))]
        let r = self.surface.render(&self.gpu);
        #[cfg(target_os = "windows")]
        let alpha = (self.opacity * 255.0) as u8;
        #[cfg(target_os = "windows")]
        let r = if let (Some(h), Some(b)) = (self.hwnd, self.bitmap.as_mut()) {
            self.surface.render(&self.gpu, h, b, alpha)
        } else {
            log::error!("Windows 渲染上下文缺失（hwnd/bitmap）");
            return;
        };
        if let Err(e) = r {
            log::error!(
                "渲染失败（{:?}）：{e:?}，尝试重建 surface",
                self.window.id()
            );
            #[cfg(not(target_os = "windows"))]
            if let Some(s) = self.gpu.create_surface(self.window.clone()) {
                self.surface = s;
                self.blit();
            }
            #[cfg(target_os = "windows")]
            {
                self.surface = self.gpu.create_surface(&self.window);
                self.blit();
            }
        }
    }

    fn resize_surface(&mut self) {
        #[cfg(target_os = "windows")]
        {
            // 保持物理尺寸为帧尺寸整数倍（同创建时的 snap）
            let scale = self.window.scale_factor().max(1.0).round().max(1.0) as u32;
            let phys =
                winit::dpi::PhysicalSize::new((WIN_W as u32) * scale, (WIN_H as u32) * scale);
            if self.window.inner_size() != phys {
                let _ = self.window.request_inner_size(phys);
            }
        }
        let size = self.window.inner_size();
        self.surface
            .resize(&self.gpu, size.width.max(1), size.height.max(1));
        log::info!(
            "窗口尺寸/缩放变化，surface 已重配为 {}×{}",
            size.width,
            size.height
        );
        #[cfg(target_os = "windows")]
        {
            self.bitmap = crate::platform::windows_layered::LayeredBitmap::new(
                size.width.max(1),
                size.height.max(1),
            );
        }
    }

    // ---------- 命中测试与点击穿透 ----------

    fn hit_test(&self, lx: f64, ly: f64) -> bool {
        if !(0.0..=WIN_W).contains(&lx) || !(0.0..=WIN_H).contains(&ly) {
            return false;
        }
        let x = lx as usize;
        let y = ly as usize;
        // 镜像后显示像素来自源帧的水平翻转位置
        let sx = if self.anim.mirrored {
            FRAME_W as usize - 1 - x
        } else {
            x
        };
        self.anim.current_frame()[(y * FRAME_W as usize + sx) * 4 + 3] > HIT_ALPHA
    }

    /// 命中测试的光标位置：实时全局查询（`MouseInput` 不携带坐标）
    fn hit_test_at_cursor(&mut self) -> bool {
        if let Some((lx, ly)) = platform::global_mouse_local(&self.window) {
            self.cursor_local = (lx, ly);
        }
        self.hit_test(self.cursor_local.0, self.cursor_local.1)
    }

    /// 全局轮询维护点击穿透（macOS；被忽略的窗口收不到事件，只能轮询恢复）
    fn hit_poll(&mut self) {
        if self.drag.is_some() {
            return; // 拖拽中必须接收事件
        }
        if let Some((lx, ly)) = platform::global_mouse_local(&self.window)
            && (0.0..=WIN_W).contains(&lx)
            && (0.0..=WIN_H).contains(&ly)
        {
            self.cursor_local = (lx, ly);
            let want_ignore = !self.hit_test(lx, ly);
            if want_ignore != self.ignoring_cursor
                && platform::set_ignore_cursor_events(&self.window, want_ignore)
            {
                self.ignoring_cursor = want_ignore;
            }
        }
    }

    // ---------- 窗口定位 ----------

    pub fn window_pos_logical(&self) -> Option<(f64, f64)> {
        let scale = self.window.scale_factor();
        self.window.outer_position().ok().map(|p| {
            let l: LogicalPosition<f64> = p.to_logical(scale);
            (l.x, l.y)
        })
    }

    fn monitor_size_logical(&self) -> Option<(f64, f64)> {
        let mon = self
            .window
            .current_monitor()
            .or_else(|| self.window.primary_monitor())?;
        let scale = mon.scale_factor();
        let s: LogicalSize<f64> = mon.size().to_logical(scale);
        Some((s.width, s.height))
    }

    /// 相对位移（逻辑 px）；x 越界翻转行走方向，y 钳制在屏幕内
    fn move_window_by(&mut self, dx: f64, dy: f64, flip_on_x: bool) {
        let Some((mut x, mut y)) = self.window_pos_logical() else {
            return;
        };
        x += dx;
        y += dy;
        if let Some((mw, mh)) = self.monitor_size_logical() {
            let mut flipped = false;
            if x < 2.0 {
                x = 2.0;
                flipped = true;
            }
            if x + WIN_W > mw - 2.0 {
                x = (mw - WIN_W - 2.0).max(2.0);
                flipped = true;
            }
            y = y.clamp(2.0, (mh - WIN_H - 2.0).max(2.0));
            if flipped && flip_on_x {
                if let Mode::Walk { dir, .. } = &mut self.anim.mode {
                    *dir = dir.flip();
                }
                self.update_walk_mirror();
            }
        }
        self.window.set_outer_position(LogicalPosition::new(x, y));
    }

    /// 朝右行走时镜像（`no_mirror` 名单除外）
    fn update_walk_mirror(&mut self) {
        if let Mode::Walk { dir, .. } = &self.anim.mode {
            let name = self
                .lib
                .meta(&self.anim.clip)
                .map(|m| m.name.to_string())
                .unwrap_or_default();
            self.anim.mirrored = *dir == WalkDir::Right && !self.lib.no_mirror(&name);
        }
    }

    // ---------- 状态机驱动 ----------

    /// 经帧库切换片段（含懒加载触发与首帧兜底）
    fn switch_clip(&mut self, id: &ClipId, mode: Mode, mirrored: bool) {
        let first = self.lib.first_frame(id).cloned().expect("首帧缓存缺失");
        let frames = self.lib.get_or_load(id, crate::clip_loader(&self.proxy));
        self.anim.set_clip(id, first, frames, mode, mirrored);
    }

    /// 触发播放特殊片段（受理规则见动画状态机）
    pub fn trigger_clip(&mut self, spec: &str) -> bool {
        let accepted = self.anim.trigger_clip(spec);
        if accepted {
            self.blit();
        }
        accepted
    }

    fn enter_idle(&mut self) {
        if let Some(idle) = self.lib.find_idle() {
            let idle = idle.clone();
            let first = self.lib.first_frame(&idle).cloned().expect("idle 首帧缺失");
            let frames = self.lib.get_or_load(&idle, crate::clip_loader(&self.proxy));
            self.anim.enter_idle(&idle, first, frames);
            self.blit();
        }
    }

    fn play_oneshot_in(&mut self, category: &str) {
        let clips = self.lib.clips_in_category(category);
        if let Some(clip) = random_pick(&clips).cloned() {
            self.switch_clip(&clip, Mode::OneShot, self.anim.mirrored);
            self.blit();
        }
    }

    /// Idle 态随机事件：turn 穿插 / 行走 / 随机动画（决策见 `anim::plan_idle_event`）
    fn fire_random_event(&mut self) {
        let snap = crate::anim::sched_snapshot(&self.lib);
        let plan = crate::anim::plan_idle_event(
            &snap,
            crate::anim::rand_range(0.0, 1.0),
            crate::anim::rand_range(0.0, 1.0),
            crate::anim::rand_u64(),
        );
        self.exec_plan(plan);
    }

    /// 执行调度计划（决策在 anim.rs，剪辑切换副作用在本层）
    fn exec_plan(&mut self, plan: crate::anim::Plan) {
        use crate::anim::Plan;
        match plan {
            Plan::Idle => self.enter_idle(),
            Plan::RandomClip => self.play_oneshot_in("random"),
            Plan::Walk { loops } => self.start_walk(loops),
            Plan::TurnThen(next) => {
                // 仅当库里有 turn 片段才会生成 TurnThen；选取失败则直接执行后续
                if let Some(clip) = random_pick(&self.lib.clips_in_category("turn")).cloned() {
                    self.switch_clip(&clip, Mode::OneShot, self.anim.mirrored);
                    self.anim.set_pending(*next);
                    self.blit();
                } else {
                    self.exec_plan(*next);
                }
            }
        }
    }

    /// 进入行走（方向随机；步幅与镜像沿用既有逻辑）
    fn start_walk(&mut self, loops: u32) {
        let moves = self.lib.clips_in_category("move");
        let with_stride: Vec<(ClipId, f64)> = moves
            .into_iter()
            .filter_map(|id| {
                let name = self.lib.meta(&id).map(|m| m.name.to_string())?;
                let (stride, _) = self.lib.move_strides.get(&name)?;
                Some((id, *stride))
            })
            .collect();
        if let Some((clip, stride)) = random_pick(&with_stride).cloned() {
            let dir = if crate::anim::rand_range(0.0, 1.0) < 0.5 {
                WalkDir::Left
            } else {
                WalkDir::Right
            };
            self.switch_clip(
                &clip,
                Mode::Walk {
                    dir,
                    remaining_loops: loops,
                },
                false,
            );
            // 步幅：源坐标 640 宽 → 窗口逻辑坐标 320 宽（等比 0.5）
            self.walk_stride = stride * 0.5;
            self.walk_last_curve = 0.0;
            self.update_walk_mirror();
            self.blit();
        }
    }

    /// 行走单帧位移（按 24→15fps 重采样后的曲线差分）
    fn walk_step(&mut self) {
        let Mode::Walk { .. } = self.anim.mode else {
            return;
        };
        let Some(name) = self.lib.meta(&self.anim.clip).map(|m| m.name.to_string()) else {
            return;
        };
        let Some((_, curve)) = self.lib.move_strides.get(&name).cloned() else {
            return;
        };
        let cur = curve_at(&curve, self.anim.frame_idx);
        let mut delta = cur - self.walk_last_curve;
        if delta < 0.0 {
            delta += 1.0; // 回绕到新一圈
        }
        self.walk_last_curve = cur;
        let Mode::Walk { dir, .. } = self.anim.mode else {
            return;
        };
        let dx = delta * self.walk_stride * dir.sign();
        self.move_window_by(dx, 0.0, true);
    }

    /// 窗口状态同步进物理世界：拖拽中无限重只挡不弹，松手后的滑行是飞行，其余静止
    pub fn sync_body_out(&mut self, body: &mut crate::physics::PetBody) {
        if let Some((x, y)) = self.window_pos_logical() {
            body.pos = (x, y);
        }
        if self.drag.is_some() {
            body.state = crate::physics::PetState::Dragging;
            body.vel = (0.0, 0.0);
        } else if matches!(self.anim.mode, Mode::Drag) {
            // 交接瞬间带上拖拽速度（px/ms 换算成 px/s 后按力度倍率放大），之后速度归物理世界管
            if body.state != crate::physics::PetState::Flying {
                body.state = crate::physics::PetState::Flying;
                body.vel = (
                    self.velocity.0 * 1000.0 * self.throw_mult,
                    self.velocity.1 * 1000.0 * self.throw_mult,
                );
            }
        } else {
            body.state = crate::physics::PetState::Resting;
            body.vel = (0.0, 0.0);
        }
    }

    /// 物理解算写回：位置变化落到窗口，落稳（飞行转静止）后回 Idle
    pub fn apply_body(&mut self, body: &crate::physics::PetBody) {
        if matches!(self.anim.mode, Mode::Drag) && body.state == crate::physics::PetState::Resting {
            self.enter_idle();
        }
        if let Some((x, y)) = self.window_pos_logical()
            && (x - body.pos.0).abs() + (y - body.pos.1).abs() > 0.5
        {
            self.window
                .set_outer_position(LogicalPosition::new(body.pos.0, body.pos.1));
        }
    }

    /// 主 tick（约 60Hz 调用）：随机事件、帧推进、穿透轮询
    pub fn tick(&mut self, now: Instant) {
        self.last_tick_at = now;

        // trigger_clip 自己切换片段时没法顺带启动加载（加载通道在窗口层），
        // 这里每帧检查排队中的片段并补上；重复调用是安全的
        if let Some(id) = self.anim.queued_clip()
            && let Some(frames) = self.lib.get_or_load(&id, crate::clip_loader(&self.proxy))
        {
            self.anim.on_clip_loaded(&id, frames);
        }

        // 探头期间拦住随机游走（上游的教训：不拦的话宠物会挂着探头被平移出屏），到时自动退回；不在探头姿态才允许随机事件
        if let Some((_, since)) = self.probe
            && now.duration_since(since).as_secs_f64() >= crate::render::PROBE_RETREAT_SECS
        {
            self.probe = None;
            self.blit();
        }
        if self.probe.is_none()
            && matches!(self.anim.mode, Mode::Idle)
            && now >= self.anim.next_event_at
        {
            self.fire_random_event();
        }

        let advanced = match self.anim.tick(now) {
            Tick::Idle => false,
            Tick::Advanced => true,
            Tick::Finished => {
                // 播完有后续计划则执行，否则回 Idle
                if let Some(next) = self.anim.take_pending() {
                    self.exec_plan(next);
                } else {
                    self.enter_idle();
                }
                true
            }
            Tick::WalkLoopDone => {
                let mut done = false;
                if let Mode::Walk {
                    remaining_loops, ..
                } = &mut self.anim.mode
                {
                    *remaining_loops = remaining_loops.saturating_sub(1);
                    done = *remaining_loops == 0;
                }
                if done {
                    // 行走途中受理的触发先于走完后的自动转身播放
                    if let Some(id) = self.anim.take_pending_trigger() {
                        self.switch_clip(&id, Mode::OneShot, self.anim.mirrored);
                        self.blit();
                    } else {
                        // 走完这一圈后先播转身动画，再回到待机
                        let snap = crate::anim::sched_snapshot(&self.lib);
                        self.exec_plan(crate::anim::plan_walk_end(&snap));
                    }
                }
                true
            }
        };
        if advanced {
            self.walk_step();
            // 闲置达到阈值时隔帧渲染（省电），交互一发生即恢复
            self.render_step = self.render_step.wrapping_add(1);
            let stride = crate::anim::power_save_stride(
                self.last_interaction.elapsed().as_secs(),
                self.power_save_idle_secs,
            );
            if self.render_step.is_multiple_of(u64::from(stride)) {
                self.blit();
            }
        }

        self.hit_poll();
    }

    // ---------- 交互事件 ----------

    pub fn on_window_event(&mut self, event: &WindowEvent) -> Option<WindowRequest> {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.last_interaction = Instant::now();
                let scale = self.window.scale_factor();
                let l: LogicalPosition<f64> = position.to_logical(scale);
                self.cursor_phys = (position.x, position.y);
                self.cursor_local = (l.x, l.y);
                if let Some(drag) = &self.drag
                    && let Some((wx, wy)) = self.window_pos_logical()
                {
                    // 保持 光标全局位置 - 窗口位置 = 抓取偏移：
                    // 新窗口位置 = 当前窗口位置 + (本地坐标 - 抓取本地坐标)
                    let new_pos = (wx + l.x - drag.offset.0, wy + l.y - drag.offset.1);
                    // 速度 EMA（窗口位移 / 时间），松手作惯性初速
                    if let Some((t, prev)) = self.last_drag_move {
                        let dt = Instant::now().duration_since(t).as_secs_f64() * 1000.0;
                        if dt >= 1.0 {
                            let (ix, iy) = ((new_pos.0 - prev.0) / dt, (new_pos.1 - prev.1) / dt);
                            self.velocity = (
                                0.65 * ix + 0.35 * self.velocity.0,
                                0.65 * iy + 0.35 * self.velocity.1,
                            );
                        }
                    }
                    self.last_drag_move = Some((Instant::now(), new_pos));
                    self.window
                        .set_outer_position(LogicalPosition::new(new_pos.0, new_pos.1));
                }
                None
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => {
                self.last_interaction = Instant::now();
                let hit = self.hit_test_at_cursor();
                log::debug!(
                    "MouseInput Pressed {button:?} @ {:?} hit={hit}",
                    self.cursor_local
                );
                if !hit {
                    return None;
                }
                match button {
                    MouseButton::Left => self.on_left_press(),
                    // 物理坐标（菜单弹出示位置）
                    MouseButton::Right => Some(WindowRequest::ShowMenu(
                        self.cursor_phys.0 as i32,
                        self.cursor_phys.1 as i32,
                    )),
                    _ => None,
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => self.on_left_release(),
            WindowEvent::Focused(false) => {
                // 失焦只终止拖拽，不清点击判定（清了会误杀双击）
                if self.drag.take().is_some() {
                    self.window.set_cursor(CursorIcon::Default);
                    self.velocity = (0.0, 0.0);
                    self.enter_idle();
                }
                None
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                self.resize_surface();
                None
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                None
            }
            _ => None,
        }
    }

    /// 左键按下：双击多开判定，否则进入拖拽
    fn on_left_press(&mut self) -> Option<WindowRequest> {
        let drag_allowed =
            !self.lock_position && (!self.shift_drag_only || self.modifiers.shift_key());
        match self
            .input
            .press(Instant::now(), self.cursor_local, drag_allowed)
        {
            Action::DoubleClick => {
                let at = self.spawn_pos_near_cursor();
                let _ = self.proxy.send_event(UserEvent::SpawnPet { at: Some(at) });
            }
            Action::DragStart { offset } => self.begin_drag(offset),
            Action::Click | Action::None => {}
        }
        None
    }

    /// 左键松开：短按且未位移视为单击，否则交给惯性
    fn on_left_release(&mut self) -> Option<WindowRequest> {
        // 无论是否拖拽中都推进判定状态（清双击抑制标志）
        let action = self.input.release(Instant::now(), self.cursor_local);
        if self.drag.take().is_some() {
            self.window.set_cursor(CursorIcon::Default);
            if action == Action::Click {
                self.velocity = (0.0, 0.0); // 单击不走惯性
            }
            // 贴着屏幕左右边缘松手：进入探头姿态，清掉速度不再飞出
            if action != Action::Click
                && let Some((x, _y)) = self.window_pos_logical()
                && let Some(m) = self.window.current_monitor()
            {
                let scale = m.scale_factor();
                let size: LogicalSize<f64> = m.size().to_logical(scale);
                if let Some(angle) = crate::render::probe_angle_at(x, WIN_W, size.width) {
                    self.probe = Some((angle, Instant::now()));
                    self.velocity = (0.0, 0.0);
                    self.blit();
                }
            }
            // 其余情况保持 Mode::Drag，速度交给物理世界飞行，落稳后由物理侧收尾回 Idle
        }
        // 探头中点击一下即拉直
        if action == Action::Click && self.probe.take().is_some() {
            self.blit();
        }
        // 锁定/仅 SHIFT 模式下没有拖拽也照常播点击动画
        if action == Action::Click {
            self.play_oneshot_in("click");
        }
        None
    }

    fn begin_drag(&mut self, offset: (f64, f64)) {
        self.drag = Some(DragState { offset });
        self.velocity = (0.0, 0.0);
        self.last_drag_move = None;
        self.ignoring_cursor = false;
        platform::set_ignore_cursor_events(&self.window, false);
        self.window.set_cursor(CursorIcon::Grabbing);
        if let Some(drag_clip) = random_pick(&self.lib.clips_in_category("drag")).cloned() {
            self.switch_clip(&drag_clip, Mode::Drag, self.anim.mirrored);
            self.blit();
        }
    }

    /// 双击生成新宠物的落点：光标附近错开一点
    fn spawn_pos_near_cursor(&self) -> (f64, f64) {
        let base = self
            .window_pos_logical()
            .map(|(x, y)| (x + self.cursor_local.0, y + self.cursor_local.1));
        match base {
            Some((x, y)) => (x + 36.0, y + 36.0),
            None => (240.0, 160.0),
        }
    }

    /// 拖拽中或物理飞行中（事件循环据此提高唤醒频率）
    pub fn is_interacting(&self) -> bool {
        self.drag.is_some() || matches!(self.anim.mode, Mode::Drag)
    }

    /// 角色切换：替换 `Library` 引用并回到 Idle（旧库的懒加载缓存随引用一起释放）
    pub fn switch_library(&mut self, lib: Arc<Library>) {
        self.lib = lib;
        if let Some(idle) = self.lib.find_idle() {
            let first = self.lib.first_frame(&idle).cloned().expect("idle 首帧缺失");
            let frames = self.lib.get_or_load(&idle, crate::clip_loader(&self.proxy));
            self.anim.enter_idle(&idle, first, frames);
        }
        self.anim.set_event_clips(
            self.lib
                .clips_in_category("events")
                .into_iter()
                .filter_map(|id| self.lib.first_frame(&id).cloned().map(|f| (id, f)))
                .collect(),
        );
        self.blit();
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    /// 位置存档的快照：显示器指纹加显示器内逻辑坐标
    pub fn position_snapshot(&self) -> Option<crate::positions::PetPos> {
        let m = self.window.current_monitor()?;
        let g: LogicalPosition<f64> = self
            .window
            .outer_position()
            .ok()?
            .to_logical(self.window.scale_factor());
        let origin: LogicalPosition<f64> = m.position().to_logical(m.scale_factor());
        Some(crate::positions::PetPos {
            monitor: crate::positions::monitor_fingerprint(
                m.name().as_deref(),
                (m.size().width, m.size().height),
            ),
            x: g.x - origin.x,
            y: g.y - origin.y,
        })
    }

    /// 把窗口挪到指定显示器的存档坐标（等待的显示器上线后调用）
    pub fn move_to_monitor(&self, m: &winit::monitor::MonitorHandle, x: f64, y: f64) {
        let origin: LogicalPosition<f64> = m.position().to_logical(m.scale_factor());
        let size: LogicalSize<f64> = m.size().to_logical(m.scale_factor());
        let (gx, gy) = crate::positions::to_global(
            (origin.x, origin.y),
            (size.width, size.height),
            x,
            y,
            (WIN_W, WIN_H),
        );
        self.window.set_outer_position(LogicalPosition::new(gx, gy));
    }
}

/// 纵向压缩 blit：把整帧压向画面底部（宠物贴地），顶部腾出的行保持透明
fn squash_into(dst: &mut [u8], src: &[u8], scale: f64) {
    const W: usize = FRAME_W as usize;
    const H: usize = FRAME_H as usize;
    let hs = (H as f64 * scale) as usize;
    for y in 0..H {
        let row = y * W * 4;
        if y + hs < H {
            // 压缩后顶部腾出的行保持透明
            dst[row..row + W * 4].fill(0);
        } else {
            let sy = (((y + hs - H) as f64 / scale) as usize).min(H - 1);
            let srow = sy * W * 4;
            dst[row..row + W * 4].copy_from_slice(&src[srow..srow + W * 4]);
        }
    }
}

/// 镜像 blit：逐行水平翻转（透明度随像素翻转，命中测试同步翻转采样）
fn mirror_blit(dst: &mut [u8], src: &[u8]) {
    const W: usize = FRAME_W as usize;
    const H: usize = FRAME_H as usize;
    for y in 0..H {
        let row = y * W * 4;
        for x in 0..W {
            let s = row + x * 4;
            let d = row + (W - 1 - x) * 4;
            dst[d] = src[s];
            dst[d + 1] = src[s + 1];
            dst[d + 2] = src[s + 2];
            dst[d + 3] = src[s + 3];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::FRAME_BYTES;

    #[test]
    fn 镜像二次翻转复原() {
        let src: Vec<u8> = (0..FRAME_BYTES).map(|i| (i % 251) as u8).collect();
        let mut once = vec![0u8; FRAME_BYTES];
        mirror_blit(&mut once, &src);
        let mut twice = vec![0u8; FRAME_BYTES];
        mirror_blit(&mut twice, &once);
        assert_eq!(twice, src);
    }

    #[test]
    fn 镜像交换同行首尾像素且保留alpha() {
        let mut src = vec![0u8; FRAME_BYTES];
        src[0..4].copy_from_slice(&[1, 2, 3, 200]);
        // 第 0 行最后一列（不是整个缓冲区的末尾，那是最后一行的行尾）
        let row0_last = (FRAME_W as usize - 1) * 4;
        src[row0_last..row0_last + 4].copy_from_slice(&[5, 6, 7, 100]);
        // 偶数宽没有不动列：列 159 的内容镜像后落到列 160
        let col = 159;
        src[col * 4..col * 4 + 4].copy_from_slice(&[9, 9, 9, 255]);
        let mut dst = vec![0u8; FRAME_BYTES];
        mirror_blit(&mut dst, &src);
        assert_eq!(&dst[row0_last..row0_last + 4], &[1, 2, 3, 200]);
        assert_eq!(&dst[0..4], &[5, 6, 7, 100]);
        assert_eq!(&dst[(col + 1) * 4..(col + 1) * 4 + 4], &[9, 9, 9, 255]);
    }
}
