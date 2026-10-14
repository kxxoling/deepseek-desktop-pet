//! 帧库：clip 索引 + 首帧常驻缓存 + 活跃 clip 懒加载。
//!
//! 全量帧解码后约 3.6GB，禁止整包载入：启动只缓存各 clip 首帧（约 24MB），
//! 活跃 clip 后台加载、多窗共享 Arc、引用计数驱逐（最后一个窗口切走即释放）。
//! 帧存储用单块连续缓冲，释放时整块归还。
//!
//! 帧文件格式（优先）`clip.zst`：zstd 压缩的连续 RGBA 帧流，运行时仅解压；
//! 回退 `f0000.png..`：PNG 序列逐帧解码。

use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

/// 统一帧规格：320×180 RGBA @15fps（预处理管线决定）
pub const FRAME_W: u32 = 320;
pub const FRAME_H: u32 = 180;
pub const FRAME_BYTES: usize = (FRAME_W * FRAME_H * 4) as usize;
pub const FPS: f64 = 15.0;
/// 源素材帧率（`move_strides` 的 curve 按 24fps 帧号标定，重采样用）
pub const SRC_FPS: f64 = 24.0;
/// 帧时长
pub const FRAME_DUR: std::time::Duration = std::time::Duration::from_nanos(1_000_000_000 / 15);

/// clip 标识：frames/ 下的相对路径，如 "idle/待机呼吸休闲"
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ClipId(pub Arc<str>);

impl std::fmt::Display for ClipId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct ClipMeta {
    pub id: ClipId,
    /// 一级目录名（idle/move/click/drag/random/events/turn）
    pub category: Arc<str>,
    /// clip 名（目录名，不含扩展），`move_strides` / `text_clips` 按名匹配
    pub name: Arc<str>,
    /// PNG 帧序列（`f0000.png..`，`zst` 缺失时回退）
    pub files: Vec<PathBuf>,
    /// 单文件快加载格式：zstd 压缩的连续 RGBA 帧流（优先）
    pub zst: Option<PathBuf>,
}

impl ClipMeta {
    /// PNG 模式下的帧数（zst 模式以解压后实际大小为准）
    pub fn frame_count(&self) -> usize {
        self.files.len()
    }
}

/// 一帧 RGBA（长度 `FRAME_BYTES`），用于首帧常驻缓存
pub struct Frame(pub Vec<u8>);

/// 一个 clip 的完整帧序列：单块连续缓冲
pub struct ClipFrames {
    data: Vec<u8>,
    frame_count: usize,
}

impl ClipFrames {
    pub fn frame_count(&self) -> usize {
        self.frame_count
    }

    /// 取第 idx 帧；越界钳制到最后一帧（加载中/竞态时的兜底）
    pub fn frame(&self, idx: usize) -> &[u8] {
        let idx = idx.min(self.frame_count.saturating_sub(1));
        &self.data[idx * FRAME_BYTES..(idx + 1) * FRAME_BYTES]
    }
}

pub struct Library {
    clips: Vec<Arc<ClipMeta>>,
    index: HashMap<ClipId, Arc<ClipMeta>>,
    /// 全部 clip 首帧常驻缓存（切换零延迟）
    first_frames: HashMap<ClipId, Arc<Frame>>,
    /// 活跃缓存：Weak 引用，窗口持有 Arc，全部切走后自动释放
    active: Mutex<HashMap<ClipId, Weak<ClipFrames>>>,
    /// 加载中标记（防重复起线程；Arc 供后台线程失败时自行清理）
    loading: Arc<Mutex<HashSet<ClipId>>>,
    /// clip 名 -> (步幅 px/圈, 圈内逐帧累计位移曲线 0..1)
    pub move_strides: HashMap<String, (f64, Vec<f64>)>,
    /// `text_clips.json` 的 `no_mirror` 名单：含可读文字、朝右时不做水平镜像
    pub no_mirror: HashSet<String>,
}

