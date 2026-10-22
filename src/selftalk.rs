//! 自言自语：宠物待机时定时说一句话，配着气泡显示。有对应动画的台词同时播放动画。
//! 台词可以用 config `self_talk_lines` 自定义，不配就用默认的。

/// 默认台词；前面几条有对应的动画片段，说到时同时播放
pub const BUILTIN_LINES: &[&str] = &[
    "写代码",
    "深度思考碎碎念",
    "是啊，吃什么",
    "写福字",
    "收红包",
    "吃Token",
    "今天天气不错",
    "有点困了",
    "摸鱼中……",
    "你在看我吗？",
];

/// 有动画的台词名单（来自 `text_clips.json`，这些动画里有可读文字所以不做镜像）
pub const TEXT_CLIP_NAMES: &[&str] = &[
    "是啊，吃什么",
    "写代码",
    "写福字",
    "收红包",
    "吃Token",
    "深度思考碎碎念",
];

/// 自言自语最小间隔（秒），config `self_talk_min_secs` 可调
pub const SELF_TALK_MIN_SECS: f64 = 20.0;
/// 自言自语最大间隔（秒），config `self_talk_max_secs` 可调
pub const SELF_TALK_MAX_SECS: f64 = 60.0;

/// 从台词列表里随机选一条，不会连续两次说同一句；列表只有一条时就没得选了
pub fn pick_line(lines: &[String], last: Option<&str>, roll: f64) -> String {
    if lines.is_empty() {
        return String::new();
    }
    if lines.len() == 1 {
        return lines[0].clone();
    }
    // 排除上一条后随机
    let candidates: Vec<&String> = lines.iter().filter(|l| Some(l.as_str()) != last).collect();
    let idx = (roll * candidates.len() as f64) as usize % candidates.len();
    candidates[idx].clone()
}

/// 台词是否绑定了动画（台词名与片段名精确匹配）
pub fn matching_text_clip(line: &str) -> Option<&'static str> {
    TEXT_CLIP_NAMES.iter().find(|name| **name == line).copied()
}

/// 下次自言自语要等多久（秒），在 `[min, max)` 区间内随机
pub fn next_interval_secs(min_secs: f64, max_secs: f64, roll: f64) -> f64 {
    min_secs + (max_secs - min_secs) * roll.clamp(0.0, 1.0)
}

/// 从 config 读取台词列表，没配就用默认的
/// 根据配置值决定用哪套台词：配了非空列表就用它，没配就用默认的
pub fn lines_from(config_lines: Option<Vec<String>>) -> Vec<String> {
    match config_lines {
        Some(lines) if !lines.is_empty() => lines,
        _ => BUILTIN_LINES.iter().copied().map(str::to_string).collect(),
    }
}

/// 从 config 读取台词列表，没配就用默认的
pub fn lines_from_config() -> Vec<String> {
    lines_from(crate::config::get_str_array("self_talk_lines"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_line_returns_from_list() {
        let lines = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let picked = pick_line(&lines, None, 0.0);
        assert!(lines.contains(&picked));
    }

    #[test]
    fn pick_line_avoids_last() {
        let lines = vec!["a".to_string(), "b".to_string()];
        let picked = pick_line(&lines, Some("a"), 0.0);
        assert_eq!(picked, "b");
        let picked = pick_line(&lines, Some("b"), 0.0);
        assert_eq!(picked, "a");
    }

    #[test]
    fn pick_line_single_item_returns_it() {
        let lines = vec!["only".to_string()];
        assert_eq!(pick_line(&lines, Some("only"), 0.5), "only");
    }

    #[test]
    fn pick_line_empty_returns_empty() {
        assert_eq!(pick_line(&[], None, 0.5), "");
    }

    #[test]
    fn matching_clip_finds_exact() {
        assert_eq!(matching_text_clip("写代码"), Some("写代码"));
        assert_eq!(matching_text_clip("深度思考碎碎念"), Some("深度思考碎碎念"));
    }

    #[test]
    fn matching_clip_returns_none_for_custom() {
        assert_eq!(matching_text_clip("自定义台词"), None);
        assert_eq!(matching_text_clip("写代码中"), None); // 不是精确匹配
    }

    #[test]
    fn next_interval_in_range() {
        let v = next_interval_secs(20.0, 60.0, 0.0);
        assert_eq!(v, 20.0);
        let v = next_interval_secs(20.0, 60.0, 1.0);
        assert_eq!(v, 60.0);
        let v = next_interval_secs(20.0, 60.0, 0.5);
        assert_eq!(v, 40.0);
    }

    #[test]
    fn next_interval_clamps_roll() {
        let v = next_interval_secs(20.0, 60.0, -1.0);
        assert_eq!(v, 20.0);
        let v = next_interval_secs(20.0, 60.0, 2.0);
        assert_eq!(v, 60.0);
    }

    #[test]
    fn lines_from_uses_config_when_provided() {
        let custom = vec!["自定义".to_string()];
        assert_eq!(lines_from(Some(custom.clone())), custom);
        // 空列表回退到默认
        assert_eq!(lines_from(Some(Vec::new())).len(), BUILTIN_LINES.len());
        // 没配也回退
        assert_eq!(lines_from(None).len(), BUILTIN_LINES.len());
    }

    #[test]
    fn builtin_lines_have_animation_matches() {
        // 内置表前几条应绑定了动画（与 TEXT_CLIP_NAMES 精确匹配）
        let matched = BUILTIN_LINES
            .iter()
            .filter(|l| matching_text_clip(l).is_some())
            .count();
        assert!(matched >= 5, "内置表应有至少 5 条绑定动画，实际 {matched}");
    }
}
