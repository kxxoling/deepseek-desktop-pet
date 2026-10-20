//! 屏幕上宠物的运动与碰撞，纯计算，不依赖窗口或图形库。坐标为屏幕逻辑
//! 坐标（左上原点，y 轴向下），速度单位是 px/s，时间步长以秒计。速度太快
//! 时一帧穿过对方的「隧穿」问题暂未处理。

#![allow(dead_code)]

/// 重力加速度（px/s²，y 向下）
pub const GRAVITY: f64 = 2200.0;
/// 反弹保留系数（地面/墙壁/宠物间一致）
pub const RESTITUTION: f64 = 0.45;
/// 触地反弹后水平速度的摩擦保留系数
pub const GROUND_FRICTION: f64 = 0.75;
/// 触地时速度低于这个值就视为停稳，从飞行转为静止
pub const SETTLE_SPEED: f64 = 30.0;

/// 世界边界（屏幕逻辑尺寸）
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub width: f64,
    pub height: f64,
}

/// 身体在帧内（320×180）的覆盖范围，矩形近似；默认值取自上游标定数据换算，
/// 多角色时按角色注入。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl BodyBox {
    /// 上游标定 [212,60,428,330]（640×360 坐标）换算到 320×180
    pub const DEFAULT: BodyBox = BodyBox {
        x0: 106.0,
        y0: 30.0,
        x1: 214.0,
        y1: 165.0,
    };

    /// 中心点（帧内坐标），旋转等姿态渲染用
    pub fn center(&self) -> (f64, f64) {
        (self.x0.midpoint(self.x1), self.y0.midpoint(self.y1))
    }
}

/// 宠物运动状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PetState {
    /// 静止：待机、行走等状态，不受重力，速度保持为零
    Resting,
    /// 拖拽中：无限重，只挡不弹，位置由外部设置
    Dragging,
    /// 飞行中：受重力和反弹，低速触地后转为静止
    Flying,
}

/// 一只宠物在物理世界里的数据
#[derive(Debug, Clone)]
pub struct PetBody {
    /// 窗口左上角位置（逻辑点）
    pub pos: (f64, f64),
    /// 速度（px/s）
    pub vel: (f64, f64),
    pub state: PetState,
    /// 身体框的帧内偏移，屏幕上的矩形就是 pos 加上它
    pub body: BodyBox,
}

impl PetBody {
    pub fn new(pos: (f64, f64)) -> Self {
        Self {
            pos,
            vel: (0.0, 0.0),
            state: PetState::Resting,
            body: BodyBox::DEFAULT,
        }
    }

    pub fn flying(pos: (f64, f64), vel: (f64, f64)) -> Self {
        Self {
            pos,
            vel,
            state: PetState::Flying,
            body: BodyBox::DEFAULT,
        }
    }

    pub fn dragging(pos: (f64, f64)) -> Self {
        Self {
            pos,
            vel: (0.0, 0.0),
            state: PetState::Dragging,
            body: BodyBox::DEFAULT,
        }
    }

