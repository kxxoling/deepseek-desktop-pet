//! 动画状态机
//!
//! - Idle：idle 片段循环，每 20–40s 触发随机事件（穿插转身 / 行走 / 随机动画）
//! - OneShot：单次片段播完回 Idle（可携带后续计划）
//! - Drag：drag 片段循环（拖拽中与松手惯性）
//! - Walk：move 片段循环，窗口按步幅曲线位移（速度 = stride/圈时长，步态不打滑）
//!
//! events 类片段不参与自主调度，需要时通过 [`Anim::trigger_clip`] 触发播放。

use std::sync::Arc;
use std::time::Instant;

use crate::library::{ClipFrames, ClipId, FPS, Frame, Library, SRC_FPS};

/// 随机事件间隔范围（秒）
const RANDOM_EVERY_MIN: f64 = 20.0;
const RANDOM_EVERY_MAX: f64 = 40.0;
/// 随机事件中行走的概率
pub const WALK_PROB: f64 = 0.4;
/// 随机事件中穿插 turn 片段的概率（无 turn 资产时不生效）
pub const TURN_IDLE_PROB: f64 = 0.15;
/// 播放速率范围（config `playback_speed`，越界钳制）
pub const SPEED_MIN: f64 = 1.0;
pub const SPEED_MAX: f64 = 2.0;

/// 省电判定：闲置达到阈值后隔帧渲染。返回渲染步进（1=每帧，2=隔一帧）。
/// 阈值为 0 表示功能关闭，恒为 1。渲染节拍由调用方掌握。
pub fn power_save_stride(idle_secs: u64, threshold_secs: u64) -> u32 {
    if threshold_secs > 0 && idle_secs >= threshold_secs {
        2
    } else {
        1
    }
}

pub fn rand_range(min: f64, max: f64) -> f64 {
    min + (max - min) * (rand_u64() as f64 / u64::MAX as f64)
}

/// xorshift64* 随机源（避免引入 rand 依赖）
pub fn rand_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut s = STATE.load(Ordering::Relaxed);
    if s == 0 {
        s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
            | 1;
    }
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    STATE.store(s, Ordering::Relaxed);
    s.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalkDir {
    Left,
    Right,
}

impl WalkDir {
    pub fn sign(self) -> f64 {
        match self {
            WalkDir::Left => -1.0,
            WalkDir::Right => 1.0,
        }
    }
    pub fn flip(self) -> Self {
        match self {
            WalkDir::Left => WalkDir::Right,
            WalkDir::Right => WalkDir::Left,
        }
    }
}

#[derive(Debug)]
pub enum Mode {
    Idle,
    /// 播完回 Idle 的单次片段（random / click）
    OneShot,
    /// 拖拽中或松手惯性滑行
    Drag,
    /// 行走：clip 每循环一圈位移 stride px
    Walk {
        dir: WalkDir,
        remaining_loops: u32,
    },
}

/// 帧推进结果
pub enum Tick {
    /// 无事发生（未到帧时间）
    Idle,
    /// 正常推进一帧
    Advanced,
    /// `OneShot` 播完
    Finished,
    /// Walk 走完一圈
    WalkLoopDone,
}

pub struct Anim {
    pub mode: Mode,
    pub clip: ClipId,
    /// 该片段的完整 `ClipFrames`（未加载完成时为 None，用 first 兜底显示）
    pub frames: Option<Arc<ClipFrames>>,
    pub first: Arc<Frame>,
    pub frame_idx: usize,
    /// 水平镜像（朝右行走时翻转；`no_mirror` 名单除外）
    pub mirrored: bool,
    /// 播放速率（1.0–2.0），帧间隔按此缩短
    speed: f64,
    next_frame_at: Option<Instant>,
    /// 下次随机事件时刻（仅 Idle 态有效）
    pub next_event_at: Instant,
    /// 后台加载中的目标 clip（到货即切到它的 `ClipFrames`）
    queued: Option<ClipId>,
    /// 单次片段播完后的后续计划；切片段即作废
    pending: Option<Plan>,
    /// `trigger_clip` 的候选片段池，首帧直接共享库里的缓存
    event_clips: Vec<(ClipId, Arc<Frame>)>,
    /// 行走途中受理的触发片段，等当前这圈走完再播放；一旦切换片段就作废
    pending_trigger: Option<ClipId>,
}

pub fn random_pick<T>(v: &[T]) -> Option<&T> {
    if v.is_empty() {
        None
    } else {
        v.get((rand_u64() % v.len() as u64) as usize)
    }
}

// ---------- 调度决策 ----------
// 纯函数：库概况以快照传入，随机值由调用方注入，副作用留给 window

/// 调度所需的库快照
pub struct SchedSnapshot {
    /// 库中存在 turn 类片段
    pub turn_available: bool,
    /// 库中存在带步幅的 move 片段（Walk 分支可走）
    pub walk_available: bool,
}