/// 解码一张 PNG 追加到连续缓冲；失败返回 false（中止该 clip，避免帧错位）
fn decode_png_into(path: &Path, out: &mut Vec<u8>) -> bool {
    let Ok(data) = std::fs::read(path) else {
        log::error!("读取失败：{}", path.display());
        return false;
    };
    let decoder = png::Decoder::new(Cursor::new(data));
    let Ok(mut reader) = decoder.read_info() else {
        log::error!("PNG 头解析失败：{}", path.display());
        return false;
    };
    let start = out.len();
    out.resize(start + FRAME_BYTES, 0);
    match reader.next_frame(&mut out[start..]) {
        Ok(info) if info.width == FRAME_W && info.height == FRAME_H => true,
        Ok(info) => {
            log::error!(
                "帧尺寸异常 {}×{}：{}",
                info.width,
                info.height,
                path.display()
            );
            out.truncate(start);
            false
        }
        Err(e) => {
            log::error!("PNG 解码失败 {e}：{}", path.display());
            out.truncate(start);
            false
        }
    }
}

/// 加载完整 clip（后台线程调用）：优先 `clip.zst`，回退 PNG 逐帧解码
fn load_clip(meta: &ClipMeta) -> Option<ClipFrames> {
    if let Some(zst) = &meta.zst {
        let Ok(compressed) = std::fs::read(zst) else {
            log::error!("读取失败：{}", zst.display());
            return None;
        };
        let data = match zstd::decode_all(Cursor::new(&compressed)) {
            Ok(d) => d,
            Err(e) => {
                log::error!("zstd 解压失败 {e}：{}", zst.display());
                return None;
            }
        };
        if data.is_empty() || data.len() % FRAME_BYTES != 0 {
            log::error!(
                "clip.zst 大小非法（{} 字节，须为 {FRAME_BYTES} 整数倍）：{}",
                data.len(),
                zst.display()
            );
            return None;
        }
        return Some(ClipFrames {
            frame_count: data.len() / FRAME_BYTES,
            data,
        });
    }

    let mut data = Vec::with_capacity(meta.files.len() * FRAME_BYTES);
    for f in &meta.files {
        if !decode_png_into(f, &mut data) {
            return None;
        }
    }
    Some(ClipFrames {
        frame_count: meta.files.len(),
        data,
    })
}

/// 加载首帧：zst 流式解压只读一帧（不解压整个 clip）；PNG 解码 `f0000.png`
fn load_first_frame(meta: &ClipMeta) -> Option<Arc<Frame>> {
    if let Some(zst) = &meta.zst {
        let mut file = std::fs::File::open(zst).ok()?;
        let mut dec = zstd::stream::read::Decoder::new(&mut file).ok()?;
        let mut buf = vec![0u8; FRAME_BYTES];
        dec.read_exact(&mut buf).ok()?;
        return Some(Arc::new(Frame(buf)));
    }
    let mut buf = Vec::with_capacity(FRAME_BYTES);
    if decode_png_into(&meta.files[0], &mut buf) {
        Some(Arc::new(Frame(buf)))
    } else {
        None
    }
}

/// 列目录条目；不可读（或非目录）返回空
fn list_dir(dir: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(std::result::Result::ok).collect())
        .unwrap_or_default()
}

/// 递归收集含帧资产的目录为 clip（`clip.zst` 或 `f*.png`）
fn scan_clips(root: &Path, dir: &Path, out: &mut Vec<Arc<ClipMeta>>) {
    let mut files: Vec<PathBuf> = list_dir(dir)
        .into_iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("png")
                && p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| {
                    s.starts_with('f') && s[1..].chars().all(|c| c.is_ascii_digit())
                })
        })
        .collect();
    files.sort(); // f0000.. 零填充，字典序即帧序

    let zst = dir.join("clip.zst");
    let has_zst = zst.is_file();
    if has_zst || !files.is_empty() {
        let rel = dir.strip_prefix(root).unwrap_or(dir);
        // 分隔符统一 '/'（Windows 的 read_dir 路径为 '\'）
        let id = ClipId(Arc::from(rel.to_string_lossy().replace('\\', "/").as_str()));
        let category = Arc::from(
            rel.components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_str(),
        );
        let name = Arc::from(
            rel.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_str(),
        );
        out.push(Arc::new(ClipMeta {
            id: id.clone(),
            category,
            name,
            files,
            zst: has_zst.then_some(zst),
        }));
    }

    // 递归子目录
    let mut subs: Vec<PathBuf> = list_dir(dir)
        .into_iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    subs.sort();
    for sub in subs {
        scan_clips(root, &sub, out);
    }
}