    /// 身体在屏幕上的矩形
    fn rect(&self) -> Rect {
        Rect {
            x0: self.pos.0 + self.body.x0,
            y0: self.pos.1 + self.body.y0,
            x1: self.pos.0 + self.body.x1,
            y1: self.pos.1 + self.body.y1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// 两个矩形的重叠量，分 x、y 两个方向；小于等于零表示不相交
fn overlap(a: Rect, b: Rect) -> (f64, f64) {
    (
        a.x1.min(b.x1) - a.x0.max(b.x0),
        a.y1.min(b.y1) - a.y0.max(b.y0),
    )
}

/// 每次小步计算允许的最大位移（半个身位）；一帧的位移超过它就把这帧拆成多个小步，高速或掉帧时不会穿透
pub const MAX_STEP_DIST: f64 = 54.0;
/// 甩出力度倍率范围（config `throw_strength`）
pub const THROW_STRENGTH_MIN: f64 = 0.25;
pub const THROW_STRENGTH_MAX: f64 = 3.0;

/// 弹弓发射的最大速度（px/s）
pub const SLINGSHOT_MAX_SPEED: f64 = 2600.0;
/// 弹弓拉距到速度的换算（px 拉距 × 系数 = px/s）
pub const SLINGSHOT_GAIN: f64 = 8.0;

/// 弹弓发射向量：从拉住点指向宠物中心的方向发射（反向拉），速度随拉距线性增长并封顶。
/// `pet_center` 为宠物身体中心的全局坐标，`pull_pos` 为松手时光标位置
pub fn slingshot_launch(pet_center: (f64, f64), pull_pos: (f64, f64)) -> (f64, f64) {
    let dx = pet_center.0 - pull_pos.0;
    let dy = pet_center.1 - pull_pos.1;
    let dist = dx.hypot(dy);
    if dist < f64::EPSILON {
        return (0.0, 0.0);
    }
    let speed = (dist * SLINGSHOT_GAIN).min(SLINGSHOT_MAX_SPEED);
    (dx / dist * speed, dy / dist * speed)
}

pub fn clamp_throw_strength(v: f64) -> f64 {
    v.clamp(THROW_STRENGTH_MIN, THROW_STRENGTH_MAX)
}

pub struct World {
    pub pets: Vec<PetBody>,
    pub bounds: Bounds,
}

impl World {
    pub fn new(bounds: Bounds) -> Self {
        Self {
            pets: Vec::new(),
            bounds,
        }
    }

    /// 推进一帧：积分、碰撞、越界钳制。相同的输入一定得到相同的输出，没有随机。
    /// 内部按最大速度把一帧拆成多个小步计算，防止高速时一步跨过别的宠物造成穿透
    pub fn step(&mut self, dt: f64) {
        let max_speed = self
            .pets
            .iter()
            .map(|p| p.vel.0.hypot(p.vel.1))
            .fold(0.0, f64::max);
        let steps = ((max_speed * dt / MAX_STEP_DIST).ceil() as usize).clamp(1, 8);
        let sub = dt / steps as f64;
        for _ in 0..steps {
            self.step_inner(sub);
        }
    }

    fn step_inner(&mut self, dt: f64) {
        for p in &mut self.pets {
            match p.state {
                PetState::Resting => p.vel = (0.0, 0.0),
                PetState::Dragging => {}
                PetState::Flying => {
                    // 半隐式欧拉：先加速后位移
                    p.vel.1 += GRAVITY * dt;
                    p.pos.0 += p.vel.0 * dt;
                    p.pos.1 += p.vel.1 * dt;
                }
            }
        }
        for i in 0..self.pets.len() {
            for j in (i + 1)..self.pets.len() {
                let (l, r) = self.pets.split_at_mut(j);
                collide_pair(&mut l[i], &mut r[0]);
            }
        }
        for p in &mut self.pets {
            if p.state == PetState::Flying {
                clamp_to_bounds(p, self.bounds);
            }
        }
    }
}

/// 宠物间碰撞：沿重叠较小的方向分离并交换速度；拖拽中的宠物不动。
fn collide_pair(a: &mut PetBody, b: &mut PetBody) {
    let (ox, oy) = overlap(a.rect(), b.rect());
    if ox <= 0.0 || oy <= 0.0 {
        return;
    }
    // 沿穿透更小的方向分开；两只完全重合时固定往 +x 分，保证结果确定
    let (x_axis, pen) = if ox <= oy { (true, ox) } else { (false, oy) };
    let diff = if x_axis {
        b.pos.0 - a.pos.0
    } else {
        b.pos.1 - a.pos.1
    };
    let sign = if diff < 0.0 { -1.0 } else { 1.0 };

    // 位置分离按质量反比，拖拽视为无限重
    let ia = f64::from(!matches!(a.state, PetState::Dragging));
    let ib = f64::from(!matches!(b.state, PetState::Dragging));
    let total = ia + ib;
    if total == 0.0 {
        return; // 两只都在拖拽中：不需要处理
    }
    let sep = pen * sign / total;
    if x_axis {
        a.pos.0 -= sep * ia;
        b.pos.0 += sep * ib;
    } else {
        a.pos.1 -= sep * ia;
        b.pos.1 += sep * ib;
    }

    // 速度变化按各自的逆质量缩放，拖拽方不动
    let (va, vb) = if x_axis {
        (a.vel.0, b.vel.0)
    } else {
        (a.vel.1, b.vel.1)
    };
    let closing = (va - vb) * sign; // 大于零表示正在互相接近
    if closing > 0.0 {
        let j = (1.0 + RESTITUTION) * closing / total;
        let (da, db) = (j * ia, j * ib);
        if x_axis {
            a.vel.0 -= sign * da;
            b.vel.0 += sign * db;
        } else {
            a.vel.1 -= sign * da;
            b.vel.1 += sign * db;
        }
    }

    // 被撞出速度的静止宠物转为飞行，从下一帧开始受重力
    for p in [&mut *a, &mut *b] {
        if p.state == PetState::Resting
            && (p.vel.0 * p.vel.0 + p.vel.1 * p.vel.1) > SETTLE_SPEED * SETTLE_SPEED
        {
            p.state = PetState::Flying;
        }
    }
}

/// 越界钳制与反弹，低速触地转为静止；只作用于飞行中的宠物。
fn clamp_to_bounds(p: &mut PetBody, bounds: Bounds) {
    let r = p.rect();
    if r.x0 < 0.0 {
        p.pos.0 -= r.x0;
        p.vel.0 = -p.vel.0 * RESTITUTION;
    }
    if r.x1 > bounds.width {
        p.pos.0 -= r.x1 - bounds.width;
        p.vel.0 = -p.vel.0 * RESTITUTION;
    }
    if r.y0 < 0.0 {
        p.pos.1 -= r.y0;
        p.vel.1 = -p.vel.1 * RESTITUTION;
    }
    if r.y1 > bounds.height {
        // 地面
        p.pos.1 -= r.y1 - bounds.height;
        let speed = (p.vel.0 * p.vel.0 + p.vel.1 * p.vel.1).sqrt();
        if speed <= SETTLE_SPEED {
            p.vel = (0.0, 0.0);
            p.state = PetState::Resting;
        } else {
            p.vel.1 = -p.vel.1 * RESTITUTION;
            p.vel.0 *= GROUND_FRICTION;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const BOUNDS: Bounds = Bounds {
        width: 1920.0,
        height: 1080.0,
    };

    pub(super) fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn default_body_box_from_upstream_calibration() {
        // 上游标定的 [212,60,428,330]（640×360 坐标）乘 0.5 换算而来
        assert_eq!(
            BodyBox::DEFAULT,
            BodyBox {
                x0: 212.0 * 0.5,
                y0: 60.0 * 0.5,
                x1: 428.0 * 0.5,
                y1: 330.0 * 0.5,
            }
        );
    }

    #[test]
    fn resting_pet_with_zero_speed_unchanged() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::new((500.0, 400.0)));
        w.step(1.0);
        w.step(0.016);
        assert_eq!(w.pets[0].pos, (500.0, 400.0));
        assert_eq!(w.pets[0].vel, (0.0, 0.0));
        assert_eq!(w.pets[0].state, PetState::Resting);
    }

    #[test]
    fn flying_pet_parabola_semi_implicit_euler() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::flying((100.0, 100.0), (60.0, 0.0)));
        let dt = 0.1;
        w.step(dt);
        approx(w.pets[0].vel.1, GRAVITY * dt);
        approx(w.pets[0].pos.0, 100.0 + 60.0 * dt);
        approx(w.pets[0].pos.1, 100.0 + GRAVITY * dt * dt);
    }

    #[test]
    fn ground_bounce_decay_and_settle() {
        // 底部越界且速度高：反弹并衰减
        let mut w = World::new(Bounds {
            width: 1000.0,
            height: 300.0,
        });
        // 身体底边越过地面
        w.pets.push(PetBody::flying((100.0, 140.0), (200.0, 100.0)));
        w.step(0.0); // 时间步长为零，只处理越界
        approx(w.pets[0].pos.1, 300.0 - BodyBox::DEFAULT.y1);
        approx(w.pets[0].vel.0, 200.0 * GROUND_FRICTION);
        approx(w.pets[0].vel.1, -100.0 * RESTITUTION);
        assert_eq!(w.pets[0].state, PetState::Flying);

        // 低速触地时停稳
        let mut w2 = World::new(Bounds {
            width: 1000.0,
            height: 300.0,
        });
        w2.pets.push(PetBody::flying((100.0, 140.0), (10.0, 20.0)));
        w2.step(0.0);
        approx(w2.pets[0].pos.1, 300.0 - BodyBox::DEFAULT.y1);
        assert_eq!(w2.pets[0].vel, (0.0, 0.0));
        assert_eq!(w2.pets[0].state, PetState::Resting);
    }

    #[test]
    fn wall_and_ceiling_bounce() {
        let mut w = World::new(Bounds {
            width: 1000.0,
            height: 1080.0,
        });
        // 左墙：横坐标取一个让身体左边缘越界的值
        w.pets.push(PetBody::flying((-120.0, 100.0), (-200.0, 0.0)));
        w.step(0.0);
        approx(w.pets[0].pos.0, -BodyBox::DEFAULT.x0);
        approx(w.pets[0].vel.0, 200.0 * RESTITUTION);

        // 右墙
        let mut w2 = World::new(Bounds {
            width: 1000.0,
            height: 1080.0,
        });
        w2.pets.push(PetBody::flying((900.0, 100.0), (200.0, 0.0)));
        w2.step(0.0);
        approx(w2.pets[0].pos.0, 1000.0 - BodyBox::DEFAULT.x1);
        approx(w2.pets[0].vel.0, -200.0 * RESTITUTION);

        // 天花板
        let mut w3 = World::new(Bounds {
            width: 1000.0,
            height: 1080.0,
        });
        w3.pets.push(PetBody::flying((100.0, -40.0), (0.0, -500.0)));
        w3.step(0.0);
        approx(w3.pets[0].pos.1, -BodyBox::DEFAULT.y0);
        approx(w3.pets[0].vel.1, 500.0 * RESTITUTION);
    }

    #[test]
    fn fall_eventually_settles_on_ground() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::flying((100.0, 0.0), (50.0, 0.0)));
        let mut rested = false;
        for _ in 0..2000 {
            w.step(0.016);
            if w.pets[0].state == PetState::Resting {
                rested = true;
                break;
            }
        }
        assert!(rested, "应该在有限的步数内停稳");
        approx(w.pets[0].pos.1 + w.pets[0].body.y1, BOUNDS.height);
        assert_eq!(w.pets[0].vel, (0.0, 0.0));
    }

    #[test]
    fn non_overlapping_pets_untouched() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::new((100.0, 100.0)));
        w.pets.push(PetBody::new((600.0, 100.0)));
        w.step(0.016);
        assert_eq!(w.pets[0].pos, (100.0, 100.0));
        assert_eq!(w.pets[1].pos, (600.0, 100.0));
    }

    #[test]
    fn coincident_pets_separate_deterministically() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::new((100.0, 100.0)));
        w.pets.push(PetBody::new((100.0, 100.0)));
        w.step(0.0);
        let (a, b) = (&w.pets[0], &w.pets[1]);
        assert!(a.pos.0.is_finite() && b.pos.0.is_finite());
        // 身体宽度是较小的重叠方向，沿 x 分离；中心重合时固定往 +x 分开
        let body_w = BodyBox::DEFAULT.x1 - BodyBox::DEFAULT.x0;
        approx(a.pos.0, 100.0 - body_w / 2.0);
        approx(b.pos.0, 100.0 + body_w / 2.0);
        assert_eq!(a.state, PetState::Resting);
        assert_eq!(b.state, PetState::Resting);
    }

    #[test]
    fn overlapping_resting_pets_separated() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::new((100.0, 100.0)));
        w.pets.push(PetBody::new((150.0, 100.0)));
        w.step(0.0);
        let overlap_x = (BodyBox::DEFAULT.x1 - BodyBox::DEFAULT.x0) - 50.0;
        approx(w.pets[0].pos.0, 100.0 - overlap_x / 2.0);
        approx(w.pets[1].pos.0, 150.0 + overlap_x / 2.0);
        let (ox, _) = overlap(w.pets[0].rect(), w.pets[1].rect());
        approx(ox, 0.0);
        assert_eq!(w.pets[0].vel, (0.0, 0.0));
        assert_eq!(w.pets[1].vel, (0.0, 0.0));
    }

    #[test]
    fn flying_hits_resting_momentum_transfer() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::flying((0.0, 100.0), (200.0, 0.0)));
        w.pets.push(PetBody::new((50.0, 100.0)));
        w.step(0.0); // 时间步长为零，只处理碰撞
        let (a, b) = (&w.pets[0], &w.pets[1]);
        // 等质量时双方各承担一半冲量
        let j = (1.0 + RESTITUTION) * 200.0 / 2.0;
        approx(a.vel.0, 200.0 - j);
        approx(b.vel.0, j);
        assert_eq!(a.state, PetState::Flying);
        assert_eq!(b.state, PetState::Flying); // 被撞出速度后转为飞行
        assert_eq!(a.vel.1, 0.0);
    }

    #[test]
    fn vertical_collision_uses_y_axis() {
        let mut w = World::new(BOUNDS);
        // x 方向完全重叠、y 方向重叠 75，沿 y 分离；上方留足空间，避免分离后撞到天花板
        w.pets.push(PetBody::flying((100.0, 400.0), (0.0, 300.0)));
        w.pets.push(PetBody::new((100.0, 460.0)));
        w.step(0.0);
        let (a, b) = (&w.pets[0], &w.pets[1]);
        let j = (1.0 + RESTITUTION) * 300.0 / 2.0;
        approx(a.vel.1, 300.0 - j);
        approx(b.vel.1, j);
        assert_eq!(b.state, PetState::Flying);
    }

    #[test]
    fn dragging_pet_blocks_without_bouncing() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::flying((0.0, 100.0), (200.0, 0.0)));
        w.pets.push(PetBody::dragging((50.0, 100.0)));
        w.step(0.0);
        let (a, d) = (&w.pets[0], &w.pets[1]);
        // 被撞后，被拖拽的宠物位置、速度、状态都保持不变
        assert_eq!(d.pos, (50.0, 100.0));
        assert_eq!(d.vel, (0.0, 0.0));
        assert_eq!(d.state, PetState::Dragging);
        // 撞击方全额分离 + 反射：v' = 200 - 1.45*200 = -90
        approx(a.vel.0, -90.0);
        let (ox, _) = overlap(a.rect(), d.rect());
        approx(ox, 0.0);
    }

    #[test]
    fn two_dragging_pets_noop() {
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::dragging((100.0, 100.0)));
        w.pets.push(PetBody::dragging((110.0, 100.0)));
        w.step(0.0);
        assert_eq!(w.pets[0].pos, (100.0, 100.0));
        assert_eq!(w.pets[1].pos, (110.0, 100.0));
    }
}

