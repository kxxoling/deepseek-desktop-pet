//! 日志落盘：stderr 双写 + panic 捕获，日志文件可供用户作为 issue 附件。
//!
//! 位置（`DS_PET_LOG_DIR` 可覆盖）：macOS `~/Library/Logs/ds-pet/`、
//! Windows `%LOCALAPPDATA%\ds-pet\logs\`、Linux `$XDG_STATE_HOME/ds-pet/logs/`。
//! 追加写入，超 1MB 轮转为 `ds-pet.log.old`（保留一代）。时间戳为 UTC。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// 日志目录：`DS_PET_LOG_DIR` 优先，其次平台约定
pub fn log_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DS_PET_LOG_DIR") {
        return PathBuf::from(p);
    }
    platform_log_dir().unwrap_or_else(|| std::env::temp_dir().join("ds-pet-logs"))
}

fn platform_log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs/ds-pet"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|a| PathBuf::from(a).join(r"ds-pet\logs"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        Some(state.join("ds-pet/logs"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// 超限时改名为 `ds-pet.log.old`（保留一代）
fn rotate_if_large(path: &Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let old = path.with_extension("log.old");
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(path, &old) {
        eprintln!("日志轮转失败：{e}");
    }
}

/// 双写：文件 + stderr
struct Tee {
    file: Mutex<std::fs::File>,
}

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 文件写失败不阻塞日志通道
        let _ = self.file.lock().map(|mut f| f.write_all(buf));
        let _ = std::io::stderr().write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.file.lock().map(|mut f| f.flush());
        std::io::stderr().flush()
    }
}

/// 初始化日志：文件 + stderr 双写，panic 捕获落盘。
/// 返回日志文件路径；目录创建/打开失败返回 None（回退纯 stderr，不阻塞启动）。
pub fn init() -> Option<PathBuf> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("ds-pet.log");
    rotate_if_large(&path, MAX_LOG_BYTES);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;

    // 会话头：构建标识 / PID / 平台 / 日志路径
    let _ = writeln!(
        file,
        "==== ds-pet {} 会话开始 pid={} os={}/{} log={} ====",
        crate::build_info(),
        std::process::id(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        path.display()
    );

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    builder.target(env_logger::Target::Pipe(Box::new(Tee {
        file: Mutex::new(file),
    })));
    let _ = builder.try_init();

    // panic 连同 backtrace 写入日志，再交回默认 hook
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic：{info}");
        log::error!(
            "backtrace：\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        default_hook(info);
    }));

    Some(path)
}

#[cfg(test)]
#[allow(unsafe_code)] // 测试内修改进程环境变量（edition 2024 要求 unsafe 块）
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ds-pet-log-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn log_dir_env_takes_priority() {
        let root = temp_root("dir");
        unsafe {
            std::env::set_var("DS_PET_LOG_DIR", &root);
        }
        assert_eq!(log_dir(), root);
        unsafe {
            std::env::remove_var("DS_PET_LOG_DIR");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rotate_over_limit_keeps_one() {
        let root = temp_root("rotate");
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("ds-pet.log");

        // 超限 → 改名 .old
        let mut f = std::fs::File::create(&log).unwrap();
        f.write_all(&vec![b'x'; MAX_LOG_BYTES as usize + 1])
            .unwrap();
        drop(f);
        rotate_if_large(&log, MAX_LOG_BYTES);
        assert!(!log.exists());
        assert!(root.join("ds-pet.log.old").exists());

        // 未超限 → 保持原文件
        let mut f = std::fs::File::create(&log).unwrap();
        f.write_all(b"small").unwrap();
        drop(f);
        rotate_if_large(&log, MAX_LOG_BYTES);
        assert!(log.exists());

        std::fs::remove_dir_all(&root).ok();
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;

    /// 环境变量互斥：`DS_PET_LOG_DIR` 的设置/读取测试串行执行
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ds-pet-log2-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn default_log_dir_matches_platform() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = log_dir(); // 无 DS_PET_LOG_DIR 时
        #[cfg(target_os = "macos")]
        assert!(dir.ends_with("Library/Logs/ds-pet"));
        #[cfg(target_os = "windows")]
        assert!(dir.ends_with(r"ds-pet\logs"));
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(dir.ends_with("ds-pet/logs"));
    }

    #[test]
    fn init_writes_and_is_idempotent() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_root("init");
        unsafe {
            std::env::set_var("DS_PET_LOG_DIR", &root);
        }
        let path = init().expect("init 应成功");
        assert_eq!(path, root.join("ds-pet.log"));
        assert!(path.exists());
        // logger 全局只装一次，第二次 init 仍应成功（文件可再开）
        assert!(init().is_some());
        // 触发一次 panic，验证全局 hook 不破坏日志通道
        let _ = std::panic::catch_unwind(|| panic!("hook 覆盖测试"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("会话开始"));
        unsafe {
            std::env::remove_var("DS_PET_LOG_DIR");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn init_returns_none_when_dir_unbuildable() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = std::env::temp_dir().join(format!(
            "ds-pet-log-file-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&file, b"x").unwrap(); // 用文件占用目录路径
        unsafe {
            std::env::set_var("DS_PET_LOG_DIR", &file);
        }
        assert!(init().is_none());
        unsafe {
            std::env::remove_var("DS_PET_LOG_DIR");
        }
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn tee_writes_file_and_stderr() {
        use std::io::Write as _;
        let root = temp_root("tee");
        std::fs::create_dir_all(&root).unwrap();
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("t.log"))
            .unwrap();
        let mut tee = Tee {
            file: Mutex::new(f),
        };
        tee.write_all(b"hello ").unwrap();
        tee.write_all(b"world\n").unwrap();
        tee.flush().unwrap();
        let content = std::fs::read_to_string(root.join("t.log")).unwrap();
        assert_eq!(content, "hello world\n");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn occupied_rotation_warns_not_panics() {
        let root = temp_root("rotfail");
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("ds-pet.log");
        std::fs::write(&log, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        std::fs::create_dir_all(root.join("ds-pet.log.old")).unwrap(); // rename 目标是目录 → Err
        rotate_if_large(&log, MAX_LOG_BYTES);
        assert!(log.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rotation_boundary_exactly_at_limit() {
        let root = temp_root("rotedge");
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("ds-pet.log");
        std::fs::write(&log, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        rotate_if_large(&log, MAX_LOG_BYTES);
        assert!(log.exists()); // == 上限不动
        assert!(!root.join("ds-pet.log.old").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