/// assets/meta 下的三份元数据解析结果
struct MetaInfo {
    /// clip 名 -> (步幅 px/圈, 圈内逐帧累计位移曲线 0..1)
    move_strides: HashMap<String, (f64, Vec<f64>)>,
    /// `text_clips.json` 的 `no_mirror` 名单：含可读文字、朝右时不做水平镜像
    no_mirror: HashSet<String>,
}

fn parse_meta(meta_dir: &Path) -> MetaInfo {
    let mut strides = HashMap::new();
    let mut no_mirror = HashSet::new();
    if let Ok(s) = std::fs::read_to_string(meta_dir.join("move_strides.json"))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&s)
        && let Some(map) = v.as_object()
    {
        for (name, item) in map {
            if name.starts_with('_') {
                continue;
            }
            let stride = item
                .get("stride")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            let curve = item
                .get("curve")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(serde_json::Value::as_f64)
                        .collect::<Vec<f64>>()
                })
                .unwrap_or_default();
            if stride > 0.0 && curve.len() >= 2 {
                strides.insert(name.clone(), (stride, curve));
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string(meta_dir.join("text_clips.json"))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&s)
        && let Some(list) = v.get("no_mirror").and_then(|n| n.as_array())
    {
        for n in list {
            if let Some(name) = n.as_str() {
                no_mirror.insert(name.to_string());
            }
        }
    }
    MetaInfo {
        move_strides: strides,
        no_mirror,
    }
}

impl Library {
    /// 加载帧库：扫描索引 + 全部首帧常驻 + 元数据。找不到任何 clip 返回 Err。
    pub fn load(assets_root: &Path) -> Result<Self, String> {
        let frames_root = assets_root.join("frames");
        if !frames_root.is_dir() {
            return Err(format!("帧目录不存在：{}", frames_root.display()));
        }

        let mut clips = Vec::new();
        scan_clips(&frames_root, &frames_root, &mut clips);
        if clips.is_empty() {
            return Err(format!(
                "帧目录为空：{}（先运行 scripts/preprocess_assets.sh）",
                frames_root.display()
            ));
        }
        clips.sort_by(|a, b| a.id.0.cmp(&b.id.0));

        let zst_count = clips.iter().filter(|c| c.zst.is_some()).count();
        let png_frames: usize = clips.iter().map(|c| c.frame_count()).sum();
        log::info!(
            "帧库索引：{} 个 clip（zst 快加载 {zst_count} 个，PNG 回退 {} 帧）",
            clips.len(),
            png_frames
        );

        let mut first_frames = HashMap::new();
        for clip in &clips {
            match load_first_frame(clip) {
                Some(f) => {
                    first_frames.insert(clip.id.clone(), f);
                }
                None => log::error!("首帧加载失败：{}", clip.id),
            }
        }

        let meta = parse_meta(&assets_root.join("meta"));
        let MetaInfo {
            move_strides,
            no_mirror,
        } = meta;
        log::info!(
            "元数据：{} 条步幅曲线，{} 个不镜像片段",
            move_strides.len(),
            no_mirror.len()
        );

        let index = clips.iter().map(|c| (c.id.clone(), c.clone())).collect();
        Ok(Self {
            clips,
            index,
            first_frames,
            active: Mutex::new(HashMap::new()),
            loading: Arc::new(Mutex::new(HashSet::new())),
            move_strides,
            no_mirror,
        })
    }

