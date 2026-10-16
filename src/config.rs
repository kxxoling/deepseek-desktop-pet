//! 用户级 JSON 配置的读写，进程内全局共享。
//!
//! 文件默认放在各平台的用户目录（`DS_PET_CONFIG` 可覆盖路径）；保存先写
//! 临时文件再 rename，崩溃不留半个文件；损坏的配置回退默认值，原文件
//! 改名 `config.json.broken` 备查。键值是开放结构：加配置项只需在调用处
//! 读写新键名，不需要改这个文件。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, PoisonError, RwLock};

use serde_json::{Map, Value};

/// 结构版本键，将来改配置格式时按它迁移
const KEY_VERSION: &str = "config_version";
const CURRENT_VERSION: u64 = 1;

/// 键值配置。类型化读写走全局 API 或 [`Config::get`] + `Value::as_*`
pub struct Config {
    values: Map<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        let mut values = Map::new();
        values.insert(KEY_VERSION.to_string(), Value::from(CURRENT_VERSION));
        Self { values }
    }
}

impl Config {
    /// 原始取值（数组/对象等复杂类型经此访问）
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// 写入内存；持久化走 [`save`] 或全局 [`set`]
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.values.insert(key.to_string(), value.into());
    }

    /// 结构版本（旧文件缺版本键时视为当前版本）
    pub fn version(&self) -> u64 {
        self.get(KEY_VERSION)
            .and_then(Value::as_u64)
            .unwrap_or(CURRENT_VERSION)
    }
}

/// 配置文件路径：`DS_PET_CONFIG` 优先，其次平台约定
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("DS_PET_CONFIG") {
        return PathBuf::from(p);
    }
    platform_config_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("ds-pet"))
        .join("config.json")
}

/// 平台约定的配置目录，取法与日志目录一致
fn platform_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| {
            let mut p = PathBuf::from(h);
            p.push("Library/Application Support/ds-pet");
            p
        })
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(r"ds-pet"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(config.join("ds-pet"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// 读取配置；出问题一律回退默认值，确认损坏的文件改名 `.broken` 备查
pub fn load(path: &Path) -> Config {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(e) => {
            log::warn!("配置读取失败（{e}）：{}，使用默认值", path.display());
            return Config::default();
        }
    };
    match serde_json::from_str::<Map<String, Value>>(&data) {
        Ok(mut values) => {
            // 缺版本键的旧文件补上，下次保存时写进去
            values
                .entry(KEY_VERSION.to_string())
                .or_insert_with(|| Value::from(CURRENT_VERSION));
            Config { values }
        }
        Err(e) => {
            log::warn!(
                "配置损坏（{e}）：{}，改用默认配置，原文件改名保存",
                path.display()
            );
            if let Some(name) = file_name(path) {
                let broken = path.with_file_name(format!("{name}.broken"));
                let _ = std::fs::remove_file(&broken); // 新的留档会覆盖上一份
                if let Err(r) = std::fs::rename(path, &broken) {
                    log::warn!("坏配置改名保存失败（{r}）：{}", broken.display());
                }
            }
            Config::default()
        }
    }
}

/// 保存：先写临时文件再 rename，多实例同时保存互不干扰
pub fn save(cfg: &Config, path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let Some(name) = file_name(path) else {
        return Err(std::io::Error::other(format!(
            "配置路径缺少文件名：{}",
            path.display()
        )));
    };
    let json = serde_json::to_string_pretty(&cfg.values)
        .map_err(|e| std::io::Error::other(format!("配置序列化失败：{e}")))?;
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, json)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp); // rename 失败不残留临时文件
        return Err(e);
    }
    Ok(())
}

// ---------- 进程内全局单例 ----------

struct Global {
    /// 配置文件路径（首次访问时固定）
    path: PathBuf,
    cfg: RwLock<Config>,
}

fn global() -> &'static Global {
    static GLOBAL: OnceLock<Global> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let path = config_path();
        let cfg = load(&path);
        Global {
            path,
            cfg: RwLock::new(cfg),
        }
    })
}

/// 启动时调用一次，返回配置文件路径
pub fn init() -> PathBuf {
    global().path.clone()
}

/// 拿读锁访问全局配置；锁中毒不影响读取
fn with_global<R>(f: impl FnOnce(&Config) -> R) -> R {
    let guard = global().cfg.read().unwrap_or_else(PoisonError::into_inner);
    f(&guard)
}

/// 原始取值（克隆；复杂类型用）
pub fn get(key: &str) -> Option<Value> {
    with_global(|c| c.get(key).cloned())
}

pub fn get_bool(key: &str) -> Option<bool> {
    with_global(|c| c.get(key).and_then(Value::as_bool))
}

pub fn get_i64(key: &str) -> Option<i64> {
    with_global(|c| c.get(key).and_then(Value::as_i64))
}

pub fn get_u64(key: &str) -> Option<u64> {
    with_global(|c| c.get(key).and_then(Value::as_u64))
}

pub fn get_f64(key: &str) -> Option<f64> {
    with_global(|c| c.get(key).and_then(Value::as_f64))
}

pub fn get_str(key: &str) -> Option<String> {
    with_global(|c| c.get(key).and_then(Value::as_str).map(str::to_string))
}

/// 写入并立即保存到磁盘；失败只记日志，不影响内存值
pub fn set(key: &str, value: impl Into<Value>) {
    let g = global();
    let mut cfg = g.cfg.write().unwrap_or_else(PoisonError::into_inner);
    cfg.set(key, value);
    if let Err(e) = save(&cfg, &g.path) {
        log::error!("配置写盘失败（{e}）：{}", g.path.display());
    }
}

#[allow(unsafe_code)] // 测试内修改进程环境变量（edition 2024 要求 unsafe 块）
#[allow(clippy::wildcard_imports)] // 测试文件整批引入被测模块的项
#[path = "config_test.rs"]
mod tests;