/// 从 `Library` 提取调度快照
pub fn sched_snapshot(lib: &Library) -> SchedSnapshot {
    SchedSnapshot {
        turn_available: !lib.clips_in_category("turn").is_empty(),
        walk_available: lib.clips_in_category("move").iter().any(|id| {
            let name = lib.meta(id).map(|m| m.name.to_string()).unwrap_or_default();
            lib.move_strides.contains_key(&name)
        }),
    }
}

/// 调度计划：window 侧按此逐项执行剪辑切换
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// 回到 Idle
    Idle,
    /// 先播一个 turn 片段，再执行 `next`
    TurnThen(Box<Plan>),
    /// random 类随机单次片段
    RandomClip,
    /// 行走 `loops` 圈（1..=3）
    Walk { loops: u32 },
}

/// Idle 随机事件决策，随机值由调用方注入；无 turn 资产时行为与之前一致
pub fn plan_idle_event(
    snap: &SchedSnapshot,
    roll_event: f64,
    roll_turn: f64,
    roll_loops: u64,
) -> Plan {
    if snap.turn_available && roll_turn < TURN_IDLE_PROB {
        return Plan::TurnThen(Box::new(Plan::Idle));
    }
    if roll_event < WALK_PROB && snap.walk_available {
        let walk = Plan::Walk {
            loops: 1 + (roll_loops % 3) as u32,
        };
        return if snap.turn_available {
            Plan::TurnThen(Box::new(walk))
        } else {
            walk
        };
    }
    Plan::RandomClip
}

/// Walk 走完后的去向：有 turn 资产则先转身再回 Idle
pub fn plan_walk_end(snap: &SchedSnapshot) -> Plan {
    if snap.turn_available {
        Plan::TurnThen(Box::new(Plan::Idle))
    } else {
        Plan::Idle
    }
}

/// 时间对齐重采样 24fps 曲线到 15fps：输出帧 i 对应源帧号 i*24/15，线性插值
pub fn curve_at(curve: &[f64], out_frame_idx: usize) -> f64 {
    let t = (out_frame_idx as f64 * (SRC_FPS / FPS)).min(curve.len().saturating_sub(1) as f64);
    let i0 = t.floor() as usize;
    let i1 = (i0 + 1).min(curve.len() - 1);
    let f = t - i0 as f64;
    curve[i0] * (1.0 - f) + curve[i1] * f
}

impl Anim {
    pub fn new(idle: &ClipId, first: Arc<Frame>, frames: Option<Arc<ClipFrames>>) -> Self {
        Self {
            mode: Mode::Idle,
            clip: idle.clone(),
            frames,
            first,
            frame_idx: 0,
            mirrored: false,
            speed: 1.0,
            next_frame_at: None,
            next_event_at: Instant::now()
                + std::time::Duration::from_secs_f64(rand_range(
                    RANDOM_EVERY_MIN,
                    RANDOM_EVERY_MAX,
                )),
            queued: None,
            pending: None,
            event_clips: Vec::new(),
            pending_trigger: None,
        }
    }

    /// 切换片段（mode 由调用方设定）。未加载完成时显示首帧并排队。
    pub fn set_clip(
        &mut self,
        id: &ClipId,
        first: Arc<Frame>,
        frames: Option<Arc<ClipFrames>>,
        mode: Mode,
        mirrored: bool,
    ) {
        self.queued = frames.is_none().then(|| id.clone());
        self.frames = frames;
        self.clip = id.clone();
        self.first = first;
        self.frame_idx = 0;
        self.mode = mode;
        self.mirrored = mirrored;
        self.next_frame_at = Some(Instant::now() + self.frame_interval());
        // 任何剪辑切换都使既有的后续计划与待播触发作废
        self.pending = None;
        self.pending_trigger = None;
    }

    /// 设置单次片段播完后的去向
    pub fn set_pending(&mut self, next: Plan) {
        self.pending = Some(next);
    }

    /// 取走 `OneShot` 播完后的去向（一次性）
    pub fn take_pending(&mut self) -> Option<Plan> {
        self.pending.take()
    }

    /// 切到 Idle（保持已有镜像朝向，避免静止时突然翻转）
    pub fn enter_idle(
        &mut self,
        idle: &ClipId,
        first: Arc<Frame>,
        frames: Option<Arc<ClipFrames>>,
    ) {
        self.set_clip(idle, first, frames, Mode::Idle, self.mirrored);
        self.schedule_next_event();
    }

    pub fn schedule_next_event(&mut self) {
        self.next_event_at = Instant::now()
            + std::time::Duration::from_secs_f64(rand_range(RANDOM_EVERY_MIN, RANDOM_EVERY_MAX));
    }

    /// 后台加载到货。返回 true 表示当前显示的正是该 clip（应继续播放）。
    pub fn on_clip_loaded(&mut self, id: &ClipId, frames: Arc<ClipFrames>) -> bool {
        if self.queued.as_ref().is_some_and(|q| q == id)
            || (self.clip == *id && self.frames.is_none())
        {
            self.frames = Some(frames);
            self.queued = None;
            self.frame_idx = 0;
            self.next_frame_at = Some(Instant::now() + crate::library::FRAME_DUR);
            true
        } else {
            false // 已切走，丢弃
        }
    }