    pub fn clips(&self) -> &[Arc<ClipMeta>] {
        &self.clips
    }

    pub fn meta(&self, id: &ClipId) -> Option<&Arc<ClipMeta>> {
        self.index.get(id)
    }

    pub fn first_frame(&self, id: &ClipId) -> Option<&Arc<Frame>> {
        self.first_frames.get(id)
    }

    pub fn clips_in_category(&self, category: &str) -> Vec<ClipId> {
        self.clips
            .iter()
            .filter(|c| &*c.category == category)
            .map(|c| c.id.clone())
            .collect()
    }

    pub fn find_idle(&self) -> Option<ClipId> {
        self.clips_in_category("idle").into_iter().next()
    }

    pub fn no_mirror(&self, name: &str) -> bool {
        self.no_mirror.contains(name)
    }

    /// 取活跃帧集；未加载则返回 None 并触发后台加载，完成后调用 `notify`
    pub fn get_or_load(
        &self,
        id: &ClipId,
        notify: impl Fn(ClipId, Arc<ClipFrames>) + Send + 'static,
    ) -> Option<Arc<ClipFrames>> {
        {
            let active = self.active.lock().unwrap();
            if let Some(weak) = active.get(id)
                && let Some(arc) = weak.upgrade()
            {
                return Some(arc);
            }
        }

        if self.loading.lock().unwrap().insert(id.clone()) {
            let Some(meta) = self.index.get(id).cloned() else {
                self.loading.lock().unwrap().remove(id);
                log::error!("未知 clip：{id}");
                return None;
            };
            let id = id.clone();
            let loading = Arc::clone(&self.loading);
            std::thread::spawn(move || {
                if let Some(cf) = load_clip(&meta) {
                    notify(id, Arc::new(cf));
                } else {
                    log::error!("clip 加载失败：{id}");
                    loading.lock().unwrap().remove(&id);
                }
            });
        }
        None
    }

    /// 后台加载完成：登记进 active 表。返回值丢弃也安全（Weak 立即失效，下次重加载）。
    pub fn install_loaded(&self, id: &ClipId, frames: &Arc<ClipFrames>) {
        self.loading.lock().unwrap().remove(id);
        let mut active = self.active.lock().unwrap();
        active.insert(id.clone(), Arc::downgrade(frames));
        // 顺手清理死条目
        active.retain(|_, w| w.strong_count() > 0);
    }
}

#[cfg(test)]
mod from_flat_tests {
    use super::*;

    #[test]
    #[should_panic(expected = "assertion failed")]
    fn 空数据断言失败() {
        let _ = ClipFrames::from_flat(Vec::new());
    }
}

