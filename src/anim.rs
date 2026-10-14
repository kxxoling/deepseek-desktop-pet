//! 动画状态机
//!
//! - Idle：idle 片段循环，每 20–40s 触发随机事件（40% 行走 / 60% 随机动画）
//! - OneShot：random/click 片段播完回 Idle
//! - Drag：drag 片段循环（拖拽中与松手惯性）
//! - Walk：move 片段循环，窗口按步幅曲线位移（速度 = stride/圈时长，步态不打滑）

use std::sync::Arc;
use std::time::Instant;

use crate::library::{ClipFrames, ClipId, FPS, Frame, SRC_FPS};

/// 随机事件间隔范围（秒）
const RANDOM_EVERY_MIN: f64 = 20.0;
const RANDOM_EVERY_MAX: f64 = 40.0;
/// 随机事件中行走的概率
pub const WALK_PROB: f64 = 0.4;

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
    /// 完整帧集（未加载完成时为 None，用 first 兜底显示）
    pub frames: Option<Arc<ClipFrames>>,
    pub first: Arc<Frame>,
    pub frame_idx: usize,
    /// 水平镜像（朝右行走时翻转；`no_mirror` 名单除外）
    pub mirrored: bool,
    next_frame_at: Option<Instant>,
    /// 下次随机事件时刻（仅 Idle 态有效）
    pub next_event_at: Instant,
    /// 后台加载中的目标 clip（到货即从当前帧集切过去）
    queued: Option<ClipId>,
}

pub fn random_pick<T>(v: &[T]) -> Option<&T> {
    if v.is_empty() {
        None
    } else {
        v.get((rand_u64() % v.len() as u64) as usize)
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
            next_frame_at: None,
            next_event_at: Instant::now()
                + std::time::Duration::from_secs_f64(rand_range(
                    RANDOM_EVERY_MIN,
                    RANDOM_EVERY_MAX,
                )),
            queued: None,
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
        self.next_frame_at = Some(Instant::now() + crate::library::FRAME_DUR);
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

    /// 下一次帧推进时刻（事件循环据此调度唤醒）
    pub fn next_frame_at(&self) -> Option<Instant> {
        self.next_frame_at
    }

    pub fn is_loaded(&self) -> bool {
        self.frames.is_some()
    }

    /// 帧推进。返回 `WalkLoopDone` 时调用方负责行走位移与圈数递减。
    pub fn tick(&mut self, now: Instant) -> Tick {
        let Some(next) = self.next_frame_at else {
            self.next_frame_at = Some(now + crate::library::FRAME_DUR);
            return Tick::Idle;
        };
        if now < next {
            return Tick::Idle;
        }
        // 补帧上限：落后超过 4 帧直接重同步，避免后台加载/卡顿后狂追帧
        let base = if next.elapsed() > crate::library::FRAME_DUR * 4 {
            now
        } else {
            next
        };
        self.next_frame_at = Some(base + crate::library::FRAME_DUR);

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
    fn curve_at_按时间线性插值() {
        let curve = vec![0.0, 0.5, 1.0];
        // 15fps 输出帧 1 对应源帧 1*24/15=1.6：0.5 与 1.0 之间 0.6 处 → 0.8
        assert!((curve_at(&curve, 1) - 0.8).abs() < 1e-9);
        assert!(curve_at(&curve, 0).abs() < f64::EPSILON);
    }

    #[test]
    fn curve_at_超界钳制到末尾() {
        let curve = vec![0.0, 0.5, 1.0];
        assert!((curve_at(&curve, 100) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn curve_at_线性曲线保持单调() {
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
    fn 初始状态() {
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
    fn 未加载时显示首帧并排队() {
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
    fn 到货但不匹配则丢弃() {
        let mut a = Anim::new(&ClipId(Arc::from("idle/x")), first_frame(), None);
        assert!(!a.on_clip_loaded(&ClipId(Arc::from("other/z")), frames_n(1)));
        assert!(!a.is_loaded());
    }

    #[test]
    fn 当前clip未加载时到货也接管() {
        let mut a = Anim::new(&ClipId(Arc::from("idle/x")), first_frame(), None);
        // queued 为 None（构造时未排队），但 clip 匹配且 frames 空
        assert!(a.on_clip_loaded(&ClipId(Arc::from("idle/x")), frames_n(1)));
    }

    #[test]
    fn enter_idle重排随机事件() {
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
    fn tick初始化与未到时不推进() {
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
    fn tick连续推进不重同步() {
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
    fn tick推进与回绕() {
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
    fn tick落后超限重同步() {
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
    fn oneshot播完返回finished() {
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
    fn walk走完一圈返回() {
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
    fn walkdir方向与翻转() {
        assert!((WalkDir::Left.sign() - (-1.0)).abs() < f64::EPSILON);
        assert!((WalkDir::Right.sign() - 1.0).abs() < f64::EPSILON);
        assert_eq!(WalkDir::Left.flip(), WalkDir::Right);
        assert_eq!(WalkDir::Right.flip(), WalkDir::Left);
    }

    #[test]
    fn random_pick空与非空() {
        let empty: [u8; 0] = [];
        assert!(random_pick(&empty).is_none());
        let v = [10u8, 20];
        let picked = random_pick(&v).unwrap();
        assert!(*picked == 10 || *picked == 20);
    }

    #[test]
    fn rand_range在界内() {
        for _ in 0..100 {
            let r = rand_range(5.0, 7.0);
            assert!((5.0..=7.0).contains(&r));
        }
    }
}