#[cfg(test)]
mod throw_tests {
    use super::tests::{BOUNDS, approx};
    use super::*;

    #[test]
    fn clamp_throw_strength_bounds() {
        assert_eq!(clamp_throw_strength(0.01), THROW_STRENGTH_MIN);
        assert_eq!(clamp_throw_strength(99.0), THROW_STRENGTH_MAX);
        assert_eq!(clamp_throw_strength(1.5), 1.5);
    }

    #[test]
    fn high_speed_pet_does_not_tunnel_through_resting() {
        // 一帧位移 200pt，超过身宽 108；不拆成小步会一步跨过静止的宠物
        let mut w = World::new(BOUNDS);
        let gap = 300.0;
        w.pets.push(PetBody::flying((0.0, 100.0), (6000.0, 0.0)));
        w.pets.push(PetBody::new((gap, 100.0)));
        w.step(1.0 / 30.0);
        // 被撞到的宠物应脱离静止（得到速度），说明碰撞发生了
        assert_eq!(w.pets[1].state, PetState::Flying);
        assert!(w.pets[1].vel.0 > 0.0);
    }

    #[test]
    fn substep_matches_single_step_at_low_speed() {
        // 低速时不拆分（steps 为 1），行为和整帧直接计算一致
        let mut w = World::new(BOUNDS);
        w.pets.push(PetBody::flying((100.0, 100.0), (60.0, 0.0)));
        let dt = 0.1;
        let steps = ((60.0f64.hypot(0.0) * dt / MAX_STEP_DIST).ceil() as usize).clamp(1, 8);
        assert_eq!(steps, 1);
        w.step(dt);
        approx(w.pets[0].pos.0, 100.0 + 60.0 * dt);
    }
}