    /// 注入候选片段池；首帧直接共享库里的缓存
    pub fn set_event_clips(&mut self, clips: Vec<(ClipId, Arc<Frame>)>) {
        self.event_clips = clips;
    }

    /// 按 spec 前缀筛选候选片段，spec 为类名或「类/子分类」
    #[allow(dead_code)]
    fn pick_event_clip(&self, spec: &str) -> Option<&(ClipId, Arc<Frame>)> {
        let prefix = format!("{spec}/");
        let cands: Vec<_> = self
            .event_clips
            .iter()
            .filter(|(id, _)| id.0.starts_with(&prefix))
            .collect();
        random_pick(&cands).copied()
    }

    /// 触发播放一个 events 类片段，这是其他功能点播动画的标准入口：只调用
    /// 这个接口，不要直接改动画状态机的内部。spec 为类名或「类/子分类」，
    /// 在候选池内随机选一个。
    ///
    /// 受理规则：待机或播放单次片段时立即切换；行走中则等当前这圈走完；
    /// 拖拽中直接拒绝并返回 false。没有匹配的候选时同样返回 false。
    #[allow(dead_code)]
    pub fn trigger_clip(&mut self, spec: &str) -> bool {
        let Some((id, first)) = self.pick_event_clip(spec).cloned() else {
            return false;
        };
        if matches!(self.mode, Mode::Drag) {
            return false;
        }
        if let Mode::Walk {
            remaining_loops, ..
        } = &mut self.mode
        {
            *remaining_loops = 1; // 剩余圈数改为 1：走完脚下这一圈就结束行走，随后播放触发的片段
            self.pending_trigger = Some(id);
        } else {
            self.set_clip(&id, first, None, Mode::OneShot, self.mirrored);
        }
        true
    }

    /// 取走行走结束后待播的触发片段；只有第一次能取到
    pub fn take_pending_trigger(&mut self) -> Option<ClipId> {
        self.pending_trigger.take()
    }

    /// 当前应显示的帧（未加载完成时为首帧兜底）
    pub fn current_frame(&self) -> &[u8] {
        match &self.frames {
            Some(cf) => cf.frame(self.frame_idx),
            None => &self.first.0,
        }
    }

    pub fn frame_len(&self) -> usize {
        self.frames.as_ref().map_or(1, |f| f.frame_count())
    }

    /// 把动画进度归零，从现在重新播放。长时间暂停后恢复时必须调这个，否则恢复的一瞬间动画会先快进一阵才停下来
    pub fn resync_clock(&mut self) {
        self.next_frame_at = None;
    }

    /// 下一次帧推进时刻（事件循环据此调度唤醒）
    pub fn next_frame_at(&self) -> Option<Instant> {
        self.next_frame_at
    }

    /// 设置播放速率，越界钳制到 1.0–2.0
    pub fn set_speed(&mut self, speed: f64) {
        self.speed = speed.clamp(SPEED_MIN, SPEED_MAX);
    }

