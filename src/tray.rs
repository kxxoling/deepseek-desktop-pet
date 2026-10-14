//! 托盘图标

use tray_icon::Icon;

use crate::library::{FRAME_H, FRAME_W};

/// idle 首帧中心裁方 + box 平均降采样为托盘图标
pub fn tray_icon_from_frame(frame: &[u8]) -> Icon {
    const OUT: u32 = 32;
    // 中心正方形裁剪（320×180 -> 180×180）
    let side = FRAME_H;
    let crop_x0 = ((FRAME_W - side) / 2) as usize;
    let ratio = f64::from(side) / f64::from(OUT);

    let mut rgba = vec![0u8; (OUT * OUT * 4) as usize];
    for oy in 0..OUT as usize {
        for ox in 0..OUT as usize {
            let sx0 = (ox as f64 * ratio).floor() as usize;
            let sx1 = (((ox + 1) as f64 * ratio).ceil() as usize)
                .min(side as usize)
                .max(sx0 + 1);
            let sy0 = (oy as f64 * ratio).floor() as usize;
            let sy1 = (((oy + 1) as f64 * ratio).ceil() as usize)
                .min(side as usize)
                .max(sy0 + 1);
            let (mut sum_r, mut sum_g, mut sum_b, mut sum_a, mut sample_count) =
                (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = ((sy * FRAME_W as usize) + (crop_x0 + sx)) * 4;
                    sum_r += u32::from(frame[i]);
                    sum_g += u32::from(frame[i + 1]);
                    sum_b += u32::from(frame[i + 2]);
                    sum_a += u32::from(frame[i + 3]);
                    sample_count += 1;
                }
            }
            let dst_off = (oy * OUT as usize + ox) * 4;
            rgba[dst_off] = (sum_r / sample_count) as u8;
            rgba[dst_off + 1] = (sum_g / sample_count) as u8;
            rgba[dst_off + 2] = (sum_b / sample_count) as u8;
            rgba[dst_off + 3] = (sum_a / sample_count) as u8;
        }
    }
    Icon::from_rgba(rgba, OUT, OUT).expect("尺寸恒为 32×32")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 图标生成() {
        // 全透明帧
        let _icon1 = tray_icon_from_frame(&vec![0u8; crate::library::FRAME_BYTES]);
        // 含不透明像素的帧
        let mut frame = vec![0u8; crate::library::FRAME_BYTES];
        let w = crate::library::FRAME_W as usize;
        for y in 60..120 {
            for x in 100..220 {
                let i = (y * w + x) * 4;
                frame[i] = 200;
                frame[i + 1] = 100;
                frame[i + 2] = 50;
                frame[i + 3] = 255;
            }
        }
        let _icon2 = tray_icon_from_frame(&frame);
    }
}
