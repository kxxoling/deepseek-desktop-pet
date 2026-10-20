//! 位置记忆：保存和恢复宠物的窗口位置，按显示器指纹找到目标屏幕。
//! 坐标以显示器内逻辑坐标保存（原点在该显示器的左上角），显示器布局变化后数据仍然有效。

/// 单只宠物的存档位置
#[derive(Debug, Clone, PartialEq)]
pub struct PetPos {
    /// 显示器指纹（见 [`monitor_fingerprint`]）
    pub monitor: String,
    /// 显示器内逻辑坐标
    pub x: f64,
    pub y: f64,
}

/// 显示器指纹：优先用名称（各平台通常稳定），取不到名称时用物理尺寸。
/// 同尺寸的双屏会共用指纹，恢复时取第一个匹配，总比丢掉位置好。
pub fn monitor_fingerprint(name: Option<&str>, size_phys: (u32, u32)) -> String {
    match name {
        Some(n) if !n.is_empty() => format!("name:{n}"),
        _ => format!("size:{}x{}", size_phys.0, size_phys.1),
    }
}

/// 序列化为 config 里的 JSON 数组
pub fn to_value(positions: &[PetPos]) -> serde_json::Value {
    serde_json::Value::Array(
        positions
            .iter()
            .map(|p| {
                serde_json::json!({
                    "monitor": p.monitor,
                    "x": p.x,
                    "y": p.y,
                })
            })
            .collect(),
    )
}

/// 从 config 值解析；单个条目损坏（缺字段或不是对象）时跳过它，不影响其余条目
pub fn from_value(v: &serde_json::Value) -> Vec<PetPos> {
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            Some(PetPos {
                monitor: obj.get("monitor")?.as_str()?.to_string(),
                x: obj.get("x")?.as_f64()?,
                y: obj.get("y")?.as_f64()?,
            })
        })
        .collect()
}

/// 恢复时的落位决策：返回匹配指纹的显示器下标；目标显示器不在线时先回退主屏，并标记等待它上线。
/// 返回 (显示器下标, 是否等待该显示器上线)
pub fn resolve(pos: &PetPos, online: &[String], primary: usize) -> (usize, bool) {
    match online.iter().position(|m| m == &pos.monitor) {
        Some(i) => (i, false),
        None => (primary, true),
    }
}

/// 把显示器内坐标换算成全局逻辑坐标，并夹在该显示器的可视范围内
pub fn to_global(
    origin: (f64, f64),
    size: (f64, f64),
    x: f64,
    y: f64,
    win: (f64, f64),
) -> (f64, f64) {
    (
        (origin.0 + x).clamp(origin.0, (origin.0 + size.0 - win.0).max(origin.0)),
        (origin.1 + y).clamp(origin.1, (origin.1 + size.1 - win.1).max(origin.1)),
    )
}

/// 目标显示器还没上线的宠物，临时坐标不写入存档：按等待列表过滤快照
pub fn filter_saved<T: PartialEq + Copy>(snapshot: &[(T, PetPos)], waiting: &[T]) -> Vec<PetPos> {
    snapshot
        .iter()
        .filter(|(id, _)| !waiting.contains(id))
        .map(|(_, p)| p.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(monitor: &str, x: f64, y: f64) -> PetPos {
        PetPos {
            monitor: monitor.to_string(),
            x,
            y,
        }
    }

    #[test]
    fn value_roundtrip() {
        let saved = vec![
            pos("name:LED", 12.5, 30.0),
            pos("size:1920x1080", 0.0, 900.0),
        ];
        assert_eq!(from_value(&to_value(&saved)), saved);
        assert_eq!(from_value(&serde_json::Value::Null), Vec::<PetPos>::new());
    }

    #[test]
    fn from_value_skips_bad_entries() {
        let v = serde_json::json!([
            {"monitor": "name:A", "x": 1.0, "y": 2.0},
            {"monitor": "缺坐标"},
            "不是对象",
            {"monitor": "name:B", "x": 3.0, "y": 4.0},
        ]);
        assert_eq!(
            from_value(&v),
            vec![pos("name:A", 1.0, 2.0), pos("name:B", 3.0, 4.0)]
        );
    }

    #[test]
    fn fingerprint_prefers_name() {
        assert_eq!(
            monitor_fingerprint(Some("DELL U2723"), (2560, 1440)),
            "name:DELL U2723"
        );
        assert_eq!(
            monitor_fingerprint(Some(""), (2560, 1440)),
            "size:2560x1440"
        );
        assert_eq!(monitor_fingerprint(None, (1920, 1080)), "size:1920x1080");
    }

    #[test]
    fn resolve_matches_or_falls_back_to_primary() {
        let online = vec!["name:A".to_string(), "name:B".to_string()];
        assert_eq!(resolve(&pos("name:B", 0.0, 0.0), &online, 0), (1, false));
        // 目标不在线：回退主屏并等待
        assert_eq!(resolve(&pos("name:C", 0.0, 0.0), &online, 0), (0, true));
        // 空显示器列表同样回退主屏
        assert_eq!(resolve(&pos("name:A", 0.0, 0.0), &[], 0), (0, true));
    }

    #[test]
    fn to_global_clamps_into_monitor() {
        // 常规换算
        assert_eq!(
            to_global((100.0, 50.0), (1512.0, 982.0), 200.0, 300.0, (320.0, 180.0)),
            (300.0, 350.0)
        );
        // 超出右下界被夹回（窗口完全留在屏内）
        assert_eq!(
            to_global((0.0, 0.0), (1512.0, 982.0), 5000.0, 5000.0, (320.0, 180.0)),
            (1512.0 - 320.0, 982.0 - 180.0)
        );
        // 比窗口还小的显示器：夹到原点，不会出现负坐标
        assert_eq!(
            to_global((0.0, 0.0), (200.0, 100.0), 10.0, 10.0, (320.0, 180.0)),
            (0.0, 0.0)
        );
    }

    #[test]
    fn filter_saved_drops_waiting_pets() {
        let snap = vec![
            (1u64, pos("name:A", 1.0, 1.0)),
            (2u64, pos("name:B", 2.0, 2.0)),
        ];
        assert_eq!(filter_saved(&snap, &[2u64]), vec![pos("name:A", 1.0, 1.0)]);
        assert_eq!(
            filter_saved(&snap, &[]),
            vec![pos("name:A", 1.0, 1.0), pos("name:B", 2.0, 2.0)]
        );
    }
}