    /// 当前帧间隔（提高速率靠缩短间隔，不跳帧）
    fn frame_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(crate::library::FRAME_DUR.as_secs_f64() / self.speed)
    }

    pub fn is_loaded(&self) -> bool {
        self.frames.is_some()
    }

    /// 加载排队中的 clip，window 据此补懒加载
    pub fn queued_clip(&self) -> Option<ClipId> {
        self.queued.clone()
    }

    /// 帧推进。返回 `WalkLoopDone` 时调用方负责行走位移与圈数递减。
    pub fn tick(&mut self, now: Instant) -> Tick {
        let interval = self.frame_interval();
        let Some(next) = self.next_frame_at else {
            self.next_frame_at = Some(now + interval);
            return Tick::Idle;
        };
        if now < next {
            return Tick::Idle;
        }
        // 补帧上限：落后超过 4 帧直接重同步，避免后台加载/卡顿后狂追帧
        let base = if now - next > interval * 4 { now } else { next };
        self.next_frame_at = Some(base + interval);

        let len = self.frame_len();
        let next_idx = self.frame_idx + 1;
        if next_idx < len {
            self.frame_idx = next_idx;
            Tick::Advanced
        } else {
            self.frame_idx = 0;
            match &self.mode {
                Mode::Idle | Mode::Drag => Tick::Advanced,
                Mode::OneShot => Tick::Finished,
                Mode::Walk { .. } => Tick::WalkLoopDone,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_save_stride_all_branches() {
        // 阈值 0 表示关闭，闲置再久也不降频
        assert_eq!(power_save_stride(0, 0), 1);
        assert_eq!(power_save_stride(600, 0), 1);
        // 未到阈值不降频，到达即隔帧
        assert_eq!(power_save_stride(29, 30), 1);
        assert_eq!(power_save_stride(30, 30), 2);
        assert_eq!(power_save_stride(300, 30), 2);
    }

    #[test]
    fn curve_at_interpolates_by_time() {
        let curve = vec![0.0, 0.5, 1.0];
        // 15fps 输出帧 1 对应源帧 1*24/15=1.6：0.5 与 1.0 之间 0.6 处 → 0.8
        assert!((curve_at(&curve, 1) - 0.8).abs() < 1e-9);
        assert!(curve_at(&curve, 0).abs() < f64::EPSILON);
    }

    #[test]
    fn curve_at_clamps_beyond_end() {
        let curve = vec![0.0, 0.5, 1.0];
        assert!((curve_at(&curve, 100) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn curve_at_linear_stays_monotonic() {
        let curve: Vec<f64> = (0..=240).map(|i| f64::from(i) / 240.0).collect();
        let mut prev = f64::MIN;
        for f in 0..150 {
            let v = curve_at(&curve, f);
            assert!(v >= prev - 1e-9, "帧 {f} 处曲线回退：{v} < {prev}");
            prev = v;
        }
        // 149 帧对应源帧 238.4/240 ≈ 0.9933，未到曲线终点（播放到末帧才为 1）
        assert!((prev - 0.993_333).abs() < 1e-3);
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    fn frames_n(n: usize) -> Arc<ClipFrames> {
        Arc::new(ClipFrames::from_flat(vec![
            7u8;
            n * crate::library::FRAME_BYTES
        ]))
    }

    fn first_frame() -> Arc<Frame> {
        Arc::new(Frame(vec![9u8; crate::library::FRAME_BYTES]))
    }

    #[test]
    fn initial_state() {
        let a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(3)),
        );
        assert!(matches!(a.mode, Mode::Idle));
        assert!(a.is_loaded());
        assert_eq!(a.frame_len(), 3);
        assert_eq!(a.current_frame()[0], 7);
        assert!(a.next_frame_at().is_none());
        assert!(a.next_event_at > Instant::now());
    }

    #[test]
    fn unloaded_shows_first_frame_and_queues() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(3)),
        );
        a.set_clip(
            &ClipId(Arc::from("random/y")),
            first_frame(),
            None,
            Mode::OneShot,
            true,
        );
        assert!(!a.is_loaded());
        assert_eq!(a.frame_len(), 1);
        assert_eq!(a.current_frame()[0], 9); // 首帧兜底
        assert!(a.mirrored);
        // 排队的 clip 到货后接管
        assert!(a.on_clip_loaded(&ClipId(Arc::from("random/y")), frames_n(2)));
        assert!(a.is_loaded());
        assert_eq!(a.frame_len(), 2);
        assert_eq!(a.current_frame()[0], 7);
    }

    #[test]
    fn mismatched_load_discarded() {
        let mut a = Anim::new(&ClipId(Arc::from("idle/x")), first_frame(), None);
        assert!(!a.on_clip_loaded(&ClipId(Arc::from("other/z")), frames_n(1)));
        assert!(!a.is_loaded());
    }

    #[test]
    fn matching_load_takes_over_when_unloaded() {
        let mut a = Anim::new(&ClipId(Arc::from("idle/x")), first_frame(), None);
        // queued 为 None（构造时未排队），但 clip 匹配且 frames 空
        assert!(a.on_clip_loaded(&ClipId(Arc::from("idle/x")), frames_n(1)));
    }

    #[test]
    fn enter_idle_reschedules_random_event() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(1)),
        );
        a.set_clip(
            &ClipId(Arc::from("random/y")),
            first_frame(),
            None,
            Mode::OneShot,
            false,
        );
        // 人为把事件时刻拨到现在，enter_idle 应重排到未来
        a.next_event_at = Instant::now();
        a.enter_idle(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(1)),
        );
        assert!(matches!(a.mode, Mode::Idle));
        assert!(a.next_event_at > Instant::now());
    }

    #[test]
    fn tick_init_and_wait_no_advance() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(5)),
        );
        let t0 = Instant::now();
        assert!(matches!(a.tick(t0), Tick::Idle)); // 初始化 next_frame_at
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR / 2),
            Tick::Idle
        )); // 未到
        assert_eq!(a.frame_idx, 0);
    }

    #[test]
    fn tick_advances_without_resync() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(6)),
        );
        let t0 = Instant::now();
        a.tick(t0);
        // 逐帧推进（未超限），next_frame_at 始终为 next + 1 帧
        for i in 1..=5u32 {
            let now = t0 + crate::library::FRAME_DUR * i;
            assert!(matches!(a.tick(now), Tick::Advanced));
            assert_eq!(a.frame_idx, i as usize);
        }
    }

    #[test]
    fn tick_advance_and_wrap() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(2)),
        );
        let t0 = Instant::now();
        a.tick(t0);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::Advanced
        ));
        assert_eq!(a.frame_idx, 1);
        // 回绕（Idle → Advanced）
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR * 2),
            Tick::Advanced
        ));
        assert_eq!(a.frame_idx, 0);
    }

    #[test]
    fn tick_resyncs_when_too_far_behind() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(100)),
        );
        let t0 = Instant::now();
        a.tick(t0);
        // 落后 10 帧（> 4 帧上限）→ 重同步到 t+1 帧
        a.tick(t0 + crate::library::FRAME_DUR * 10);
        assert_eq!(a.frame_idx, 1);
    }

    #[test]
    fn set_speed_clamps_out_of_range() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(3)),
        );
        let dur = crate::library::FRAME_DUR;
        a.set_speed(0.1);
        assert_eq!(a.frame_interval(), dur);
        a.set_speed(9.0);
        assert_eq!(a.frame_interval(), dur / 2);
        a.set_speed(1.5);
        // 1.5 倍速率的间隔无法整除，容差内即可
        let want = std::time::Duration::from_secs_f64(dur.as_secs_f64() / 1.5);
        let got = a.frame_interval();
        let diff = got.abs_diff(want);
        assert!(diff < std::time::Duration::from_nanos(1_000));
    }

    #[test]
    fn default_speed_interval_equals_frame_dur() {
        let a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(3)),
        );
        assert_eq!(a.frame_interval(), crate::library::FRAME_DUR);
    }

    #[test]
    fn double_speed_advances_at_half_frame() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(6)),
        );
        a.set_speed(SPEED_MAX);
        let t0 = Instant::now();
        a.tick(t0);
        // 半帧到点即推进，半帧内未到不连推
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR / 2),
            Tick::Advanced
        ));
        assert_eq!(a.frame_idx, 1);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR * 5 / 8),
            Tick::Idle
        ));
        assert_eq!(a.frame_idx, 1);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::Advanced
        ));
        assert_eq!(a.frame_idx, 2);
    }

    #[test]
    fn one_point_five_speed_advance_sequence() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(6)),
        );
        a.set_speed(1.5);
        let t0 = Instant::now();
        a.tick(t0);
        let step =
            std::time::Duration::from_secs_f64(crate::library::FRAME_DUR.as_secs_f64() / 1.5);
        for i in 1..=3u32 {
            assert!(matches!(a.tick(t0 + step * i), Tick::Advanced));
            assert_eq!(a.frame_idx, i as usize);
        }
        // 下一拍未到不推进
        assert!(matches!(a.tick(t0 + step * 3 + step / 2), Tick::Idle));
    }

    #[test]
    fn double_speed_resync_threshold_scales_down() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(100)),
        );
        a.set_speed(SPEED_MAX);
        let t0 = Instant::now();
        a.tick(t0);
        let half = crate::library::FRAME_DUR / 2;
        // 落后 5 个半帧（> 4 个上限）→ 重同步，next 改从 now 起排
        let late = t0 + half * 6;
        a.tick(late);
        assert_eq!(a.next_frame_at(), Some(late + half));
        // 落后 2 个半帧（未超限）→ 按原时刻表追帧
        let mut b = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(100)),
        );
        b.set_speed(SPEED_MAX);
        b.tick(t0);
        b.tick(t0 + half * 3);
        assert_eq!(b.next_frame_at(), Some(t0 + half * 2));
    }

    #[test]
    fn oneshot_returns_finished_at_end() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(1)),
        );
        a.set_clip(
            &ClipId(Arc::from("click/y")),
            first_frame(),
            Some(frames_n(1)),
            Mode::OneShot,
            false,
        );
        let t0 = Instant::now();
        a.tick(t0);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::Finished
        ));
    }

    #[test]
    fn walk_returns_loop_done() {
        let mut a = Anim::new(
            &ClipId(Arc::from("idle/x")),
            first_frame(),
            Some(frames_n(1)),
        );
        a.set_clip(
            &ClipId(Arc::from("move/y")),
            first_frame(),
            Some(frames_n(1)),
            Mode::Walk {
                dir: WalkDir::Right,
                remaining_loops: 2,
            },
            true,
        );
        let t0 = Instant::now();
        a.tick(t0);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::WalkLoopDone
        ));
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;

    #[test]
    fn walkdir_direction_and_mirror() {
        assert!((WalkDir::Left.sign() - (-1.0)).abs() < f64::EPSILON);
        assert!((WalkDir::Right.sign() - 1.0).abs() < f64::EPSILON);
        assert_eq!(WalkDir::Left.flip(), WalkDir::Right);
        assert_eq!(WalkDir::Right.flip(), WalkDir::Left);
    }

    #[test]
    fn random_pick_empty_and_nonempty() {
        let empty: [u8; 0] = [];
        assert!(random_pick(&empty).is_none());
        let v = [10u8, 20];
        let picked = random_pick(&v).unwrap();
        assert!(*picked == 10 || *picked == 20);
    }

    #[test]
    fn rand_range_stays_in_bounds() {
        for _ in 0..100 {
            let r = rand_range(5.0, 7.0);
            assert!((5.0..=7.0).contains(&r));
        }
    }
}
#[cfg(test)]
mod sched_tests {
    use super::*;