#[cfg(test)]
mod slingshot_tests {
    use super::tests::BOUNDS;
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn launches_opposite_to_pull() {
        // 宠物在 (500, 300)，从 (300, 300) 向左拉 200pt：向右发射
        let (vx, vy) = slingshot_launch((500.0, 300.0), (300.0, 300.0));
        approx(vx, 200.0 * SLINGSHOT_GAIN);
        approx(vy, 0.0);
        // 斜向拉：方向沿拉向量的反向
        let (vx, vy) = slingshot_launch((500.0, 300.0), (400.0, 400.0));
        approx(
            vx.hypot(vy),
            100.0 * std::f64::consts::SQRT_2 * SLINGSHOT_GAIN,
        );
        assert!(vx > 0.0 && vy < 0.0);
    }

    #[test]
    fn speed_caps_and_zero_pull() {
        // 拉距极大时封顶
        let (vx, vy) = slingshot_launch((0.0, 0.0), (-1e6, 0.0));
        approx(vx, SLINGSHOT_MAX_SPEED);
        approx(vy, 0.0);
        // 拉点与宠物重合时不发射
        assert_eq!(slingshot_launch((5.0, 5.0), (5.0, 5.0)), (0.0, 0.0));
    }

    #[test]
    fn half_screen_pull_reaches_cap() {
        // 半屏拉距（750pt × 8 = 6000）超过封顶，发射速度即为封顶值
        let (vx, _) = slingshot_launch((0.0, 0.0), (-750.0, 0.0));
        approx(vx, SLINGSHOT_MAX_SPEED);
        let _ = BOUNDS;
    }
}
