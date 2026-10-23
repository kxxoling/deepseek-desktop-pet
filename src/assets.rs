//! 资产根目录定位（frames/ + meta/）

use std::path::PathBuf;

/// 探测资产根目录：`DS_PET_ASSETS` > exe 同级 / macOS bundle Resources / cwd。
/// 内置点击音效的字节（首次使用时加载一次）
static CLICK_SOUND: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();

/// 从资产目录读取点击音效文件
fn load_click_sound_file() -> Option<Vec<u8>> {
    let path = locate_assets_root().join("sounds/default/click.wav");
    std::fs::read(&path).ok()
}

/// 取内置点击音效；文件缺失返回 None
pub fn click_sound() -> Option<&'static [u8]> {
    CLICK_SOUND.get_or_init(load_click_sound_file).as_deref()
}

/// 解析点击音效要播的字节：custom 包用数据目录的自定义音效（click.wav
/// 优先，其次 custom/ 目录里按 roll 挑一个）；其余包名和找不到自定义
/// 文件的情况都回退内置音效
pub fn click_sound_for(pack: &str, roll: f64) -> Option<Vec<u8>> {
    if pack == "custom"
        && let Some(bytes) = custom_click_sound(roll)
    {
        return Some(bytes);
    }
    click_sound().map(<[u8]>::to_vec)
}

/// 数据目录取配置文件所在目录（config.json 旁边的 sounds/）
fn custom_click_sound(roll: f64) -> Option<Vec<u8>> {
    let root = crate::config::config_path().parent()?.join("sounds");
    // 单独的 click.wav 优先于 custom/ 目录
    if let Ok(bytes) = std::fs::read(root.join("click.wav")) {
        return Some(bytes);
    }
    let mut names: Vec<String> = std::fs::read_dir(root.join("custom"))
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_ascii_lowercase().ends_with(".wav"))
        .collect();
    // read_dir 顺序因平台而异，排序保证 roll 与文件的对应关系稳定
    names.sort();
    let pick = pick_by_roll(&names, roll)?;
    std::fs::read(root.join("custom").join(pick)).ok()
}

/// 按 roll ∈ [0,1] 从非空列表挑一个；空列表返回 None
pub(crate) fn pick_by_roll(names: &[String], roll: f64) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let idx = (roll.clamp(0.0, 1.0) * names.len() as f64) as usize;
    Some(names[idx.min(names.len() - 1)].clone())
}

pub fn locate_assets_root() -> PathBuf {
    if let Ok(p) = std::env::var("DS_PET_ASSETS") {
        let p = PathBuf::from(p);
        if p.join("frames").is_dir() {
            return p;
        }
        let shown = p.display().to_string();
        log::warn!("DS_PET_ASSETS 指向的目录无效：{shown}（缺少 frames/），继续探测默认位置");
    }

    // 首个候选兼作全部未命中时的返回值
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("assets")];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("assets"));
        // macOS .app bundle：Contents/MacOS/ds-pet → Contents/Resources/assets
        if let Some(contents) = dir.parent() {
            candidates.push(contents.join("Resources").join("assets"));
            candidates.push(contents.join("assets"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("assets"));
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("assets"));
    }

    candidates
        .into_iter()
        .find(|c| c.join("frames").is_dir())
        .unwrap_or_else(|| PathBuf::from("assets"))
}

#[cfg(test)]
#[allow(unsafe_code)] // 测试内修改进程环境变量（edition 2024 要求 unsafe 块）
mod tests {

    #[test]
    fn load_click_sound_file_reads_wav() {
        // locate_assets_root 读进程环境变量与工作目录，只读用例同样要与改环境的用例串行，
        // 否则并行时相对路径可能被解析进别的用例的临时目录
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 直接调加载函数，绕过全局缓存
        let data = load_click_sound_file().expect("音效文件应可读取");
        assert!(data.len() > 44);
        assert_eq!(&data[0..4], b"RIFF");
    }

