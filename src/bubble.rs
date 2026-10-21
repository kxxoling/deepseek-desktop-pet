//! 气泡窗口：在宠物上方显示一条文字消息，超时自动关闭。
//! 与宠物窗口共用同一个 `EventLoop` 和 `Gpu` 上下文（硬性事实 1）。

/// 气泡显示时长（秒）
#[allow(dead_code)]
pub const BUBBLE_DURATION_SECS: f64 = 5.0;
/// 气泡最大宽度（逻辑点）
#[allow(dead_code)]
pub const BUBBLE_MAX_WIDTH: f64 = 280.0;
/// 气泡内边距（逻辑点）
#[allow(dead_code)]
pub const BUBBLE_PADDING: f64 = 12.0;

/// 计算气泡窗口的位置：默认在宠物上方水平居中，上方空间不够翻到下方
#[allow(dead_code)]
pub fn bubble_position(
    pet_pos: (f64, f64),
    pet_size: (f64, f64),
    bubble_size: (f64, f64),
) -> (f64, f64) {
    let gap = 8.0;
    let x = pet_pos.0 + (pet_size.0 - bubble_size.0) / 2.0;
    let y = if pet_pos.1 - bubble_size.1 - gap >= 0.0 {
        pet_pos.1 - bubble_size.1 - gap
    } else {
        pet_pos.1 + pet_size.1 + gap
    };
    (x.max(0.0), y.max(0.0))
}

/// 计算气泡窗口的尺寸：根据换行后的行数和字号
#[allow(dead_code)]
pub fn bubble_size(line_count: usize, font_size: f64) -> (f64, f64) {
    let line_height = font_size * 1.3;
    (
        BUBBLE_MAX_WIDTH,
        line_count as f64 * line_height + BUBBLE_PADDING * 2.0,
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bubble_above_pet_when_space_available() {
        let (x, y) = bubble_position((100.0, 200.0), (320.0, 180.0), (280.0, 60.0));
        assert_eq!(y, 200.0 - 60.0 - 8.0);
        assert_eq!(x, 100.0 + (320.0 - 280.0) / 2.0);
    }

    #[test]
    fn bubble_below_pet_when_no_space_above() {
        let (_, y) = bubble_position((100.0, 20.0), (320.0, 180.0), (280.0, 60.0));
        assert_eq!(y, 20.0 + 180.0 + 8.0);
    }

    #[test]
    fn bubble_position_never_negative() {
        let (x, y) = bubble_position((0.0, 100.0), (100.0, 100.0), (280.0, 50.0));
        assert!(x >= 0.0 && y >= 0.0);
    }

    #[test]
    fn bubble_size_scales_with_lines() {
        let (w, h) = bubble_size(1, 16.0);
        assert_eq!(w, BUBBLE_MAX_WIDTH);
        assert!((h - (16.0 * 1.3 + 24.0)).abs() < 0.01);
    }
}