    fn snap(turn: bool, walk: bool) -> SchedSnapshot {
        SchedSnapshot {
            turn_available: turn,
            walk_available: walk,
        }
    }

    #[test]
    fn decision_unchanged_without_turn_clips() {
        let s = snap(false, true);
        // 40% 行走：圈数 = 1 + roll % 3
        assert_eq!(plan_idle_event(&s, 0.0, 0.0, 0), Plan::Walk { loops: 1 });
        assert_eq!(plan_idle_event(&s, 0.39, 0.99, 1), Plan::Walk { loops: 2 });
        assert_eq!(plan_idle_event(&s, 0.399, 0.5, 2), Plan::Walk { loops: 3 });
        assert_eq!(plan_idle_event(&s, 0.399, 0.5, 3), Plan::Walk { loops: 1 });
        // 边界：roll_event 恰好 0.4 不走（严格小于，与原判定一致）
        assert_eq!(plan_idle_event(&s, 0.4, 0.99, 0), Plan::RandomClip);
        assert_eq!(plan_idle_event(&s, 0.9, 0.0, 0), Plan::RandomClip);
        // 无带步幅的 move 片段：Walk 分支整体跳过
        let s2 = snap(false, false);
        assert_eq!(plan_idle_event(&s2, 0.1, 0.0, 0), Plan::RandomClip);
        // 走完直接回 Idle（无转身）
        assert_eq!(plan_walk_end(&s), Plan::Idle);
        assert_eq!(plan_walk_end(&s2), Plan::Idle);
    }