#[cfg(test)]
impl ClipFrames {
    /// 测试构造器：连续缓冲按帧切分
    pub fn from_flat(data: Vec<u8>) -> Self {
        assert!(!data.is_empty() && data.len().is_multiple_of(FRAME_BYTES));
        Self {
            frame_count: data.len() / FRAME_BYTES,
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：安装 Trace 级空 logger，保证 log 宏参数被求值（覆盖率统计需要）
    fn ensure_logger() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        struct Noop;
        impl log::Log for Noop {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                false
            }
            fn log(&self, _: &log::Record<'_>) {}
            fn flush(&self) {}
        }
        ONCE.call_once(|| {
            let _ = log::set_boxed_logger(Box::new(Noop));
            log::set_max_level(log::LevelFilter::Trace);
        });
    }

    /// 用 png crate 生成一张合法的 320×180 RGBA 测试帧
    fn write_png(path: &Path, fill: u8) {
        ensure_logger();
        let out_file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(out_file, FRAME_W, FRAME_H);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&vec![fill; FRAME_BYTES]).unwrap();
    }

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ds-pet-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn 帧库扫描_首帧缓存_元数据解析() {
        let root = temp_root("lib");
        let frames = root.join("assets/frames");
        std::fs::create_dir_all(frames.join("idle/呼吸")).unwrap();
        std::fs::create_dir_all(frames.join("events/balance/没钱了")).unwrap();
        std::fs::create_dir_all(frames.join("random/是啊")).unwrap();
        write_png(&frames.join("idle/呼吸/f0000.png"), 10);
        write_png(&frames.join("idle/呼吸/f0001.png"), 20);
        write_png(&frames.join("events/balance/没钱了/f0000.png"), 30);
        write_png(&frames.join("random/是啊/f0000.png"), 40);

        let meta = root.join("assets/meta");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(
            meta.join("move_strides.json"),
            r#"{"左转奔跑":{"stride":240,"curve":[0.0,0.5,1.0]},"_comment":"被忽略"}"#,
        )
        .unwrap();
        std::fs::write(meta.join("text_clips.json"), r#"{"no_mirror":["是啊"]}"#).unwrap();

        let lib = Library::load(&root.join("assets")).unwrap();

        // 扫描：三个 clip，events 子目录归入 events 分类
        assert_eq!(lib.clips().len(), 3);
        assert_eq!(lib.clips_in_category("idle").len(), 1);
        assert_eq!(lib.clips_in_category("events").len(), 1);
        assert_eq!(lib.clips_in_category("random").len(), 1);
        assert_eq!(
            lib.meta(&ClipId(Arc::from("idle/呼吸")))
                .unwrap()
                .frame_count(),
            2
        );
        // 嵌套目录的 id 同样为 '/' 分隔
        assert!(
            lib.meta(&ClipId(Arc::from("events/balance/没钱了")))
                .is_some()
        );

        // 首帧常驻缓存
        let idle = lib.find_idle().unwrap();
        let first = lib.first_frame(&idle).unwrap();
        assert_eq!(first.0[0], 10);

        // PNG 回退路径：完整加载为连续缓冲
        let cf = load_clip(lib.meta(&idle).unwrap()).unwrap();
        assert_eq!(cf.frame_count(), 2);
        assert_eq!(cf.frame(0)[0], 10);
        assert_eq!(cf.frame(1)[0], 20);

        // 元数据：no_mirror / 步幅曲线（_comment 被忽略）/ 包围盒 640→320 等比缩放
        assert!(lib.no_mirror("是啊"));
        assert!(!lib.no_mirror("呼吸"));
        let (stride, curve) = lib.move_strides.get("左转奔跑").cloned().unwrap();
        assert!((stride - 240.0).abs() < f64::EPSILON);
        assert_eq!(curve, vec![0.0, 0.5, 1.0]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn zst_快加载_连续缓冲_首帧流式读取() {
        ensure_logger();
        let root = temp_root("zst");
        let clip_dir = root.join("assets/frames/idle/呼吸");
        std::fs::create_dir_all(&clip_dir).unwrap();
        // 3 帧，帧 i 全部填充 (i+1)*10
        let mut raw = Vec::new();
        for i in 1..=3u8 {
            raw.extend_from_slice(&vec![i * 10; FRAME_BYTES]);
        }
        let compressed = zstd::encode_all(Cursor::new(&raw), 1).unwrap();
        std::fs::write(clip_dir.join("clip.zst"), compressed).unwrap();

        let lib = Library::load(&root.join("assets")).unwrap();
        assert_eq!(lib.clips().len(), 1);

        // 首帧：流式只解压第一帧
        let first = lib.first_frame(&lib.find_idle().unwrap()).unwrap();
        assert_eq!(first.0[0], 10);

        // 完整加载：zst 路径
        let cf = load_clip(lib.meta(&lib.find_idle().unwrap()).unwrap()).unwrap();
        assert_eq!(cf.frame_count(), 3);
        assert_eq!(cf.frame(0)[0], 10);
        assert_eq!(cf.frame(2)[0], 30);
        // 越界钳制到最后一帧
        assert_eq!(cf.frame(999)[0], 30);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 帧目录缺失或为空时报错() {
        let root = temp_root("empty");
        std::fs::create_dir_all(root.join("assets/frames")).unwrap();
        assert!(Library::load(&root.join("assets")).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 非法zst返回none() {
        ensure_logger();
        let root = temp_root("badzst");
        let clip_dir = root.join("assets/frames/idle/x");
        std::fs::create_dir_all(&clip_dir).unwrap();
        // 大小非 FRAME_BYTES 整数倍
        std::fs::write(
            clip_dir.join("clip.zst"),
            zstd::encode_all(&[1u8; 100][..], 1).unwrap(),
        )
        .unwrap();
        let lib = Library::load(&root.join("assets")).unwrap();
        let meta = lib.meta(&lib.find_idle().unwrap()).unwrap().clone();
        assert!(load_clip(&meta).is_none());

        // 非 zstd 数据
        std::fs::write(clip_dir.join("clip.zst"), b"not-zstd").unwrap();
        let lib = Library::load(&root.join("assets")).unwrap();
        let meta = lib.meta(&lib.find_idle().unwrap()).unwrap().clone();
        assert!(load_clip(&meta).is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 损坏png返回none() {
        let root = temp_root("badpng");
        let clip_dir = root.join("assets/frames/idle/x");
        std::fs::create_dir_all(&clip_dir).unwrap();
        std::fs::write(clip_dir.join("f0000.png"), b"garbage").unwrap();
        std::fs::write(clip_dir.join("f0001.png"), b"garbage").unwrap();
        let lib = Library::load(&root.join("assets")).unwrap();
        let meta = lib.meta(&lib.find_idle().unwrap()).unwrap().clone();
        assert!(load_first_frame(&meta).is_none());
        assert!(load_clip(&meta).is_none());
        assert!(lib.first_frame(&lib.find_idle().unwrap()).is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 活跃缓存命中与登记() {
        ensure_logger();
        let root = temp_root("cache");
        let clip_dir = root.join("assets/frames/idle/x");
        std::fs::create_dir_all(&clip_dir).unwrap();
        write_png(&clip_dir.join("f0000.png"), 3);
        write_png(&clip_dir.join("f0001.png"), 5);
        let lib = Library::load(&root.join("assets")).unwrap();
        let idle = lib.find_idle().unwrap();

        // 未命中 → 触发后台加载并经 notify 送达
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(
            lib.get_or_load(&idle, move |id, frames| {
                tx.send((id, frames)).unwrap();
            })
            .is_none()
        );
        let (loaded_id, frames) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(loaded_id, idle);
        lib.install_loaded(&loaded_id, &frames);
        assert_eq!(frames.frame_count(), 2);

        // 命中 active 缓存
        let again = lib.get_or_load(&idle, |_, _| panic!("不应再触发"));
        assert!(again.is_some());

        // 未知 clip
        let unknown = ClipId(Arc::from("nope/none"));
        assert!(lib.get_or_load(&unknown, |_, _| {}).is_none());
        assert!(lib.meta(&unknown).is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 截断png解码失败() {
        let root = temp_root("trunc");
        let clip_dir = root.join("assets/frames/idle/x");
        std::fs::create_dir_all(&clip_dir).unwrap();
        write_png(&clip_dir.join("f0000.png"), 8);
        // 截断后半：头完整（read_info 通过），数据损坏（next_frame 失败）
        let data = std::fs::read(clip_dir.join("f0000.png")).unwrap();
        std::fs::write(clip_dir.join("f0000.png"), &data[..data.len() / 2]).unwrap();
        let mut out = Vec::new();
        assert!(!decode_png_into(&clip_dir.join("f0000.png"), &mut out));
        assert_eq!(out, [] as [u8; 0]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 加载中重复请求不重复起线程() {
        let root = temp_root("dedup");
        let clip_dir = root.join("assets/frames/idle/x");
        std::fs::create_dir_all(&clip_dir).unwrap();
        let compressed = zstd::encode_all(&vec![2u8; crate::library::FRAME_BYTES][..], 1).unwrap();
        std::fs::write(clip_dir.join("clip.zst"), compressed).unwrap();
        let lib = Library::load(&root.join("assets")).unwrap();
        let idle = lib.find_idle().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let tx2 = tx.clone();
        assert!(
            lib.get_or_load(&idle, move |_, _| {
                tx2.send(()).unwrap();
            })
            .is_none()
        );
        // loading 尚未完成时重复请求：不再起线程，直接返回 None
        assert!(
            lib.get_or_load(&idle, move |_, _| {
                tx.send(()).unwrap();
            })
            .is_none()
        );
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn clipid显示为路径() {
        let id = ClipId(Arc::from("idle/x"));
        assert_eq!(format!("{id}"), "idle/x");
    }

    #[test]
    fn png读取失败与尺寸异常() {
        let mut out = Vec::new();
        assert!(!decode_png_into(
            Path::new("/nonexistent/f0000.png"),
            &mut out
        ));
        assert_eq!(out, [] as [u8; 0]);

        // 尺寸错误的合法 PNG（160×90）
        let root = temp_root("badsize");
        std::fs::create_dir_all(&root).unwrap();
        let f = std::fs::File::create(root.join("bad.png")).unwrap();
        let mut enc = png::Encoder::new(f, 160, 90);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&vec![0u8; 160 * 90 * 4]).unwrap();
        drop(w);
        assert!(!decode_png_into(&root.join("bad.png"), &mut out));
        assert_eq!(out, [] as [u8; 0]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn zst读取失败与scan错误分支() {
        let root = temp_root("zstmiss");
        let clip_dir = root.join("assets/frames/idle/x");
        std::fs::create_dir_all(&clip_dir).unwrap();
        let compressed =
            zstd::encode_all(&vec![1u8; crate::library::FRAME_BYTES * 2][..], 1).unwrap();
        std::fs::write(clip_dir.join("clip.zst"), compressed).unwrap();
        let lib = Library::load(&root.join("assets")).unwrap();
        let meta = lib.meta(&lib.find_idle().unwrap()).unwrap().clone();
        std::fs::remove_file(clip_dir.join("clip.zst")).unwrap();
        assert!(load_clip(&meta).is_none());
        assert!(load_first_frame(&meta).is_none());

        // scan_clips 对"非目录"路径直接返回
        let mut found = Vec::new();
        scan_clips(&root, &root.join("assets/meta/manifest.json"), &mut found);
        assert!(found.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 帧目录不存在时报错() {
        let root = temp_root("nodir");
        std::fs::create_dir_all(&root).unwrap();
        assert!(Library::load(&root.join("assets")).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 元数据坏json被忽略() {
        let root = temp_root("badjson");
        let frames = root.join("assets/frames");
        std::fs::create_dir_all(frames.join("idle/x")).unwrap();
        write_png(&frames.join("idle/x/f0000.png"), 1);
        let meta = root.join("assets/meta");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("move_strides.json"), "{invalid").unwrap();
        std::fs::write(meta.join("text_clips.json"), "[also bad").unwrap();
        std::fs::write(meta.join("manifest.json"), "\"scalar\"").unwrap();
        let lib = Library::load(&root.join("assets")).unwrap();
        assert!(lib.move_strides.is_empty());
        assert!(lib.no_mirror.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 加载失败清理loading允许重试() {
        ensure_logger();
        let root = temp_root("retry");
        let clip_dir = root.join("assets/frames/idle/x");
        std::fs::create_dir_all(&clip_dir).unwrap();
        // 损坏 zst：扫描可注册，但加载必失败
        std::fs::write(clip_dir.join("clip.zst"), b"garbage").unwrap();
        let lib = Library::load(&root.join("assets")).unwrap();
        let idle = lib.find_idle().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        assert!(
            lib.get_or_load(&idle, move |_, _| {
                tx.send(()).unwrap();
            })
            .is_none()
        );
        // 失败路径不应调用 notify
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err()
        );
        for _ in 0..100 {
            if lib.loading.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(lib.loading.lock().unwrap().is_empty());

        std::fs::remove_dir_all(&root).ok();
    }
}
