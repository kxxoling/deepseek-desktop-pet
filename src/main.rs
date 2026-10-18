//! ds-pet：跨平台桌宠（纯 Rust，winit 0.30 + pixels/wgpu + png 帧序列）
//!
//! 架构：数据（帧库/状态机）与窗口分离，帧库全局共享、多窗引用同一份解码缓存。
//! 详见 AGENTS.md / PROMPT.md。

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod anim;
mod assets;
mod autostart;
mod character;
mod config;
mod input;
mod library;
mod logging;
mod physics;
mod platform;
mod render;
mod tray;
mod window;

use std::sync::Arc;
use std::time::{Duration, Instant};

use library::{ClipFrames, ClipId, Library};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

use crate::window::{MAX_PETS, PetWindow, WindowRequest};

/// 事件循环自定义事件（proxy 转发：后台解码线程 / 宠物窗口上抛）
pub enum UserEvent {
    ClipLoaded(ClipId, Arc<ClipFrames>),
    SpawnPet {
        at: Option<(f64, f64)>,
    },
    /// 触发播放特殊片段，参数为类名或「类/子分类」
    TriggerClip(String),
    ExitApp,
}

struct App {
    lib: Arc<Library>,
    gpu: Arc<render::Gpu>,
    proxy: EventLoopProxy<UserEvent>,
    pets: Vec<PetWindow>,
    /// 碰撞世界（`pets` 与 `physics.pets` 按下标一一对应）
    physics: physics::World,
    last_phys_at: Instant,
    // muda::Menu 非 Send/Sync，用 Rc 即可（仅主线程使用）
    ctx_menu: std::rc::Rc<muda::Menu>,
    #[allow(dead_code)] // 托盘句柄需保活
    tray: Option<tray_icon::TrayIcon>,
    /// 右键菜单弹出的目标窗口（「关闭这只」用）
    menu_target: Option<WindowId>,
    spawned_initial: bool,
    /// idle 片段 id（ClipLoaded 到达时比对）
    idle_clip: Option<ClipId>,
    /// idle 帧集常驻（持强引用防驱逐，避免每次回待机重新解码）
    idle_frames: Option<Arc<ClipFrames>>,
}

/// 常规唤醒周期：穿透轮询 + 菜单事件兜底（30Hz）
const POLL_TICK: Duration = Duration::from_millis(33);
/// 高频唤醒：拖拽 / 惯性滑行期间（60Hz）
const FAST_TICK: Duration = Duration::from_millis(16);

/// 菜单项 ID
const MENU_NEW: &str = "app.new";
const MENU_CLOSE: &str = "pet.close";
const MENU_LOGS: &str = "app.openlogs";
const MENU_EXIT: &str = "app.exit";
const MENU_AUTOSTART: &str = "app.autostart";
/// 触发片段的菜单项 ID 前缀，后面接类名或「类/子分类」
const MENU_ANIM_PREFIX: &str = "app.anim.";

/// 菜单项定义（纯数据，加菜单项就是往表里加一行）；muda 菜单构建必须在主线程
#[allow(dead_code)]
enum MenuItemDef {
    /// 普通项
    Item { id: String, label: String },
    /// 勾选项（设置开关用）
    Check {
        id: String,
        label: String,
        checked: bool,
    },
    /// 子菜单（树）
    Submenu {
        label: String,
        children: Vec<MenuItemDef>,
    },
}

fn item(id: &str, label: &str) -> MenuItemDef {
    MenuItemDef::Item {
        id: id.to_string(),
        label: label.to_string(),
    }
}

fn check(id: &str, label: &str, checked: bool) -> MenuItemDef {
    MenuItemDef::Check {
        id: id.to_string(),
        label: label.to_string(),
        checked,
    }
}

/// 右键菜单；events 类有子分类时，追加「动画」子菜单逐项触发
fn context_menu_defs(subs: &[String], autostart_on: bool) -> Vec<MenuItemDef> {
    let mut defs = base_context_defs(autostart_on);
    if !subs.is_empty() {
        let children = subs
            .iter()
            .map(|s| item(&format!("{MENU_ANIM_PREFIX}events/{s}"), s))
            .collect();
        defs.insert(
            2,
            MenuItemDef::Submenu {
                label: "动画".to_string(),
                children,
            },
        );
    }
    defs
}