    #[test]
    fn decisions_with_turn_clips() {
        let s = snap(true, true);
        // Idle 穿插：roll_turn < 0.15 → turn 后回 Idle；边界恰好 0.15 不判
        assert_eq!(
            plan_idle_event(&s, 0.9, 0.0, 0),
            Plan::TurnThen(Box::new(Plan::Idle))
        );
        assert_eq!(
            plan_idle_event(&s, 0.9, 0.149, 0),
            Plan::TurnThen(Box::new(Plan::Idle))
        );
        assert_eq!(plan_idle_event(&s, 0.9, 0.15, 0), Plan::RandomClip);
        // Walk 起步先转身：turn roll 不中 + roll_event < 0.4
        assert_eq!(
            plan_idle_event(&s, 0.3, 0.5, 2),
            Plan::TurnThen(Box::new(Plan::Walk { loops: 3 }))
        );
        // 走完先转身再回 Idle
        assert_eq!(plan_walk_end(&s), Plan::TurnThen(Box::new(Plan::Idle)));
        // 有 turn 但无可走片段：Walk 分支跳过，穿插不受影响
        let s2 = snap(true, false);
        assert_eq!(plan_idle_event(&s2, 0.1, 0.9, 0), Plan::RandomClip);
        assert_eq!(
            plan_idle_event(&s2, 0.1, 0.1, 0),
            Plan::TurnThen(Box::new(Plan::Idle))
        );
    }

