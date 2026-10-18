use super::*;

/// 安装空 logger，让日志宏的参数被求值（覆盖率统计需要）
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

/// 修改环境变量的用例互斥串行
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("ds-pet-cfg-{tag}-{}-{nanos}", std::process::id()))
}

#[test]
fn defaults_and_value_roundtrip() {
    let mut c = Config::default();
    assert_eq!(c.version(), CURRENT_VERSION);
    assert_eq!(
        c.get(KEY_VERSION).and_then(Value::as_u64),
        Some(CURRENT_VERSION)
    );

    c.set("b", true);
    c.set("i", -3i64);
    c.set("u", 7u64);
    c.set("f", 1.5f64);
    c.set("s", "文本");
    c.set("arr", serde_json::json!([1.0, 2.0]));
    assert_eq!(c.get("b").and_then(Value::as_bool), Some(true));
    assert_eq!(c.get("i").and_then(Value::as_i64), Some(-3));
    assert_eq!(c.get("u").and_then(Value::as_u64), Some(7));
    assert_eq!(c.get("f").and_then(Value::as_f64), Some(1.5));
    assert_eq!(c.get("s").and_then(Value::as_str), Some("文本"));
    assert!(
        c.get("arr")
            .and_then(Value::as_array)
            .is_some_and(|a| a.len() == 2)
    );
    assert!(c.get("缺失").is_none());

    // 同一个键再次写入会覆盖旧值
    c.set("b", false);
    assert_eq!(c.get("b").and_then(Value::as_bool), Some(false));

    // 版本键也可以显式改写
    c.set(KEY_VERSION, 2u64);
    assert_eq!(c.version(), 2);
}

