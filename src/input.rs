//! 单击/双击/拖拽开始的判定，纯状态机，不依赖窗口系统。判定规则：
//! 距上次按下不足 `DOUBLE_CLICK_SECS` 的按下算双击；短按且几乎没动算单击。
//! 失焦只终止拖拽，不清点击状态——快速两次按下之间可能夹着失焦事件，
//! 清了会误杀双击。光标位置由调用方随事件传入，判定器自己不跟踪。

use std::time::Instant;

/// 按下后超过此时长，松开不再判单击
pub const CLICK_SECS: f64 = 0.35;
/// 两次按下之间的双击窗口
pub const DOUBLE_CLICK_SECS: f64 = 0.4;
/// 松开时位移低于此值（px）才判单击
pub const CLICK_MOVE_PX: f64 = 6.0;

/// 判定输出
#[derive(Debug, PartialEq)]
pub enum Action {
    /// 单击（短按且未位移）：播 click 动画
    Click,
    /// 双击窗口内的第二次按下：多开一只
    DoubleClick,
    /// 普通按下：立即进入拖拽；`offset` 为抓取偏移（按下时的窗口内坐标）
    DragStart { offset: (f64, f64) },
    /// 无判定：长按/大位移的松手交给惯性，或被双击抑制的松开
    None,
}

/// 点击判定状态机（每窗口一份）
#[derive(Default)]
pub struct InputState {
    /// 上一次按下时刻（双击窗口与单击时长的共同基准；松开不重置）
    last_press: Option<Instant>,
    /// 按下时的光标位置（位移判定基准）
    press_pos: (f64, f64),
    /// 双击后的松开抑制单击
    suppress_single: bool,
}

impl InputState {
    /// 左键按下。`pos` 为窗口内逻辑坐标；返回 [`Action::DragStart`] 时
    /// `offset` 即抓取偏移。
    pub fn press(&mut self, now: Instant, pos: (f64, f64)) -> Action {
        if let Some(t) = self.last_press
            && now.duration_since(t).as_secs_f64() < DOUBLE_CLICK_SECS
        {
            self.last_press = None; // 三连击的第三次按下重新按普通按下处理
            self.suppress_single = true;
            return Action::DoubleClick;
        }
        self.last_press = Some(now);
        self.press_pos = pos;
        self.suppress_single = false;
        Action::DragStart { offset: pos }
    }

    /// 左键松开。无论结果如何都会推进内部状态（清抑制标志），
    /// 拖拽中的松手由 window 侧据此转入惯性。
    pub fn release(&mut self, now: Instant, pos: (f64, f64)) -> Action {
        let click = !self.suppress_single
            && self
                .last_press
                .is_some_and(|t| now.duration_since(t).as_secs_f64() < CLICK_SECS)
            && dist(pos, self.press_pos) < CLICK_MOVE_PX;
        self.suppress_single = false;
        if click { Action::Click } else { Action::None }
    }
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    #[test]
    fn short_press_no_move_is_click_with_offset() {
        let mut s = InputState::default();
        let t0 = Instant::now();
        assert_eq!(
            s.press(t0, (10.0, 20.0)),
            Action::DragStart {
                offset: (10.0, 20.0)
            }
        );
        // 0.34s 后松开，位移 ~2.2px：单击
        assert_eq!(s.release(at(t0, 0.34), (11.0, 22.0)), Action::Click);
    }

    #[test]
    fn long_press_boundary_not_click() {
        let mut s = InputState::default();
        let t0 = Instant::now();
        s.press(t0, (5.0, 5.0));
        // 恰好 CLICK_SECS：严格小于才判，边界不判
        assert_eq!(s.release(at(t0, CLICK_SECS), (5.0, 5.0)), Action::None);
        // 明显长按
        s.press(at(t0, 1.0), (5.0, 5.0));
        assert_eq!(s.release(at(t0, 2.0), (5.0, 5.0)), Action::None);
    }

    #[test]
    fn move_threshold_boundary() {
        let mut s = InputState::default();
        let t0 = Instant::now();
        s.press(t0, (0.0, 0.0));
        // 位移恰好 6px：不判
        assert_eq!(s.release(at(t0, 0.1), (CLICK_MOVE_PX, 0.0)), Action::None);
        // 略小于阈值：判（斜向距离 5px < 6）
        s.press(at(t0, 1.0), (0.0, 0.0));
        assert_eq!(s.release(at(t0, 1.1), (3.0, 4.0)), Action::Click);
    }

    #[test]
    fn double_click_suppresses_release() {
        let mut s = InputState::default();
        let t0 = Instant::now();
        s.press(t0, (8.0, 8.0));
        assert_eq!(s.release(at(t0, 0.1), (8.0, 8.0)), Action::Click);
        // 距第一次按下 0.39s < 0.4：双击（基准是上次按下，不是松开）
        assert_eq!(s.press(at(t0, 0.39), (8.0, 8.0)), Action::DoubleClick);
        // 双击后的松开即使短按未位移也不判单击
        assert_eq!(s.release(at(t0, 0.45), (8.0, 8.0)), Action::None);
    }

    #[test]
    fn double_click_window_boundary() {
        let mut s = InputState::default();
        let t0 = Instant::now();
        s.press(t0, (0.0, 0.0));
        s.release(at(t0, 0.1), (0.0, 0.0));
        // 恰好 0.4s：严格小于才判双击，边界为普通按下
        assert_eq!(
            s.press(at(t0, DOUBLE_CLICK_SECS), (1.0, 1.0)),
            Action::DragStart { offset: (1.0, 1.0) }
        );
    }

    #[test]
    fn third_click_of_triple_is_plain_press() {
        let mut s = InputState::default();
        let t0 = Instant::now();
        s.press(t0, (0.0, 0.0));
        assert_eq!(s.press(at(t0, 0.2), (0.0, 0.0)), Action::DoubleClick);
        // 双击已清空基准：第三次按下（仍在 0.4s 内）按普通按下处理
        assert_eq!(
            s.press(at(t0, 0.3), (2.0, 2.0)),
            Action::DragStart { offset: (2.0, 2.0) }
        );
    }

    #[test]
    fn release_without_press_is_safe() {
        let mut s = InputState::default();
        let t0 = Instant::now();
        assert_eq!(s.release(t0, (0.0, 0.0)), Action::None);
        // 重复松开（无中间按下）同样安全
        assert_eq!(s.release(at(t0, 0.1), (1.0, 1.0)), Action::None);
    }

    #[test]
    fn distance_is_euclidean() {
        assert!((dist((0.0, 0.0), (3.0, 4.0)) - 5.0).abs() < 1e-9);
        assert!((dist((1.0, 1.0), (1.0, 1.0)) - 0.0).abs() < f64::EPSILON);
    }
}