    // ---------- 两套真实库的快照提取 ----------

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ds-pet-sched-{tag}-{}-{nanos}", std::process::id()))
    }

    /// 写一个单帧 zst clip
    fn write_zst_clip(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let raw = vec![0u8; crate::library::FRAME_BYTES];
        let z = zstd::encode_all(std::io::Cursor::new(&raw), 1).unwrap();
        std::fs::write(dir.join("clip.zst"), z).unwrap();
    }

    #[test]
    fn snapshot_from_two_libraries() {
        // 库 A：turn + 带步幅 move + random；库 B：无 turn、move 无步幅
        let root_a = temp_root("with-turn");
        let frames_a = root_a.join("assets/frames");
        write_zst_clip(&frames_a.join("idle/待机"));
        write_zst_clip(&frames_a.join("turn/左转身"));
        write_zst_clip(&frames_a.join("move/走"));
        write_zst_clip(&frames_a.join("random/笑"));
        std::fs::create_dir_all(root_a.join("assets/meta")).unwrap();
        std::fs::write(
            root_a.join("assets/meta/move_strides.json"),
            r#"{"走":{"stride":240,"curve":[0.0,0.5,1.0]}}"#,
        )
        .unwrap();
        let lib_a = Library::load(&root_a.join("assets")).unwrap();
        let snap_a = sched_snapshot(&lib_a);
        assert!(snap_a.turn_available);
        assert!(snap_a.walk_available);

        let root_b = temp_root("no-turn");
        let frames_b = root_b.join("assets/frames");
        write_zst_clip(&frames_b.join("idle/待机"));
        write_zst_clip(&frames_b.join("move/走")); // 无 move_strides.json → 无步幅
        write_zst_clip(&frames_b.join("random/笑"));
        let lib_b = Library::load(&root_b.join("assets")).unwrap();
        let snap_b = sched_snapshot(&lib_b);
        assert!(!snap_b.turn_available);
        assert!(!snap_b.walk_available);
        // 无 turn 库的决策全部走回退路径
        assert_eq!(plan_idle_event(&snap_b, 0.1, 0.0, 0), Plan::RandomClip);
        assert_eq!(plan_walk_end(&snap_b), Plan::Idle);

        std::fs::remove_dir_all(&root_a).ok();
        std::fs::remove_dir_all(&root_b).ok();
    }

    // ---------- turn 链的迁移与打断 ----------

    fn frames_n(n: usize) -> Arc<ClipFrames> {
        Arc::new(ClipFrames::from_flat(vec![
            7u8;
            n * crate::library::FRAME_BYTES
        ]))
    }

    fn first_frame() -> Arc<Frame> {
        Arc::new(Frame(vec![9u8; crate::library::FRAME_BYTES]))
    }

    fn clip(id: &str) -> ClipId {
        ClipId(Arc::from(id))
    }

    #[test]
    fn walk_end_via_turn_to_idle() {
        // 模拟 window 侧执行 plan_walk_end（有 turn）：切 turn 片段 + set_pending(Idle)
        let mut a = Anim::new(&clip("idle/x"), first_frame(), Some(frames_n(1)));
        a.set_clip(
            &clip("turn/左转身"),
            first_frame(),
            Some(frames_n(1)),
            Mode::OneShot,
            false,
        );
        a.set_pending(Plan::Idle);
        let t0 = Instant::now();
        a.tick(t0);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::Finished
        ));
        // window 在 Finished 时取后续计划并执行
        assert_eq!(a.take_pending(), Some(Plan::Idle));
        assert_eq!(a.take_pending(), None); // 一次性
        a.enter_idle(&clip("idle/x"), first_frame(), Some(frames_n(1)));
        assert!(matches!(a.mode, Mode::Idle));
    }

    #[test]
    fn walk_start_via_turn() {
        let mut a = Anim::new(&clip("idle/x"), first_frame(), Some(frames_n(1)));
        // window 执行 plan_idle_event = TurnThen(Walk)：先切 turn
        a.set_clip(
            &clip("turn/右转身"),
            first_frame(),
            Some(frames_n(1)),
            Mode::OneShot,
            false,
        );
        a.set_pending(Plan::Walk { loops: 2 });
        let t0 = Instant::now();
        a.tick(t0);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::Finished
        ));
        // window 拿到 Walk 计划：切 move 片段进 Walk
        a.set_clip(
            &clip("move/走"),
            first_frame(),
            Some(frames_n(1)),
            Mode::Walk {
                dir: WalkDir::Right,
                remaining_loops: 2,
            },
            true,
        );
        assert!(matches!(a.mode, Mode::Walk { .. }));
        // 走完一圈：window 递减圈数（非 0 继续）
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR * 2),
            Tick::WalkLoopDone
        ));
    }

    #[test]
    fn clip_switch_cancels_pending() {
        let mut a = Anim::new(&clip("idle/x"), first_frame(), Some(frames_n(1)));
        a.set_pending(Plan::Idle);
        // 拖拽/点击等任何切换都会使 pending 失效
        a.set_clip(
            &clip("drag/抓"),
            first_frame(),
            Some(frames_n(1)),
            Mode::Drag,
            false,
        );
        assert_eq!(a.take_pending(), None);
        a.set_pending(Plan::Walk { loops: 1 });
        a.enter_idle(&clip("idle/x"), first_frame(), Some(frames_n(1)));
        assert_eq!(a.take_pending(), None);
    }
}

#[cfg(test)]
mod trigger_tests {
    use super::*;

    fn frames_n(n: usize) -> Arc<ClipFrames> {
        Arc::new(ClipFrames::from_flat(vec![
            7u8;
            n * crate::library::FRAME_BYTES
        ]))
    }

    fn first_frame(fill: u8) -> Arc<Frame> {
        Arc::new(Frame(vec![fill; crate::library::FRAME_BYTES]))
    }

    fn clip(id: &str) -> ClipId {
        ClipId(Arc::from(id))
    }

    /// 候选池：balance 子类两条，其他子类一条
    fn events_pool() -> Vec<(ClipId, Arc<Frame>)> {
        vec![
            (clip("events/balance/没钱了"), first_frame(1)),
            (clip("events/balance/发工资"), first_frame(2)),
            (clip("events/喝水"), first_frame(3)),
        ]
    }

    fn anim_with_events(mode: Mode) -> Anim {
        let mut a = Anim::new(&clip("idle/x"), first_frame(9), Some(frames_n(1)));
        a.set_event_clips(events_pool());
        if !matches!(mode, Mode::Idle) {
            let id = match &mode {
                Mode::Walk { .. } => "move/走",
                Mode::Drag => "drag/抓",
                Mode::OneShot => "random/笑",
                Mode::Idle => unreachable!(),
            };
            a.set_clip(&clip(id), first_frame(9), Some(frames_n(1)), mode, false);
        }
        a
    }