fn base_context_defs(autostart_on: bool) -> Vec<MenuItemDef> {
    vec![
        item(MENU_NEW, "新增一只"),
        item(MENU_CLOSE, "关闭这只"),
        item(MENU_LOGS, "打开日志文件夹"),
        check(MENU_AUTOSTART, "开机自启", autostart_on),
        item(MENU_EXIT, "退出"),
    ]
}

/// 托盘菜单（无「关闭这只」：托盘不指向具体窗口）
fn tray_menu_defs(autostart_on: bool) -> Vec<MenuItemDef> {
    vec![
        item(MENU_NEW, "新增一只"),
        item(MENU_LOGS, "打开日志文件夹"),
        check(MENU_AUTOSTART, "开机自启", autostart_on),
        item(MENU_EXIT, "退出"),
    ]
}

/// events 类下的子分类名，去重排序；直接放在 events/ 下的片段不算子分类
fn events_subcategories(lib: &Library) -> Vec<String> {
    events_subcategories_of(&lib.clips_in_category("events"))
}

fn events_subcategories_of(ids: &[ClipId]) -> Vec<String> {
    let mut subs: Vec<String> = ids
        .iter()
        .filter_map(|id| {
            let segs: Vec<&str> = id.0.split('/').collect();
            (segs.len() >= 3).then(|| segs[1].to_string())
        })
        .collect();
    subs.sort();
    subs.dedup();
    subs
}

fn menu_item(id: &str, label: &str) -> muda::MenuItem {
    muda::MenuItem::with_id(muda::MenuId::new(id), label, true, None)
}

/// Menu 与 Submenu 的 append 收口，递归构建子菜单用
trait AppendItem {
    fn append_item(&self, item: &dyn muda::IsMenuItem) -> muda::Result<()>;
}

impl AppendItem for muda::Menu {
    fn append_item(&self, item: &dyn muda::IsMenuItem) -> muda::Result<()> {
        self.append(item)
    }
}

impl AppendItem for muda::Submenu {
    fn append_item(&self, item: &dyn muda::IsMenuItem) -> muda::Result<()> {
        self.append(item)
    }
}

/// 按注册表递归构建菜单
fn append_defs(menu: &impl AppendItem, defs: &[MenuItemDef]) -> muda::Result<()> {
    for def in defs {
        match def {
            MenuItemDef::Item { id, label } => {
                menu.append_item(&menu_item(id, label))?;
            }
            MenuItemDef::Check { id, label, checked } => {
                menu.append_item(&muda::CheckMenuItem::with_id(
                    muda::MenuId::new(id),
                    label,
                    *checked,
                    true,
                    None,
                ))?;
            }
            MenuItemDef::Submenu { label, children } => {
                let sub = muda::Submenu::with_id(
                    muda::MenuId::new(format!("submenu.{label}")),
                    label,
                    true,
                );
                append_defs(&sub, children)?;
                menu.append_item(&sub)?;
            }
        }
    }
    Ok(())
}

fn build_menu(defs: &[MenuItemDef]) -> muda::Result<muda::Menu> {
    let menu = muda::Menu::new();
    append_defs(&menu, defs)?;
    Ok(menu)
}

fn build_context_menu(lib: &Library, autostart_on: bool) -> muda::Result<muda::Menu> {
    build_menu(&context_menu_defs(&events_subcategories(lib), autostart_on))
}

fn build_tray_menu(autostart_on: bool) -> muda::Result<muda::Menu> {
    build_menu(&tray_menu_defs(autostart_on))
}

fn build_tray(
    menu: muda::Menu,
    icon: tray_icon::Icon,
) -> Result<tray_icon::TrayIcon, tray_icon::Error> {
    tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("桌宠（双击托盘新增一只）")
        .with_icon(icon)
        .build()
}

/// 在系统文件管理器中打开目录
fn open_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    // explorer 成功时也可能返回非零，忽略退出码
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    let _ = cmd.arg(path).spawn();
}

