//! 角色：从 manifest.json 解析元数据；发现可用角色（内置 + 用户目录下的外部角色）。

use std::path::{Path, PathBuf};

use crate::library::{FRAME_H, FRAME_W};

// ---------- 角色元数据 ----------

/// 帧内（320×180 坐标）包围盒
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

/// 整个画面
const FULL_FRAME: FrameBox = FrameBox {
    x0: 0.0,
    y0: 0.0,
    x1: FRAME_W as f64,
    y1: FRAME_H as f64,
};

/// 头部缺省值取身体框的上缘 45%，口径来自上游
fn head_fallback(body: FrameBox) -> FrameBox {
    FrameBox {
        y1: body.y0 + (body.y1 - body.y0) * 0.45,
        ..body
    }
}

/// 解析 box 字段（上游 640×360 坐标）并换算到 320×180；非法返回 None
fn parse_manifest_box(v: Option<&serde_json::Value>) -> Option<FrameBox> {
    let arr = v?.as_array()?;
    if arr.len() != 4 {
        return None;
    }
    let n = [
        arr[0].as_f64()?,
        arr[1].as_f64()?,
        arr[2].as_f64()?,
        arr[3].as_f64()?,
    ];
    if n[0] >= n[2] || n[1] >= n[3] {
        return None; // 坐标逆序或退化
    }
    Some(FrameBox {
        x0: n[0] * 0.5,
        y0: n[1] * 0.5,
        x1: n[2] * 0.5,
        y1: n[3] * 0.5,
    })
}

/// 角色元数据；范围框以参数形式提供，供需要的功能注入
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterMeta {
    /// 角色标识来自目录名，统一用 `/` 分隔
    pub id: String,
    /// 显示名，取 manifest 的 `display_name` 或 `name` 字段，没有就用 id
    pub display_name: String,
    /// 身体框，320×180 帧内坐标，缺省为整帧
    pub body_box: FrameBox,
    /// 头部框，缺省取 `body_box` 的上缘 45%
    pub head_box: FrameBox,
}

impl CharacterMeta {
    /// 解析 manifest.json；缺失或损坏一律回退默认值
    #[allow(dead_code)]
    pub fn load(meta_dir: &Path, fallback_id: &str) -> Self {
        let mut meta = Self {
            id: fallback_id.to_string(),
            display_name: fallback_id.to_string(),
            body_box: FULL_FRAME,
            head_box: head_fallback(FULL_FRAME),
        };
        let Ok(text) = std::fs::read_to_string(meta_dir.join("manifest.json")) else {
            return meta;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return meta;
        };
        if let Some(name) = v
            .get("display_name")
            .and_then(serde_json::Value::as_str)
            .or_else(|| v.get("name").and_then(serde_json::Value::as_str))
        {
            meta.display_name = name.to_string();
        }
        if let Some(b) = parse_manifest_box(v.get("body_box")) {
            meta.body_box = b;
        }
        // 头部框损坏或缺失时，都回退到已解析身体框的上缘 45%
        meta.head_box =
            parse_manifest_box(v.get("head_box")).unwrap_or_else(|| head_fallback(meta.body_box));
        meta
    }
}

/// 内置角色的标识，目前只有 shenshen 一个
pub const BUILTIN_CHARACTER_ID: &str = "shenshen";

// ---------- 多角色发现 ----------

/// 一个可用角色：资产根加元数据
#[allow(dead_code)]
pub struct Character {
    /// 角色标识来自目录名，统一用 `/` 分隔
    pub id: String,
    /// 该角色的资产根目录
    pub root: PathBuf,
    pub meta: CharacterMeta,
}

/// 外部角色根目录（与配置文件的目录约定一致，其下是各角色的子目录）
#[allow(dead_code)]
pub fn external_characters_root() -> PathBuf {
    platform_characters_root().unwrap_or_else(|| std::env::temp_dir().join("ds-pet-characters"))
}