    #[test]
    fn acceptance_matrix() {
        // 待机时立即受理：切到单次播放，`ClipFrames` 排队加载，先显示选中的首帧
        let mut a = anim_with_events(Mode::Idle);
        assert!(a.trigger_clip("events"));
        assert!(matches!(a.mode, Mode::OneShot));
        assert!(a.clip.0.starts_with("events/"));
        assert!(!a.is_loaded()); // 没有预加载，排队等窗口层补上
        assert_eq!(a.queued_clip(), Some(a.clip.clone())); // 排队的正是触发片段
        assert!(matches!(a.current_frame()[0], 1..=3)); // 先显示选中的首帧
        // 播放单次片段时再次触发：受理并替换当前片段
        assert!(a.trigger_clip("events/balance"));
        assert!(a.clip.0.starts_with("events/balance/"));
        // 拖拽中：拒绝，状态保持不变
        let mut d = anim_with_events(Mode::Drag);
        let before = d.clip.clone();
        assert!(!d.trigger_clip("events"));
        assert_eq!(d.clip, before);
        assert!(matches!(d.mode, Mode::Drag));
        // 类名没有匹配或池为空：拒绝
        let mut a2 = anim_with_events(Mode::Idle);
        assert!(!a2.trigger_clip("nonexistent"));
        let mut a3 = Anim::new(&clip("idle/x"), first_frame(9), Some(frames_n(1)));
        assert!(!a3.trigger_clip("events")); // 没有注入候选池
    }

    #[test]
    fn walk_trigger_plays_after_current_loop() {
        let mut a = anim_with_events(Mode::Walk {
            dir: WalkDir::Right,
            remaining_loops: 3,
        });
        assert!(a.trigger_clip("events"));
        // 剩余圈数改为 1：走完脚下这一圈就结束行走
        assert!(matches!(
            &a.mode,
            Mode::Walk {
                remaining_loops: 1,
                ..
            }
        ));
        assert!(a.is_loaded()); // 未切片段，仍播放当前 walk 帧
        let t0 = Instant::now();
        a.tick(t0);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::WalkLoopDone
        ));
        // 走完后的取片段与切换归窗口层，这里模拟它的动作
        let id = a.take_pending_trigger().expect("应有待播触发");
        assert!(id.0.starts_with("events/"));
        assert_eq!(a.take_pending_trigger(), None); // 一次性
        a.set_clip(&id, first_frame(1), None, Mode::OneShot, false);
        assert!(matches!(a.mode, Mode::OneShot));
    }

    #[test]
    fn subcategory_and_category_random() {
        // 指定子分类时，只会在 balance 的两条里选
        for _ in 0..30 {
            let mut a = anim_with_events(Mode::Idle);
            assert!(a.trigger_clip("events/balance"));
            assert!(
                a.clip == clip("events/balance/没钱了") || a.clip == clip("events/balance/发工资")
            );
        }
        // 指定整类时，三条都可能选中
        for _ in 0..30 {
            let mut a = anim_with_events(Mode::Idle);
            assert!(a.trigger_clip("events"));
            assert!(a.clip.0.starts_with("events/"));
        }
        // 只有一个候选时，命中的必然是它
        let mut a = anim_with_events(Mode::Idle);
        a.set_event_clips(vec![(clip("events/balance/没钱了"), first_frame(1))]);
        assert!(a.trigger_clip("events/balance"));
        assert_eq!(a.clip, clip("events/balance/没钱了"));
        // 前缀必须对准目录层级，events/balancex 不会误命中 balance
        let mut b = anim_with_events(Mode::Idle);
        assert!(!b.trigger_clip("events/balancex")); // 前缀必须按目录段匹配
    }

    #[test]
    fn triggered_clip_returns_to_idle() {
        let mut a = anim_with_events(Mode::Idle);
        a.set_event_clips(vec![(clip("events/喝水"), first_frame(3))]);
        assert!(a.trigger_clip("events"));
        let t0 = Instant::now();
        a.tick(t0);
        assert!(matches!(
            a.tick(t0 + crate::library::FRAME_DUR),
            Tick::Finished
        ));
        a.enter_idle(&clip("idle/x"), first_frame(9), Some(frames_n(1)));
        assert!(matches!(a.mode, Mode::Idle));
    }

    #[test]
    fn clip_switch_cancels_pending_trigger() {
        let mut a = anim_with_events(Mode::Walk {
            dir: WalkDir::Left,
            remaining_loops: 2,
        });
        assert!(a.trigger_clip("events"));
        // 拖拽打断后，待播的触发片段作废
        a.set_clip(
            &clip("drag/抓"),
            first_frame(9),
            Some(frames_n(1)),
            Mode::Drag,
            false,
        );
        assert_eq!(a.take_pending_trigger(), None);
    }
}