fn clip_loader(
    proxy: &EventLoopProxy<UserEvent>,
) -> impl Fn(ClipId, Arc<ClipFrames>) + Send + 'static {
    let proxy = proxy.clone();
    move |cid, frames| {
        let _ = proxy.send_event(UserEvent::ClipLoaded(cid, frames));
    }
}

fn parse_pos_env() -> Option<(f64, f64)> {
    let v = std::env::var("DS_PET_POS").ok()?;
    let (x, y) = v.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn default_spawn_pos(event_loop: &ActiveEventLoop) -> (f64, f64) {
    let mon = event_loop
        .primary_monitor()
        .or_else(|| event_loop.available_monitors().next());
    match mon {
        Some(m) => {
            let size: winit::dpi::LogicalSize<f64> = m.size().to_logical(m.scale_factor());
            let x = anim::rand_range(0.08, 0.88) * (size.width - window::WIN_W).max(0.0);
            let y = (size.height - window::WIN_H - 90.0).max(10.0); // 菜单栏/程序坞上方
            (x, y)
        }
        None => (240.0, 160.0),
    }
}

/// 物理世界的地面边界取主屏逻辑尺寸（拿不到显示器时给一个常见值兜底）
fn monitor_bounds(mon: Option<winit::monitor::MonitorHandle>) -> physics::Bounds {
    match mon {
        Some(m) => {
            let size: winit::dpi::LogicalSize<f64> = m.size().to_logical(m.scale_factor());
            physics::Bounds {
                width: size.width,
                height: size.height,
            }
        }
        None => physics::Bounds {
            width: 1512.0,
            height: 982.0,
        },
    }
}

impl App {
    fn spawn_pet(&mut self, event_loop: &ActiveEventLoop, at: Option<(f64, f64)>) {
        if self.pets.len() >= MAX_PETS {
            log::warn!("窗口数量已达上限 {MAX_PETS}，忽略新增请求");
            return;
        }
        let pos = at.or_else(|| {
            parse_pos_env()
                .filter(|_| self.pets.is_empty())
                .or_else(|| Some(default_spawn_pos(event_loop)))
        });
        // 多开随机起始相位
        let offset = (anim::rand_u64() % 150) as usize;
        match PetWindow::new(
            event_loop,
            self.lib.clone(),
            self.proxy.clone(),
            self.gpu.clone(),
            pos,
            offset,
        ) {
            Ok(pet) => {
                log::info!(
                    "新增宠物窗口 {:?}（共 {} 只）",
                    pet.id(),
                    self.pets.len() + 1
                );
                let body_pos = pet.window_pos_logical().unwrap_or((240.0, 160.0));
                self.physics.pets.push(physics::PetBody::new(body_pos));
                self.pets.push(pet);
            }
            Err(e) => log::error!("创建宠物窗口失败：{e}"),
        }
    }

    fn remove_pet(&mut self, id: WindowId) {
        // CloseRequested 与随后窗口销毁的 Destroyed 会各调一次，找不到时静默跳过
        if let Some(pos) = self.pets.iter().position(|p| p.id() == id) {
            self.pets.remove(pos);
            self.physics.pets.remove(pos);
            log::info!("关闭宠物窗口（剩余 {} 只）", self.pets.len());
            if self.pets.is_empty() {
                log::info!("全部窗口已关闭，托盘保活中（托盘「新增一只」可召回）");
            }
        }
    }

    /// 切换开机自启并重建两处菜单刷新勾选态
    fn toggle_autostart(&mut self) {
        let ok = if autostart::is_enabled() {
            autostart::disable()
        } else {
            autostart::enable()
        };
        if !ok {
            log::error!("切换开机自启失败");
            return;
        }
        let on = autostart::is_enabled();
        if let Ok(menu) = build_context_menu(&self.lib, on) {
            self.ctx_menu = std::rc::Rc::new(menu);
        }
        if let Some(tray) = &self.tray
            && let Ok(menu) = build_tray_menu(on)
        {
            tray.set_menu(Some(Box::new(menu)));
        }
    }

    fn handle_menu_id(&mut self, event_loop: &ActiveEventLoop, id: &str) {
        log::debug!("MenuEvent 收到：{id}");
        match id {
            MENU_NEW => self.spawn_pet(event_loop, None),
            MENU_CLOSE => {
                if let Some(target) = self.menu_target.take() {
                    self.remove_pet(target);
                }
            }
            MENU_LOGS => open_in_file_manager(&logging::log_dir()),
            MENU_AUTOSTART => self.toggle_autostart(),
            MENU_EXIT => event_loop.exit(),
            _ if let Some(spec) = id.strip_prefix(MENU_ANIM_PREFIX) => {
                let _ = self
                    .proxy
                    .send_event(UserEvent::TriggerClip(spec.to_string()));
            }
            _ => log::debug!("未知菜单项：{id}"),
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.spawned_initial {
            self.spawned_initial = true;
            self.physics.bounds = monitor_bounds(
                event_loop
                    .primary_monitor()
                    .or_else(|| event_loop.available_monitors().next()),
            );
            self.spawn_pet(event_loop, None);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::ClipLoaded(id, frames) => {
                self.lib.install_loaded(&id, &frames);
                if self.idle_clip.as_ref() == Some(&id) && self.idle_frames.is_none() {
                    self.idle_frames = Some(frames.clone());
                }
                for pet in &mut self.pets {
                    if pet.anim.on_clip_loaded(&id, frames.clone()) {
                        log::debug!("clip {id} 就绪，开始播放");
                    }
                }
            }
            UserEvent::SpawnPet { at } => self.spawn_pet(event_loop, at),
            UserEvent::TriggerClip(spec) => {
                // 右键菜单在具体宠物上弹出，触发也落在它身上
                if let Some(target) = self.menu_target
                    && let Some(pet) = self.pets.iter_mut().find(|p| p.id() == target)
                    && !pet.trigger_clip(&spec)
                {
                    log::debug!("触发被拒绝：{spec}");
                }
            }
            UserEvent::ExitApp => event_loop.exit(),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                self.remove_pet(window_id);
            }
            WindowEvent::RedrawRequested => {
                if let Some(pet) = self.pets.iter_mut().find(|p| p.id() == window_id) {
                    pet.render();
                }
            }
            _ => {
                let Some(pos) = self.pets.iter().position(|p| p.id() == window_id) else {
                    return;
                };
                if let Some(WindowRequest::ShowMenu(x, y)) = self.pets[pos].on_window_event(&event)
                {
                    self.menu_target = Some(window_id);
                    platform::show_context_menu(&self.ctx_menu, &self.pets[pos].window, x, y);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();

        // 托盘菜单 / 右键菜单事件（muda 与 tray-icon 的全局 receiver，轮询转发）
        while let Ok(ev) = muda::MenuEvent::receiver().try_recv() {
            self.handle_menu_id(event_loop, &ev.id.0);
        }
        while let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
            if matches!(
                ev,
                tray_icon::TrayIconEvent::DoubleClick { .. }
                    | tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        ..
                    }
            ) {
                self.spawn_pet(event_loop, None);
            }
        }

        // 唤醒对齐下一动画帧时刻；拖拽/惯性期间提高轮询频率
        let mut deadline = now + POLL_TICK;
        for pet in &mut self.pets {
            pet.tick(now);
            if let Some(nf) = pet.anim.next_frame_at() {
                deadline = deadline.min(nf);
            }
            if pet.is_interacting() {
                deadline = deadline.min(now + FAST_TICK);
            }
        }

        self.step_physics(now);

        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
    }
}

impl App {
    /// 窗口状态进物理世界、解算、结果写回（与宠物列表同下标对齐）
    fn step_physics(&mut self, now: Instant) {
        let dt = now
            .duration_since(self.last_phys_at)
            .as_secs_f64()
            .min(0.05);
        self.last_phys_at = now;
        for (pet, body) in self.pets.iter_mut().zip(self.physics.pets.iter_mut()) {
            pet.sync_body_out(body);
        }
        self.physics.step(dt);
        for (pet, body) in self.pets.iter_mut().zip(self.physics.pets.iter_mut()) {
            pet.apply_body(body);
        }
    }
}

/// 构建标识：版本 + profile + commit 短哈希（哈希由 build.rs 从 git 注入）
pub fn build_info() -> String {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "v{} {} ({})",
        env!("CARGO_PKG_VERSION"),
        profile,
        env!("DS_PET_BUILD_COMMIT")
    )
}

fn main() {
    match logging::init() {
        Some(path) => log::info!("日志文件：{}", path.display()),
        None => eprintln!("日志初始化失败，仅输出到 stderr"),
    }
    // 启动时就加载配置，坏文件尽早暴露
    log::info!("配置文件：{}", config::init().display());

    let root = assets::locate_assets_root();
    let lib = match Library::load(&root) {
        Ok(l) => Arc::new(l),
        Err(e) => {
            eprintln!("资产加载失败：{e}\n先运行 scripts/preprocess_assets.sh 生成帧资产");
            std::process::exit(1);
        }
    };

    #[cfg(target_os = "macos")]
    let event_loop = {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        EventLoop::<UserEvent>::with_user_event()
            .with_activation_policy(ActivationPolicy::Accessory)
            .build()
            .expect("事件循环创建失败")
    };
    #[cfg(not(target_os = "macos"))]
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("事件循环创建失败");

    let proxy = event_loop.create_proxy();

    autostart::cleanup_stale();
    let ctx_menu = std::rc::Rc::new(
        build_context_menu(&lib, autostart::is_enabled()).expect("右键菜单创建失败"),
    );
    let tray_menu = build_tray_menu(autostart::is_enabled()).expect("托盘菜单创建失败");
    let icon = lib
        .find_idle()
        .and_then(|id| lib.first_frame(&id))
        .map(|f| tray::tray_icon_from_frame(&f.0));
    let tray = if let Some(icon) = icon {
        build_tray(tray_menu, icon)
            .inspect_err(|e| log::error!("托盘创建失败：{e}"))
            .ok()
    } else {
        log::error!("托盘图标生成失败");
        None
    };

    // idle 帧集常驻（持强引用防缓存驱逐）
    let idle_clip = lib.find_idle();
    let idle_frames = idle_clip
        .as_ref()
        .and_then(|id| lib.get_or_load(id, clip_loader(&proxy)));

    let gpu = Arc::new(
        render::Gpu::new(event_loop.owned_display_handle()).expect("GPU 上下文初始化失败"),
    );
    let mut app = App {
        lib,
        gpu,
        proxy,
        pets: Vec::new(),
        // 边界依赖显示器信息，resumed 首次回调时补上真实值
        physics: physics::World::new(physics::Bounds {
            width: 0.0,
            height: 0.0,
        }),
        last_phys_at: Instant::now(),
        ctx_menu,
        tray,
        menu_target: None,
        spawned_initial: false,
        idle_clip,
        idle_frames,
    };

    log::info!("桌宠启动（{} 个 clip 就绪）", app.lib.clips().len());
    if let Err(e) = event_loop.run_app(&mut app) {
        log::error!("事件循环异常退出：{e:?}");
        std::process::exit(1);
    }
    log::info!("桌宠退出");
}

#[cfg(test)]
#[allow(unsafe_code)] // 测试内修改进程环境变量（edition 2024 要求 unsafe 块）
mod tests {
    use super::*;

    /// 深度收集注册表里的全部菜单项 id（含子菜单）
    fn collect_ids<'a>(defs: &'a [MenuItemDef], ids: &mut Vec<&'a str>) {
        for def in defs {
            match def {
                MenuItemDef::Item { id, .. } | MenuItemDef::Check { id, .. } => ids.push(id),
                MenuItemDef::Submenu { children, .. } => collect_ids(children, ids),
            }
        }
    }

    #[test]
    fn menu_ids_unique_and_nonempty() {
        for defs in [base_context_defs(true), tray_menu_defs(false)] {
            let mut ids = Vec::new();
            collect_ids(&defs, &mut ids);
            assert_ne!(ids, [] as [&'static str; 0]);
            assert!(ids.iter().all(|i| !i.is_empty()), "id 不能为空：{ids:?}");
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "表内 id 必须唯一：{ids:?}");
        }
    }

    #[test]
    fn menus_match_previous_structure() {
        let ctx_defs = base_context_defs(false);
        let mut ctx = Vec::new();
        collect_ids(&ctx_defs, &mut ctx);
        let tray_defs = tray_menu_defs(false);
        let mut tray = Vec::new();
        collect_ids(&tray_defs, &mut tray);
        // 右键五项、托盘四项（各含「开机自启」勾选项）；「关闭这只」只在右键
        assert_eq!(ctx.len(), 5);
        assert_eq!(tray.len(), 4);
        assert!(ctx.contains(&MENU_CLOSE));
        assert!(!tray.contains(&MENU_CLOSE));
        for id in [MENU_NEW, MENU_LOGS, MENU_EXIT] {
            assert!(ctx.contains(&id) && tray.contains(&id), "公共项缺失：{id}");
        }
    }

    #[test]
    fn submenu_and_check_defs_traversable() {
        let nested = vec![MenuItemDef::Submenu {
            label: "设置".to_string(),
            children: vec![
                MenuItemDef::Check {
                    id: "app.check".to_string(),
                    label: "开关".to_string(),
                    checked: true,
                },
                MenuItemDef::Item {
                    id: "app.sub".to_string(),
                    label: "子项".to_string(),
                },
            ],
        }];
        let mut ids = Vec::new();
        collect_ids(&nested, &mut ids);
        assert_eq!(ids, vec!["app.check", "app.sub"]);
    }

    #[test]
    fn 构建标识包含版本与哈希() {
        let info = build_info();
        assert!(info.starts_with('v'), "以 v 开头：{info}");
        assert!(
            info.contains("release") || info.contains("debug"),
            "包含构建 profile：{info}"
        );
        assert!(info.ends_with(')'), "以哈希括号结尾：{info}");
        let hash = info
            .trim_start_matches('v')
            .rsplit(" (")
            .next()
            .unwrap()
            .trim_end_matches(')');
        assert!(
            hash == "unknown" || hash.chars().all(|c| c.is_ascii_hexdigit()),
            "哈希合法：{info}"
        );
    }

    #[test]
    fn 位置环境变量解析() {
        unsafe {
            std::env::set_var("DS_PET_POS", "300,400.5");
        }
        assert_eq!(parse_pos_env(), Some((300.0, 400.5)));
        unsafe {
            std::env::set_var("DS_PET_POS", "300");
        }
        assert_eq!(parse_pos_env(), None); // 缺少 y
        unsafe {
            std::env::set_var("DS_PET_POS", "a,b");
        }
        assert_eq!(parse_pos_env(), None); // 非数字
        unsafe {
            std::env::remove_var("DS_PET_POS");
        }
        assert_eq!(parse_pos_env(), None);
    }

    #[test]
    fn events_subcategories_dedup_and_sort() {
        let ids = [
            ClipId(std::sync::Arc::from("events/balance/余额-分文不剩")),
            ClipId(std::sync::Arc::from("events/balance/余额-数金皱眉")),
            ClipId(std::sync::Arc::from("events/喝水")),
        ];
        // 直接放在 events/ 下的「喝水」不算子分类
        assert_eq!(events_subcategories_of(&ids), vec!["balance".to_string()]);
        assert_eq!(events_subcategories_of(&[]), Vec::<String>::new());
    }

    #[test]
    fn anim_submenu_appends_and_prefixes() {
        let defs = context_menu_defs(&["balance".to_string()], false);
        // 基础五项之上插入「动画」子菜单
        assert_eq!(defs.len(), 6);
        let MenuItemDef::Submenu { label, children } = &defs[2] else {
            panic!("第三项应为动画子菜单");
        };
        assert_eq!(label, "动画");
        let MenuItemDef::Item { id, label } = &children[0] else {
            panic!("动画子菜单的子项应为普通项");
        };
        assert_eq!(id, "app.anim.events/balance");
        assert_eq!(label, "balance");
        // 没有子分类时不追加
        assert_eq!(context_menu_defs(&[], false).len(), 5);
    }
}