fn platform_characters_root() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/ds-pet/characters"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(r"ds-pet\characters"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(config.join("ds-pet/characters"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// 发现可用角色：内置角色排第一，外部目录下的角色按名字排序，与内置同名的跳过
#[allow(dead_code)]
pub fn discover_characters(builtin_root: &Path, external_root: &Path) -> Vec<Character> {
    let mut out = Vec::new();
    if builtin_root.join("frames").is_dir() {
        out.push(Character {
            id: BUILTIN_CHARACTER_ID.to_string(),
            root: builtin_root.to_path_buf(),
            meta: CharacterMeta::load(&builtin_root.join("meta"), BUILTIN_CHARACTER_ID),
        });
    }
    let mut subs: Vec<PathBuf> = std::fs::read_dir(external_root)
        .map(|rd| {
            rd.filter_map(std::result::Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    subs.sort();
    for root in subs {
        if !root.join("frames").is_dir() {
            continue;
        }
        let Some(name) = root.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        // 目录名里的反斜杠统一为斜杠
        let id = name.replace('\\', "/");
        if out.iter().any(|c| c.id == id) {
            continue; // 与内置或先到的角色同名，跳过这个
        }
        let meta = CharacterMeta::load(&root.join("meta"), &id);
        out.push(Character { id, root, meta });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ds-pet-char-{tag}-{}-{nanos}", std::process::id()))
    }

    fn write_manifest(meta_dir: &Path, content: &str) {
        std::fs::create_dir_all(meta_dir).unwrap();
        std::fs::write(meta_dir.join("manifest.json"), content).unwrap();
    }

    /// 上游标定的身体范围 [212,60,428,330]（640×360 坐标）乘 0.5 换算
    const BODY: [f64; 4] = [212.0 * 0.5, 60.0 * 0.5, 428.0 * 0.5, 330.0 * 0.5];
    /// 上游标定的头部范围 [185,60,436,195] 乘 0.5 换算
    const HEAD: [f64; 4] = [185.0 * 0.5, 60.0 * 0.5, 436.0 * 0.5, 195.0 * 0.5];
    /// 身体范围上缘 45% 处的头部范围
    const BODY_TOP45: [f64; 4] = [
        BODY[0],
        BODY[1],
        BODY[2],
        BODY[1] + (BODY[3] - BODY[1]) * 0.45,
    ];

    fn box_eq(b: FrameBox, v: [f64; 4]) {
        assert_eq!(
            b,
            FrameBox {
                x0: v[0],
                y0: v[1],
                x1: v[2],
                y1: v[3]
            }
        );
    }

    #[test]
    fn parses_upstream_values_into_frame_coords() {
        let root = temp_root("real");
        write_manifest(
            &root,
            r#"{"body_box":[212,60,428,330],"head_box":[185,60,436,195]}"#,
        );
        let m = CharacterMeta::load(&root, "shenshen");
        assert_eq!(m.id, "shenshen");
        assert_eq!(m.display_name, "shenshen");
        box_eq(m.body_box, BODY);
        box_eq(m.head_box, HEAD);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn display_name_supports_both_field_names() {
        let root = temp_root("name");
        write_manifest(&root, r#"{"name":"深酱"}"#);
        assert_eq!(CharacterMeta::load(&root, "shenshen").display_name, "深酱");
        write_manifest(&root, r#"{"display_name":"小深"}"#);
        assert_eq!(CharacterMeta::load(&root, "shenshen").display_name, "小深");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_manifest_falls_back_to_full_frame() {
        let root = temp_root("missing");
        std::fs::create_dir_all(&root).unwrap();
        let m = CharacterMeta::load(&root, "x");
        box_eq(m.body_box, [0.0, 0.0, 320.0, 180.0]);
        // 头部回退为整帧的上缘 45%
        box_eq(m.head_box, [0.0, 0.0, 320.0, 180.0 * 0.45]);
        assert_eq!(m.display_name, "x");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bad_json_and_fields_fall_back_per_field() {
        let root = temp_root("bad");
        // 整个文件不是合法 JSON 时全部回退默认
        write_manifest(&root, "{oops");
        let m = CharacterMeta::load(&root, "x");
        box_eq(m.body_box, [0.0, 0.0, 320.0, 180.0]);
        // 身体字段是非数组、长度不足或坐标逆序时，各自回退
        write_manifest(&root, r#"{"body_box":"abc"}"#);
        box_eq(
            CharacterMeta::load(&root, "x").body_box,
            [0.0, 0.0, 320.0, 180.0],
        );
        write_manifest(&root, r#"{"body_box":[1,2,3]}"#);
        box_eq(
            CharacterMeta::load(&root, "x").body_box,
            [0.0, 0.0, 320.0, 180.0],
        );
        write_manifest(&root, r#"{"body_box":[4,60,2,330]}"#);
        box_eq(
            CharacterMeta::load(&root, "x").body_box,
            [0.0, 0.0, 320.0, 180.0],
        );
        // 头部字段坏但身体字段好：头部回退到身体范围的上缘 45%
        write_manifest(&root, r#"{"body_box":[212,60,428,330],"head_box":[9]}"#);
        let m = CharacterMeta::load(&root, "x");
        box_eq(m.body_box, BODY);
        box_eq(m.head_box, BODY_TOP45);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_head_falls_back_to_body_top() {
        let root = temp_root("nohead");
        write_manifest(&root, r#"{"body_box":[212,60,428,330]}"#);
        let m = CharacterMeta::load(&root, "x");
        box_eq(m.head_box, BODY_TOP45);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 构造一个角色资产根；frames 一定有，manifest.json 可选
    fn make_character(root: &Path, name: &str, manifest: Option<&str>) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        if let Some(m) = manifest {
            std::fs::create_dir_all(dir.join("meta")).unwrap();
            std::fs::write(dir.join("meta/manifest.json"), m).unwrap();
        }
        dir
    }

    #[test]
    fn discovery_builtin_first_then_sorted_by_name() {
        let root = temp_root("multi");
        let builtin = make_character(&root, "builtin", None);
        let ext = root.join("ext");
        make_character(&ext, "miko", Some(r#"{"name":"巫女"}"#));
        make_character(&ext, "alpha", None);
        // 只有嵌套子目录、自己没有 frames/ 的不算角色，跳过
        std::fs::create_dir_all(ext.join("not-a-character/nested")).unwrap();

        let found = discover_characters(&builtin, &ext);
        assert_eq!(
            found.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["shenshen", "alpha", "miko"]
        );
        // 内置角色没有 manifest 文件时，范围框回退为整个画面
        assert_eq!(
            found[0].meta.body_box,
            FrameBox {
                x0: 0.0,
                y0: 0.0,
                x1: 320.0,
                y1: 180.0
            }
        );
        // 外部角色的 manifest 带显示名时能解析出来
        assert_eq!(found[2].meta.display_name, "巫女");
        assert_eq!(found[2].root, ext.join("miko"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn builtin_wins_on_name_conflict() {
        let root = temp_root("dup");
        let builtin = make_character(&root, "builtin", None);
        let ext = root.join("ext");
        make_character(&ext, "shenshen", Some(r#"{"name":"假深酱"}"#));
        let found = discover_characters(&builtin, &ext);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "shenshen");
        assert_eq!(found[0].meta.display_name, "shenshen"); // 内置（无 manifest），非外部假货
        assert_eq!(found[0].root, builtin);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_builtin_root_and_empty_external_root() {
        let root = temp_root("empty");
        let ext = root.join("ext");
        std::fs::create_dir_all(&ext).unwrap();
        // 内置根无 frames/ → 只报外部；两边皆空 → 空表
        let bare = root.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(discover_characters(&bare, &ext).is_empty());
        // 内置空 + 外部一个 → 仅外部
        make_character(&ext, "solo", None);
        let found = discover_characters(&bare, &ext);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "solo");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn character_id_separator_normalization() {
        let root = temp_root("sep");
        let builtin = make_character(&root, "builtin", None);
        let ext = root.join("ext");
        // unix 文件名里的字面反斜杠 → id 规范化为 '/'
        #[cfg(not(target_os = "windows"))]
        {
            make_character(&ext, "a\\b", None);
            let found = discover_characters(&builtin, &ext);
            assert!(found.iter().any(|c| c.id == "a/b"));
        }
        // Windows 的 read_dir 文件名不含 '\'；断言所有 id 均无反斜杠
        #[cfg(target_os = "windows")]
        {
            make_character(&ext, "plain", None);
            let found = discover_characters(&builtin, &ext);
            assert!(found.iter().all(|c| !c.id.contains('\\')));
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn external_root_platform_convention() {
        // 三平台目录期望写死断言（AGENTS 规则 6）
        let p = external_characters_root();
        #[cfg(target_os = "macos")]
        assert!(p.ends_with("Library/Application Support/ds-pet/characters"));
        #[cfg(target_os = "windows")]
        assert!(p.ends_with(r"ds-pet\characters"));
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(p.ends_with("ds-pet/characters"));
    }
}
