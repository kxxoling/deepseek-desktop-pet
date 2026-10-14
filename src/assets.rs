//! 资产根目录定位（frames/ + meta/）

use std::path::PathBuf;

/// 探测资产根目录：`DS_PET_ASSETS` > exe 同级 / macOS bundle Resources / cwd。
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
    fn 无效环境变量时继续默认探测() {
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
    fn 默认探测命中工作目录资产() {
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
    fn 环境变量指定的资产目录优先() {
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
}