#[test]
fn save_load_roundtrip() {
    let root = temp_root("roundtrip");
    let path = root.join("config.json");
    let mut c = Config::default();
    c.set("s", "值");
    c.set("f", 2.5);
    c.set("b", true);
    save(&c, &path).unwrap();

    // 目录里只有最终文件，没有临时文件残留
    let entries: Vec<_> = std::fs::read_dir(&root).unwrap().collect();
    assert_eq!(entries.len(), 1);

    let c2 = load(&path);
    assert_eq!(c2.get("s").and_then(Value::as_str), Some("值"));
    assert_eq!(c2.get("f").and_then(Value::as_f64), Some(2.5));
    assert_eq!(c2.get("b").and_then(Value::as_bool), Some(true));
    assert_eq!(c2.version(), CURRENT_VERSION);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn save_creates_parent_dirs() {
    let root = temp_root("mkdir");
    let path = root.join("a/b/config.json");
    save(&Config::default(), &path).unwrap();
    assert!(path.is_file());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_file_returns_default() {
    let path = temp_root("missing").join("config.json");
    let c = load(&path);
    assert_eq!(c.version(), CURRENT_VERSION);
    // 只读取不写入：不会顺手创建文件
    assert!(!path.exists());
}

#[test]
fn broken_json_archived_falls_back() {
    ensure_logger();
    let root = temp_root("broken");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    let broken = root.join("config.json.broken");

    std::fs::write(&path, "{oops").unwrap();
    let c = load(&path);
    assert_eq!(c.version(), CURRENT_VERSION);
    assert!(!path.exists());
    assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{oops");

    // 新的坏文件会覆盖上一份留档；非对象 JSON 同样算损坏
    std::fs::write(&path, "[1,2]").unwrap();
    assert_eq!(load(&path).version(), CURRENT_VERSION);
    assert_eq!(std::fs::read_to_string(&broken).unwrap(), "[1,2]");
    assert!(!path.exists());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn non_utf8_falls_back_without_renaming() {
    ensure_logger();
    let root = temp_root("nonutf8");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    std::fs::write(&path, [0xFF_u8, 0xFE]).unwrap();
    let c = load(&path);
    assert_eq!(c.version(), CURRENT_VERSION);
    // 无法按 UTF-8 解码属于读取失败，不是内容损坏：原文件保持原名，
    // 也不会产生 .broken 文件
    assert!(path.exists());
    assert!(!root.join("config.json.broken").exists());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_version_key_backfilled() {
    let root = temp_root("oldver");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    std::fs::write(&path, r#"{"a":1}"#).unwrap();
    let c = load(&path);
    assert_eq!(c.get("a").and_then(Value::as_i64), Some(1));
    assert_eq!(c.version(), CURRENT_VERSION);
    assert_eq!(
        c.get(KEY_VERSION).and_then(Value::as_u64),
        Some(CURRENT_VERSION)
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn save_fails_when_parent_is_a_file() {
    let root = temp_root("blocked");
    std::fs::create_dir_all(&root).unwrap();
    let blocker = root.join("blocker");
    std::fs::write(&blocker, b"x").unwrap();
    // 父路径被文件占用，保存失败
    assert!(save(&Config::default(), &blocker.join("config.json")).is_err());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn temp_file_removed_when_rename_fails() {
    let root = temp_root("renfail");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    std::fs::create_dir(&path).unwrap(); // 目标被目录占用 → rename Err
    assert!(save(&Config::default(), &path).is_err());
    // 目录里只剩占位的 config.json 目录
    let names: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["config.json".to_string()]);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn save_errors_without_file_name() {
    assert!(save(&Config::default(), Path::new("/")).is_err());
    assert!(save(&Config::default(), Path::new("")).is_err());
}

#[test]
fn env_var_overrides_config_path() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_root("env");
    let p = root.join("custom.json");
    unsafe {
        std::env::set_var("DS_PET_CONFIG", &p);
    }
    assert_eq!(config_path(), p);
    unsafe {
        std::env::remove_var("DS_PET_CONFIG");
    }
}

#[test]
fn default_path_matches_platform_convention() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let p = config_path(); // 无 DS_PET_CONFIG 时
    // 各平台的期望路径直接写死，不运行时推断
    #[cfg(target_os = "macos")]
    assert!(p.ends_with("Library/Application Support/ds-pet/config.json"));
    #[cfg(target_os = "windows")]
    assert!(p.ends_with(r"ds-pet\config.json"));
    #[cfg(all(unix, not(target_os = "macos")))]
    assert!(p.ends_with("ds-pet/config.json"));
}

/// 全局 API 综合用例。OnceLock 进程内只初始化一次，所以凡是触碰全局的
/// 断言必须集中在同一个测试里，且先于断言设置 `DS_PET_CONFIG`。
#[test]
fn global_init_read_write_and_persistence() {
    ensure_logger();
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_root("global");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    unsafe {
        std::env::set_var("DS_PET_CONFIG", &path);
    }

    // 文件不存在用默认值；init 返回首次确定的路径
    assert_eq!(init(), path);
    assert_eq!(get_u64(KEY_VERSION), Some(CURRENT_VERSION));

    // 各类型读写
    set("b", true);
    set("i", -2i64);
    set("f", 0.5f64);
    set("s", "喵");
    assert_eq!(get_bool("b"), Some(true));
    assert_eq!(get_i64("i"), Some(-2));
    assert_eq!(get_f64("f"), Some(0.5));
    assert_eq!(get_str("s"), Some("喵".to_string()));
    assert_eq!(
        get("s").and_then(|v| v.as_str().map(str::to_string)),
        Some("喵".to_string())
    );
    assert_eq!(get_bool("nope"), None);
    // 调用 set 之后，配置立刻写入磁盘
    assert!(path.is_file());

    // 并发写入的键最终都在磁盘上
    let mut handles = Vec::new();
    for t in 0..4u32 {
        handles.push(std::thread::spawn(move || {
            for i in 0..25u32 {
                set(&format!("t{t}k{i}"), f64::from(i));
                let _ = get_f64("f"); // 写入期间穿插读
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let on_disk = load(&path);
    for t in 0..4u32 {
        for i in 0..25u32 {
            let key = format!("t{t}k{i}");
            assert_eq!(
                on_disk.get(&key).and_then(Value::as_f64),
                Some(f64::from(i)),
                "键 {key} 应已写进磁盘文件"
            );
        }
    }

    // 保存失败不影响内存值
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, b"x").unwrap(); // 父目录路径被文件占用
    set("after_fail", 1u64);
    assert_eq!(get_u64("after_fail"), Some(1));
    std::fs::remove_file(&root).ok();
    unsafe {
        std::env::remove_var("DS_PET_CONFIG");
    }
}

#[test]
fn clamp_opacity_bounds() {
    assert_eq!(clamp_opacity(0.0), OPACITY_MIN);
    assert_eq!(clamp_opacity(2.5), OPACITY_MAX);
    assert_eq!(clamp_opacity(0.55), 0.55);
    assert_eq!(clamp_opacity(OPACITY_MIN), OPACITY_MIN);
    assert_eq!(clamp_opacity(OPACITY_MAX), OPACITY_MAX);
}