    #[test]
    fn click_sound_file_exists_and_is_wav() {
        // 相对路径随进程工作目录解析，同样须串行
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 直接读文件，不经过全局缓存（OnceLock 可能被其他测试先初始化为 None）
        let data = std::fs::read("assets/sounds/default/click.wav")
            .expect("内置音效文件应存在于 assets/sounds/default/");
        assert!(data.len() > 44, "音效文件过小：{} 字节", data.len());
        assert_eq!(&data[0..4], b"RIFF");
    }

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

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ds-pet-assets-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    /// 环境变量互斥：`DS_PET_ASSETS` 的设置/读取测试串行执行
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn invalid_env_falls_back_to_default_probe() {
        ensure_logger();
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 构造自洽环境：临时 cwd 含 assets/frames，env 指向无效目录
        let home = temp_root("fallback");
        std::fs::create_dir_all(home.join("assets/frames")).unwrap();
        let orig_cwd = std::env::current_dir().unwrap();
        unsafe {
            std::env::set_var("DS_PET_ASSETS", home.join("nope"));
        }
        std::env::set_current_dir(&home).unwrap();
        let found = locate_assets_root();
        // 相对路径须在离开临时 cwd 前完成解析
        let found_abs = found.canonicalize().unwrap();
        std::env::set_current_dir(orig_cwd).unwrap();
        unsafe {
            std::env::remove_var("DS_PET_ASSETS");
        }
        let want = home.join("assets").canonicalize().unwrap();
        assert_eq!(found_abs, want, "应忽略无效 env 并命中 cwd 探测");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn default_probe_finds_cwd_assets() {
        ensure_logger();
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 构造自洽环境：临时 cwd 含 assets/frames（CI 的 checkout 不带产物资产）
        let home = temp_root("cwd");
        std::fs::create_dir_all(home.join("assets/frames")).unwrap();
        let orig_cwd = std::env::current_dir().unwrap();
        unsafe {
            std::env::remove_var("DS_PET_ASSETS");
        }
        std::env::set_current_dir(&home).unwrap();
        let root = locate_assets_root();
        let root_abs = root.canonicalize().unwrap();
        std::env::set_current_dir(orig_cwd).unwrap();
        let want = home.join("assets").canonicalize().unwrap();
        assert_eq!(root_abs, want);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn env_dir_takes_priority() {
        ensure_logger();
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_root("prio");
        std::fs::create_dir_all(root.join("frames")).unwrap();
        unsafe {
            std::env::set_var("DS_PET_ASSETS", &root);
        }
        assert_eq!(locate_assets_root(), root);
        unsafe {
            std::env::remove_var("DS_PET_ASSETS");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pick_by_roll_covers_list_in_order() {
        let names: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(pick_by_roll(&names, 0.0).as_deref(), Some("a"));
        assert_eq!(
            pick_by_roll(&names, 1.0 / names.len() as f64).as_deref(),
            Some("b")
        );
        // roll 上界钳到最后一个
        assert_eq!(pick_by_roll(&names, 1.0).as_deref(), Some("c"));
        assert_eq!(pick_by_roll(&[], 0.5), None);
    }

    #[test]
    fn custom_pack_prefers_single_file_then_random_dir() {
        ensure_logger();
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home = temp_root("custompack");
        std::fs::create_dir_all(home.join("sounds/custom")).unwrap();
        std::fs::write(home.join("sounds/custom/second.wav"), b"RIFF-two").unwrap();
        unsafe {
            std::env::set_var("DS_PET_CONFIG", home.join("config.json"));
        }
        // custom/ 只有一个文件时任意 roll 都选中它
        assert_eq!(
            click_sound_for("custom", 0.99).as_deref(),
            Some(b"RIFF-two".as_slice())
        );
        // 单独的 click.wav 优先于 custom/ 目录
        std::fs::write(home.join("sounds/click.wav"), b"RIFF-one").unwrap();
        assert_eq!(
            click_sound_for("custom", 0.0).as_deref(),
            Some(b"RIFF-one".as_slice())
        );
        unsafe {
            std::env::remove_var("DS_PET_CONFIG");
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unconfigured_custom_or_unknown_pack_falls_back_to_builtin() {
        ensure_logger();
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 数据目录没有任何自定义音效时，custom 与未知包名都回退内置默认
        let home = temp_root("fallbackpack");
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("DS_PET_CONFIG", home.join("config.json"));
        }
        let builtin = std::fs::read("assets/sounds/default/click.wav").unwrap();
        assert_eq!(click_sound_for("custom", 0.0).unwrap(), builtin);
        assert_eq!(click_sound_for("duck", 0.0).unwrap(), builtin);
        unsafe {
            std::env::remove_var("DS_PET_CONFIG");
        }
        std::fs::remove_dir_all(&home).ok();
    }
}
