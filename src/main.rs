//! ds-pet：跨平台桌宠（纯 Rust，winit 0.30 + pixels/wgpu + png 帧序列）
//!
//! 架构：数据（`Library`/状态机）与窗口分离，`Library` 全局共享、多窗引用同一份解码缓存。
//! 详见 AGENTS.md / PROMPT.md。

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod anim;
mod assets;
mod audio;
mod autostart;
mod bubble;
mod bubble_window;
mod character;
mod config;
mod input;
mod library;
mod logging;
mod physics;
mod platform;
mod positions;
mod render;
mod selftalk;
mod text;
mod tray;
mod tts;
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
    /// 对右键菜单目标宠物触发黄金回旋
    SpinPet,
    /// 宠物说了一句话；有对应动画的同时播放
    SelfTalk {
        pet_id: WindowId,
        line: String,
        clip_name: Option<String>,
    },
    ExitApp,
}

/// 等待目标显示器上线的恢复任务
struct PendingRestore {
    pet: WindowId,
    pos: positions::PetPos,
}

struct App {
    lib: Arc<Library>,
    /// 可用角色（内置 + 外部目录）
    characters: Vec<CharacterEntry>,
    /// 当前角色 id（config `character`，缺省内置）
    current_character: String,
    /// 全部桌宠当前是否隐藏；隐藏时不播动画、不算物理
    pets_hidden: bool,
    /// 全屏自动隐藏的上次检测时刻（2 秒轮询一次）
    fullscreen_check_at: Instant,
    /// 当前显示的气泡窗口（一次最多一个，新的替换旧的）
    bubble: Option<crate::bubble_window::BubbleWindow>,
    gpu: Arc<render::Gpu>,
    proxy: EventLoopProxy<UserEvent>,
    pets: Vec<PetWindow>,
    /// 碰撞世界（`pets` 与 `physics.pets` 按下标一一对应）
    physics: physics::World,
    last_phys_at: Instant,
    /// 启动时待恢复的存档位置，按顺序取用
    restore_queue: std::collections::VecDeque<positions::PetPos>,
    /// 目标显示器还没上线的宠物；它们暂时落在主屏，临时坐标不写入存档
    pending_restore: Vec<PendingRestore>,
    /// 上次写入存档的序列化结果；内容没变就不重写
    last_saved_positions: String,
    last_pos_check: Instant,
    // muda::Menu 非 Send/Sync，用 Rc 即可（仅主线程使用）
    ctx_menu: std::rc::Rc<muda::Menu>,
    #[allow(dead_code)] // 托盘句柄需保活
    tray: Option<tray_icon::TrayIcon>,
    /// 右键菜单弹出的目标窗口（「关闭这只」用）
    menu_target: Option<WindowId>,
    spawned_initial: bool,
    /// idle 片段 id（ClipLoaded 到达时比对）
    idle_clip: Option<ClipId>,
    /// 常驻的 idle `ClipFrames`（持强引用防驱逐，避免每次回待机重新解码）
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
/// 角色切换菜单项 ID 前缀，后面接角色 id
const MENU_CHAR_PREFIX: &str = "app.char.";
/// 黄金回旋菜单项
const MENU_SPIN: &str = "app.spin";
/// 设置菜单项 ID 前缀；后接「设置名.值」（值为整数百分比，避免小数点歧义）
const MENU_SET_PREFIX: &str = "app.set.";
/// 隐藏/显示桌宠菜单项
const MENU_HIDE: &str = "app.hide";

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
fn context_menu_defs(
    subs: &[String],
    autostart_on: bool,
    chars: &[CharacterEntry],
    current: &str,
    settings: &SettingsSnapshot,
) -> Vec<MenuItemDef> {
    let mut defs = base_context_defs(autostart_on, settings);
    append_character_submenu(&mut defs, chars, current);
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

fn base_context_defs(autostart_on: bool, settings: &SettingsSnapshot) -> Vec<MenuItemDef> {
    vec![
        item(MENU_NEW, "新增一只"),
        item(MENU_HIDE, "隐藏桌宠"),
        item(MENU_CLOSE, "关闭这只"),
        item(MENU_SPIN, "黄金回旋"),
        item(MENU_LOGS, "打开日志文件夹"),
        check(MENU_AUTOSTART, "开机自启", autostart_on),
        settings_submenu(settings),
        item(MENU_EXIT, "退出"),
    ]
}

/// 设置菜单需要的配置快照（构造保持纯函数，便于单测）
struct SettingsSnapshot {
    opacity: f64,
    speed: f64,
    lock: bool,
    shift_only: bool,
    power_on: bool,
}

impl SettingsSnapshot {
    /// 从 config 读取当前值
    fn from_config() -> Self {
        Self {
            opacity: config::get_f64("opacity").map_or(1.0, config::clamp_opacity),
            speed: config::get_f64("playback_speed").unwrap_or(1.0),
            lock: config::get_bool("lock_position").unwrap_or(false),
            shift_only: config::get_bool("shift_drag_only").unwrap_or(false),
            power_on: config::get_i64("power_saving_idle_secs").unwrap_or(0) > 0,
        }
    }
}

/// 不透明度档位（步进 10%）
const OPACITY_LEVELS: [u32; 6] = [100, 90, 80, 70, 60, 50];
/// 播放速率档位（步进 0.25）
const SPEED_LEVELS: [u32; 5] = [100, 125, 150, 175, 200];
/// 省电开启时的闲置秒数（关闭为 0）
const POWER_ON_SECS: i64 = 30;

/// 「设置」子菜单：不透明度与播放速率按档位选取（当前档带「✓」前缀），
/// 三个开关用勾选项；改动即时生效，无需重启
fn settings_submenu(snap: &SettingsSnapshot) -> MenuItemDef {
    MenuItemDef::Submenu {
        label: "设置".to_string(),
        children: vec![
            MenuItemDef::Submenu {
                label: "不透明度".to_string(),
                children: OPACITY_LEVELS
                    .iter()
                    .map(|lv| {
                        let label = if (f64::from(*lv) / 100.0 - snap.opacity).abs() < 1e-9 {
                            format!("✓ {lv}%")
                        } else {
                            format!("{lv}%")
                        };
                        item(&format!("{MENU_SET_PREFIX}opacity.{lv}"), &label)
                    })
                    .collect(),
            },
            MenuItemDef::Submenu {
                label: "播放速率".to_string(),
                children: SPEED_LEVELS
                    .iter()
                    .map(|lv| {
                        let label = if (f64::from(*lv) / 100.0 - snap.speed).abs() < 1e-9 {
                            format!("✓ {lv}%")
                        } else {
                            format!("{lv}%")
                        };
                        item(&format!("{MENU_SET_PREFIX}speed.{lv}"), &label)
                    })
                    .collect(),
            },
            check(&format!("{MENU_SET_PREFIX}lock"), "锁定位置", snap.lock),
            check(
                &format!("{MENU_SET_PREFIX}shift"),
                "仅限 SHIFT 拖动",
                snap.shift_only,
            ),
            check(
                &format!("{MENU_SET_PREFIX}power"),
                "省电模式",
                snap.power_on,
            ),
        ],
    }
}

/// 托盘菜单（无「关闭这只」：托盘不指向具体窗口）
fn tray_menu_defs(
    autostart_on: bool,
    chars: &[CharacterEntry],
    current: &str,
    settings: &SettingsSnapshot,
) -> Vec<MenuItemDef> {
    let mut defs = vec![
        item(MENU_NEW, "新增一只"),
        item(MENU_LOGS, "打开日志文件夹"),
        check(MENU_AUTOSTART, "开机自启", autostart_on),
        settings_submenu(settings),
        item(MENU_EXIT, "退出"),
    ];
    append_character_submenu(&mut defs, chars, current);
    defs
}

/// 菜单里使用的角色条目：把角色发现的结果整理成纯数据，让菜单构建保持无副作用
struct CharacterEntry {
    id: String,
    display_name: String,
}

/// 有多个角色时在「退出」前插入「角色」子菜单，当前角色带「✓」前缀
fn append_character_submenu(defs: &mut Vec<MenuItemDef>, chars: &[CharacterEntry], current: &str) {
    if chars.len() < 2 {
        return;
    }
    let children = chars
        .iter()
        .map(|c| {
            // 当前角色用「✓」前缀标记（勾选组件的点击事件在弹出菜单里不可靠，改用普通项）
            let label = if c.id == current {
                format!("✓ {}", c.display_name)
            } else {
                c.display_name.clone()
            };
            item(&format!("{MENU_CHAR_PREFIX}{}", c.id), &label)
        })
        .collect::<Vec<_>>();
    defs.insert(
        defs.len() - 1,
        MenuItemDef::Submenu {
            label: "角色".to_string(),
            children,
        },
    );
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

fn build_context_menu(
    lib: &Library,
    autostart_on: bool,
    chars: &[CharacterEntry],
    current: &str,
    settings: &SettingsSnapshot,
) -> muda::Result<muda::Menu> {
    build_menu(&context_menu_defs(
        &events_subcategories(lib),
        autostart_on,
        chars,
        current,
        settings,
    ))
}

fn build_tray_menu(
    autostart_on: bool,
    chars: &[CharacterEntry],
    current: &str,
    settings: &SettingsSnapshot,
) -> muda::Result<muda::Menu> {
    build_menu(&tray_menu_defs(autostart_on, chars, current, settings))
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

/// 把存档位置换算成全局逻辑坐标：显示器在线就用它的原点，离线时先落主屏并标记等待。
/// 返回 (全局坐标, 是否在等待目标显示器)
fn restore_to_global(
    event_loop: &ActiveEventLoop,
    saved: &positions::PetPos,
) -> ((f64, f64), bool) {
    let monitors: Vec<_> = event_loop.available_monitors().collect();
    let online: Vec<String> = monitors
        .iter()
        .map(|m| {
            positions::monitor_fingerprint(m.name().as_deref(), (m.size().width, m.size().height))
        })
        .collect();
    let primary = event_loop
        .primary_monitor()
        .and_then(|p| online.iter().position(|f| *f == fingerprint_of(&p)))
        .unwrap_or(0);
    let (idx, waiting) = positions::resolve(saved, &online, primary);
    match monitors.get(idx) {
        Some(m) => (monitor_relative_to_global(m, saved.x, saved.y), waiting),
        None => ((240.0, 160.0), waiting),
    }
}

/// 计算 `MonitorHandle` 的显示器指纹
fn fingerprint_of(m: &winit::monitor::MonitorHandle) -> String {
    positions::monitor_fingerprint(m.name().as_deref(), (m.size().width, m.size().height))
}

/// 把显示器内坐标换算成全局逻辑坐标（并夹在该显示器范围内）
fn monitor_relative_to_global(m: &winit::monitor::MonitorHandle, x: f64, y: f64) -> (f64, f64) {
    let scale = m.scale_factor();
    let origin: winit::dpi::LogicalPosition<f64> = m.position().to_logical(scale);
    let size: winit::dpi::LogicalSize<f64> = m.size().to_logical(scale);
    positions::to_global(
        (origin.x, origin.y),
        (size.width, size.height),
        x,
        y,
        (window::WIN_W, window::WIN_H),
    )
}

impl App {
    /// 切换角色：重新加载该角色的 `Library` 并对所有窗口生效，同时更新托盘图标和菜单里的当前角色标记，并记住这次的选择
    fn switch_character(&mut self, char_id: &str) {
        if char_id == self.current_character {
            return;
        }
        if !self.characters.iter().any(|c| c.id == char_id) {
            log::warn!("未知角色：{char_id}");
            return;
        }
        let root = entry_root(char_id).unwrap_or_else(assets::locate_assets_root);
        let new_lib = match Library::load(&root) {
            Ok(l) => Arc::new(l),
            Err(e) => {
                log::error!("角色资产加载失败（{char_id}）：{e}");
                return;
            }
        };

        // 常驻的 idle `ClipFrames`（持引用防缓存驱逐）随角色切换重新绑定到新角色
        let idle_clip = new_lib.find_idle();
        let idle_frames = idle_clip
            .as_ref()
            .and_then(|id| new_lib.get_or_load(id, clip_loader(&self.proxy)));
        self.idle_clip = idle_clip;
        self.idle_frames = idle_frames;

        self.current_character = char_id.to_string();
        config::set("character", char_id);
        let lib = new_lib.clone();
        for pet in &mut self.pets {
            pet.switch_library(lib.clone());
        }
        self.lib = lib;

        // 托盘图标换成新角色 idle 首帧
        if let Some(tray) = &self.tray
            && let Some(idle) = self.lib.find_idle()
            && let Some(frame) = self.lib.first_frame(&idle)
        {
            let icon = tray::tray_icon_from_frame(&frame.0);
            let _ = tray.set_icon(Some(icon));
        }
        // 重建两处菜单，刷新里面的当前角色标记
        let autostart_on = autostart::is_enabled();
        if let Ok(menu) = build_context_menu(
            &self.lib,
            autostart_on,
            &self.characters,
            &self.current_character,
            &SettingsSnapshot::from_config(),
        ) {
            self.ctx_menu = std::rc::Rc::new(menu);
        }
        if let Some(tray) = &self.tray
            && let Ok(menu) = build_tray_menu(
                autostart_on,
                &self.characters,
                &self.current_character,
                &SettingsSnapshot::from_config(),
            )
        {
            tray.set_menu(Some(Box::new(menu)));
        }
        log::info!("已切换角色：{char_id}");
    }

    /// 把全部桌宠显示或隐藏。隐藏时不播动画、不算物理，恢复显示后动画重新播放，不会快进
    fn toggle_pets_visible(&mut self) {
        self.pets_hidden = !self.pets_hidden;
        for pet in &mut self.pets {
            pet.set_visible(!self.pets_hidden);
        }
        log::info!(
            "{}",
            if self.pets_hidden {
                "桌宠已隐藏"
            } else {
                "桌宠已显示"
            }
        );
    }

    fn spawn_pet(&mut self, event_loop: &ActiveEventLoop, at: Option<(f64, f64)>) {
        if self.pets.len() >= MAX_PETS {
            log::warn!("窗口数量已达上限 {MAX_PETS}，忽略新增请求");
            return;
        }
        // 位置的优先级：先显式指定，其次首窗的环境变量，再是存档恢复，最后随机落位
        let mut deferred = None;
        let pos = at.or_else(|| {
            parse_pos_env()
                .filter(|_| self.pets.is_empty())
                .or_else(|| {
                    self.restore_queue.pop_front().map(|saved| {
                        let (global, waiting) = restore_to_global(event_loop, &saved);
                        if waiting {
                            deferred = Some(saved);
                        }
                        global
                    })
                })
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
                if let Some(saved) = deferred {
                    // 目标显示器暂不在线：先落主屏，等它上线再挪过去；期间的临时坐标不写入存档
                    self.pending_restore.push(PendingRestore {
                        pet: pet.id(),
                        pos: saved,
                    });
                }
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
        if let Ok(menu) = build_context_menu(
            &self.lib,
            on,
            &self.characters,
            &self.current_character,
            &SettingsSnapshot::from_config(),
        ) {
            self.ctx_menu = std::rc::Rc::new(menu);
        }
        if let Some(tray) = &self.tray
            && let Ok(menu) = build_tray_menu(
                on,
                &self.characters,
                &self.current_character,
                &SettingsSnapshot::from_config(),
            )
        {
            tray.set_menu(Some(Box::new(menu)));
        }
    }

    /// 应用设置菜单的一项改动：写入 config，对所有窗口立即生效，并重建两处菜单刷新显示
    fn apply_setting(&mut self, rest: &str) {
        let Some((name, value)) = rest.split_once('.') else {
            log::debug!("无法解析的设置项：{rest}");
            return;
        };
        let percent = value.parse::<i64>().ok().and_then(|v| {
            (1..=200)
                .contains(&v)
                .then_some(f64::from(v as u32) / 100.0)
        });
        match name {
            // 档位值以整数百分比编码，避免 ID 里出现小数点
            "opacity" => {
                let Some(v) = percent else { return };
                config::set("opacity", config::clamp_opacity(v));
            }
            "speed" => {
                let Some(v) = percent else { return };
                config::set("playback_speed", v.clamp(1.0, 2.0));
            }
            // 开关项翻转当前值
            "lock" => {
                let on = !config::get_bool("lock_position").unwrap_or(false);
                config::set("lock_position", on);
            }
            "shift" => {
                let on = !config::get_bool("shift_drag_only").unwrap_or(false);
                config::set("shift_drag_only", on);
            }
            "power" => {
                let on = config::get_i64("power_saving_idle_secs").unwrap_or(0) == 0;
                config::set("power_saving_idle_secs", if on { POWER_ON_SECS } else { 0 });
            }
            _ => log::debug!("未知设置项：{name}"),
        }
        for pet in &mut self.pets {
            pet.refresh_settings();
        }
        let settings = SettingsSnapshot::from_config();
        if let Ok(menu) = build_context_menu(
            &self.lib,
            autostart::is_enabled(),
            &self.characters,
            &self.current_character,
            &settings,
        ) {
            self.ctx_menu = std::rc::Rc::new(menu);
        }
        if let Some(tray) = &self.tray
            && let Ok(menu) = build_tray_menu(
                autostart::is_enabled(),
                &self.characters,
                &self.current_character,
                &settings,
            )
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
            MENU_EXIT => {
                self.save_positions();
                event_loop.exit();
            }
            _ if let Some(char_id) = id.strip_prefix(MENU_CHAR_PREFIX) => {
                self.switch_character(char_id);
            }
            MENU_HIDE => self.toggle_pets_visible(),
            MENU_SPIN => {
                let _ = self.proxy.send_event(UserEvent::SpinPet);
            }
            _ if let Some(rest) = id.strip_prefix(MENU_SET_PREFIX) => {
                self.apply_setting(rest);
            }
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
            UserEvent::SpinPet => {
                // 右键菜单在具体宠物上弹出，回旋也落在它身上
                if let Some(target) = self.menu_target
                    && let Some(pet) = self.pets.iter_mut().find(|p| p.id() == target)
                {
                    pet.start_spin();
                }
            }
            UserEvent::SelfTalk {
                pet_id,
                line,
                clip_name,
            } => {
                log::info!("自言自语：{line}");
                // 台词同时朗读；speak 内部检查 tts_enabled，未开时静默
                crate::tts::speak(&line);
                if let Some(clip) = clip_name
                    && let Some(pet) = self.pets.iter_mut().find(|p| p.id() == pet_id)
                {
                    pet.trigger_clip(&clip);
                }
                // 在宠物上方显示气泡
                if let Some(pet) = self.pets.iter().find(|p| p.id() == pet_id)
                    && let Some((pos, size)) = pet.window_position_and_size()
                {
                    self.bubble = Some(crate::bubble_window::BubbleWindow::new(
                        event_loop, &self.gpu, pos, size, &line,
                    ));
                }
            }
            UserEvent::ExitApp => {
                crate::tts::cleanup();
                self.save_positions();
                event_loop.exit();
            }
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
                if let Some(bubble) = self.bubble.as_mut()
                    && bubble.window_id() == window_id
                {
                    bubble.draw();
                    return;
                }
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
                    // 宠物是置顶层，会盖住菜单与它重叠的行并吃掉那里的点击；弹出期间临时降到普通层级，菜单关闭后恢复
                    self.pets[pos]
                        .window
                        .set_window_level(winit::window::WindowLevel::Normal);
                    platform::show_context_menu(&self.ctx_menu, &self.pets[pos].window, x, y);
                    self.pets[pos]
                        .window
                        .set_window_level(winit::window::WindowLevel::AlwaysOnTop);
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
                // 点托盘图标把全部桌宠显示或隐藏；要新增宠物只能通过右键菜单
                self.toggle_pets_visible();
            }
        }

        // 气泡到时自动关闭（drop 掉 BubbleWindow 随即关窗）
        if self
            .bubble
            .as_ref()
            .is_some_and(crate::bubble_window::BubbleWindow::expired)
        {
            self.bubble = None;
        }

        // 每 2 秒检测一次全屏，有应用全屏时自动隐藏，退出全屏后恢复
        // （只做显示/隐藏切换，不覆盖用户手动隐藏的状态）
        if now.duration_since(self.fullscreen_check_at).as_secs_f64() >= 2.0 {
            self.fullscreen_check_at = now;
            let fullscreen = platform::any_app_fullscreen();
            if fullscreen && !self.pets_hidden {
                self.toggle_pets_visible();
                log::info!("检测到全屏应用，自动隐藏桌宠");
            } else if !fullscreen && self.pets_hidden {
                self.toggle_pets_visible();
                log::info!("全屏应用退出，恢复桌宠显示");
            }
        }

        // 隐藏时不播动画、不算物理，只保留菜单能点
        let mut deadline = now + POLL_TICK;
        if !self.pets_hidden {
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
        }
        self.maintain_positions(event_loop, now);

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

    /// 位置记忆的定期维护（两秒节流）：等待中的显示器一旦上线就把宠物挪回去；位置快照有变化才写入存档
    fn maintain_positions(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        if now.duration_since(self.last_pos_check) < Duration::from_secs(2) {
            return;
        }
        self.last_pos_check = now;

        if !self.pending_restore.is_empty() {
            let monitors: Vec<_> = event_loop.available_monitors().collect();
            let online: Vec<String> = monitors.iter().map(fingerprint_of).collect();
            self.pending_restore.retain(|p| {
                let Some(i) = online.iter().position(|f| *f == p.pos.monitor) else {
                    return true; // 还没上线，继续等
                };
                if let Some(pet) = self.pets.iter().find(|w| w.id() == p.pet) {
                    pet.move_to_monitor(&monitors[i], p.pos.x, p.pos.y);
                    log::info!("目标显示器已上线，恢复宠物位置：{}", p.pos.monitor);
                }
                false // 已恢复（或窗口已关闭）
            });
        }

        self.save_positions();
    }

    /// 把位置快照写入存档；还在等待显示器的宠物跳过，内容没变不重写
    fn save_positions(&mut self) {
        let waiting: Vec<WindowId> = self.pending_restore.iter().map(|p| p.pet).collect();
        let snapshot: Vec<_> = self
            .pets
            .iter()
            .filter_map(|pet| pet.position_snapshot().map(|p| (pet.id(), p)))
            .collect();
        let saved = positions::filter_saved(&snapshot, &waiting);
        let value = positions::to_value(&saved);
        let json = value.to_string();
        if json != self.last_saved_positions {
            self.last_saved_positions = json;
            config::set("pet_positions", value);
        }
    }
}

/// 角色的资产目录：内置角色用仓库里的 assets，外部角色放在系统的角色目录下
fn entry_root(char_id: &str) -> Option<std::path::PathBuf> {
    if char_id == character::BUILTIN_CHARACTER_ID {
        return Some(assets::locate_assets_root());
    }
    let root = character::external_characters_root().join(char_id);
    root.join("frames").is_dir().then_some(root)
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

#[allow(clippy::too_many_lines)]
fn main() {
    match logging::init() {
        Some(path) => log::info!("日志文件：{}", path.display()),
        None => eprintln!("日志初始化失败，仅输出到 stderr"),
    }
    // 启动时就加载配置，坏文件尽早暴露
    log::info!("配置文件：{}", config::init().display());
    audio::init();
    tts::init();

    let builtin_root = assets::locate_assets_root();
    let characters: Vec<character::Character> =
        character::discover_characters(&builtin_root, &character::external_characters_root());
    // 优先用上次选择的角色；它已不存在时回退到内置角色
    let remembered = config::get_str("character");
    let current = remembered
        .as_deref()
        .filter(|id| characters.iter().any(|c| c.id == *id))
        .map_or_else(
            || character::BUILTIN_CHARACTER_ID.to_string(),
            str::to_string,
        );
    let root = characters
        .iter()
        .find(|c| c.id == current)
        .map(|c| c.root.clone())
        .unwrap_or(builtin_root);
    let entries: Vec<CharacterEntry> = characters
        .iter()
        .map(|c| CharacterEntry {
            id: c.id.clone(),
            display_name: c.meta.display_name.clone(),
        })
        .collect();
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
    let autostart_on = autostart::is_enabled();
    let settings = SettingsSnapshot::from_config();
    let ctx_menu = std::rc::Rc::new(
        build_context_menu(&lib, autostart_on, &entries, &current, &settings)
            .expect("右键菜单创建失败"),
    );
    let tray_menu =
        build_tray_menu(autostart_on, &entries, &current, &settings).expect("托盘菜单创建失败");
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

    // 常驻的 idle `ClipFrames`（持强引用防缓存驱逐）
    let idle_clip = lib.find_idle();
    let idle_frames = idle_clip
        .as_ref()
        .and_then(|id| lib.get_or_load(id, clip_loader(&proxy)));

    let gpu = Arc::new(
        render::Gpu::new(event_loop.owned_display_handle()).expect("GPU 上下文初始化失败"),
    );
    let mut app = App {
        lib,
        characters: entries,
        current_character: current,
        pets_hidden: false,
        fullscreen_check_at: Instant::now(),
        bubble: None,
        gpu,
        proxy,
        pets: Vec::new(),
        // 边界依赖显示器信息，resumed 首次回调时补上真实值
        physics: physics::World::new(physics::Bounds {
            width: 0.0,
            height: 0.0,
        }),
        last_phys_at: Instant::now(),
        restore_queue: config::get("pet_positions")
            .as_ref()
            .map(positions::from_value)
            .unwrap_or_default()
            .into(),
        pending_restore: Vec::new(),
        last_saved_positions: String::new(),
        last_pos_check: Instant::now(),
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
        for defs in [
            base_context_defs(true, &snap()),
            tray_menu_defs(false, &[], "shenshen", &snap()),
        ] {
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
        let ctx_defs = base_context_defs(false, &snap());
        let mut ctx = Vec::new();
        collect_ids(&ctx_defs, &mut ctx);
        let tray_defs = tray_menu_defs(false, &[], "shenshen", &snap());
        let mut tray = Vec::new();
        collect_ids(&tray_defs, &mut tray);
        // 顶层：右键七项、托盘五项（各含自启、回旋与设置）；统计是深收集，
        // 「设置」子树再贡献 14 项（不透明度 6 档 + 速率 5 档 + 3 个开关）
        assert_eq!(ctx.len(), 21);
        assert_eq!(tray.len(), 18);
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
    fn build_info_contains_version_and_hash() {
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
    fn position_env_parsing() {
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

    fn snap() -> SettingsSnapshot {
        SettingsSnapshot {
            opacity: 1.0,
            speed: 1.0,
            lock: false,
            shift_only: false,
            power_on: false,
        }
    }

    fn char_entry(id: &str, name: &str) -> CharacterEntry {
        CharacterEntry {
            id: id.to_string(),
            display_name: name.to_string(),
        }
    }

    #[test]
    fn character_submenu_only_with_multiple() {
        // 单角色不出现子菜单，结构不变（右键7项、托盘5项，含自启、回旋与设置）
        let one = [char_entry("shenshen", "shenshen")];
        assert_eq!(
            context_menu_defs(&[], false, &one, "shenshen", &snap()).len(),
            8
        );
        assert_eq!(tray_menu_defs(false, &one, "shenshen", &snap()).len(), 5);
        // 双角色：在「退出」前插入「角色」子菜单，当前角色的一项带「✓」前缀
        let two = [
            char_entry("shenshen", "申申"),
            char_entry("test", "测试角色"),
        ];
        let defs = context_menu_defs(&[], false, &two, "test", &snap());
        // 顺序：新增/关闭/黄金回旋/日志/自启/设置/角色/退出
        let MenuItemDef::Submenu { label, children } = &defs[7] else {
            panic!("第八项应为角色子菜单");
        };
        assert_eq!(label, "角色");
        assert_eq!(children.len(), 2);
        let MenuItemDef::Item { id, label } = &children[0] else {
            panic!("角色子项应为普通项");
        };
        assert_eq!(id, "app.char.shenshen");
        assert_eq!(label, "申申");
        let MenuItemDef::Item { label, .. } = &children[1] else {
            unreachable!();
        };
        assert_eq!(label, "✓ 测试角色");
        assert_eq!(defs.len(), 9);
    }

    #[test]
    fn settings_submenu_marks_current_levels() {
        let mut sn = snap();
        sn.opacity = 0.7;
        sn.speed = 1.5;
        sn.lock = true;
        let MenuItemDef::Submenu { children, .. } = settings_submenu(&sn) else {
            unreachable!();
        };
        // 前两级是档位子菜单，后三个是开关
        let MenuItemDef::Submenu {
            children: opacity_items,
            ..
        } = &children[0]
        else {
            unreachable!();
        };
        let MenuItemDef::Item { id, label } = &opacity_items[3] else {
            unreachable!();
        };
        assert_eq!(id, "app.set.opacity.70");
        assert_eq!(label, "✓ 70%");
        let MenuItemDef::Item { label, .. } = &opacity_items[0] else {
            unreachable!();
        };
        assert_eq!(label, "100%");
        let MenuItemDef::Submenu {
            children: speed_items,
            ..
        } = &children[1]
        else {
            unreachable!();
        };
        let MenuItemDef::Item { id, label, .. } = &speed_items[2] else {
            unreachable!();
        };
        assert_eq!(id, "app.set.speed.150");
        assert_eq!(label, "✓ 150%");
        let MenuItemDef::Check { id, checked, .. } = &children[2] else {
            unreachable!();
        };
        assert_eq!(id, "app.set.lock");
        assert!(*checked);
    }

    #[test]
    fn remembered_character_falls_back_to_builtin() {
        // 决策逻辑与启动处一致：记住的角色不在列表内时回退到内置角色
        let ids = ["shenshen".to_string()];
        let remembered = Some("gone".to_string());
        let current = remembered
            .as_deref()
            .filter(|id| ids.iter().any(|c| c == *id))
            .map_or_else(
                || character::BUILTIN_CHARACTER_ID.to_string(),
                str::to_string,
            );
        assert_eq!(current, character::BUILTIN_CHARACTER_ID);
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
        let defs = context_menu_defs(&["balance".to_string()], false, &[], "shenshen", &snap());
        // 基础八项之上插入「动画」子菜单
        assert_eq!(defs.len(), 9);
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
        assert_eq!(
            context_menu_defs(&[], false, &[], "shenshen", &snap()).len(),
            8
        );
    }
}
