//! 玩家存档（#984）：一个**买了就存档、重开还在**的最小持久化层。
//!
//! 引擎自带的 `scene_state.json` 只存场景（相机 / 变换 / 灯光），玩家的进度
//! （钓到几条鱼、卖了多少钱、解锁了什么）没有地方放，只能每个工程自己写文件——
//! 于是每个工程都要各自处理「存哪、怎么原子写、版本升级怎么办」。这里统一掉：
//!
//! - 位置由 [`user_data_dir`] 决定（Windows `%APPDATA%`、macOS
//!   `Application Support`、Linux `XDG_DATA_HOME`），Android 由宿主把
//!   `PATH` 指到 `filesDir`（[`ENV_OVERRIDE`]）。
//! - 内容是一张 **字符串键值表**（[`UserData`]），带 `version`。引擎不理解
//!   业务 schema，工程自己把结构序列化成字符串塞进来，升级时自己判断版本。
//! - 写入是**原子**的（临时文件 + rename），中途崩溃不会留下半个存档。
//! - 读到比 [`USER_DATA_VERSION`] 更高的版本时**不覆盖**：返回错误并保持原文件，
//!   避免新版存档被旧版客户端清空。
//!
//! 用法（工程侧）：
//!
//! ```ignore
//! world.insert_resource(UserStore::open(save_path("my-game")));
//! // 读
//! let caught = store.get("caught").unwrap_or("0");
//! // 写（内存 + 落盘）
//! store.set("caught", "3");
//! store.flush()?;
//! ```

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 存档格式版本。字段语义变化时才加（新增键不需要）。
pub const USER_DATA_VERSION: u32 = 1;

/// 覆盖用户数据根目录的环境变量（Android 宿主 / 便携版 / 测试用）。
pub const ENV_OVERRIDE: &str = "PRISMREV_USER_DATA";

/// 落盘的文件名：放在每个工程自己的目录下。
pub const FILE_NAME: &str = "player_data.json";

/// Reserved namespace used by the engine-owned global flag API.
pub const FLAG_STORAGE_PREFIX: &str = prism_project_api::GLOBAL_FLAG_STORAGE_PREFIX;

/// 存档文件内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserData {
    /// [`USER_DATA_VERSION`]；缺省按 1 处理（第一版没有写版本号也能读）。
    #[serde(default = "default_version")]
    pub version: u32,
    /// 字符串键值表。键由工程自己约定（`"caught"` / `"coins"` / `"quest.mayor"`）。
    #[serde(default)]
    pub entries: BTreeMap<String, String>,
}

fn default_version() -> u32 {
    1
}

impl Default for UserData {
    fn default() -> Self {
        Self {
            version: USER_DATA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

impl UserData {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.entries.insert(key.into(), value.into());
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.entries.remove(key)
    }

    /// 解析文件内容。
    ///
    /// 比当前引擎更新的存档返回 `Err`——调用方据此**不要写回**，否则新版
    /// 玩家降级回旧版客户端就会丢档。
    pub fn parse(text: &str) -> Result<Self, String> {
        let data: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if data.version > USER_DATA_VERSION {
            return Err(format!(
                "save data version {} is newer than this build ({USER_DATA_VERSION})",
                data.version
            ));
        }
        Ok(data)
    }

    pub fn to_json(&self) -> String {
        // 存档给人看得懂、可手改；只写两个字段，不怕字段顺序变。
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// 用户数据根目录（每个工程一个子目录）。
///
/// 顺序：`PRISMREV_USER_DATA` → 平台惯例目录。返回的路径**不保证存在**，
/// 由 [`UserStore::flush`] 按需创建。
pub fn user_data_dir(project: &str) -> PathBuf {
    let root = if let Some(override_dir) = std::env::var_os(ENV_OVERRIDE) {
        PathBuf::from(override_dir)
    } else if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("PrismaRev")
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library/Application Support/PrismaRev")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".local/share")
            })
            .join("prismrev")
    };
    root.join(sanitize(project))
}

/// 某个工程的存档文件路径。
pub fn save_path(project: &str) -> PathBuf {
    user_data_dir(project).join(FILE_NAME)
}

/// 工程名里可能有斜杠 / 冒号，落盘前统一替换，避免越出用户数据目录。
fn sanitize(project: &str) -> String {
    let cleaned: String = project
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

/// 运行时存档状态：内存里的键值表 + 它属于哪个文件。
///
/// 作为 ECS resource 挂在 `World` 上，所以 `GameWorld`（host 后端）和引擎
/// 系统都能直接读写，不需要额外通道。
#[derive(Debug, Clone)]
pub struct UserStore {
    path: Option<PathBuf>,
    data: UserData,
    /// 读到的是更高版本的存档：只读，不覆盖。
    read_only: bool,
    dirty: bool,
}

impl Default for UserStore {
    fn default() -> Self {
        Self {
            path: None,
            data: UserData::default(),
            read_only: false,
            dirty: false,
        }
    }
}

impl UserStore {
    /// 只放内存（不上盘）：单元测试 / 不支持文件系统的宿主。
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// 绑定一个存档文件并读取（文件不存在 = 空存档，不算错误）。
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut store = Self {
            path: Some(path.clone()),
            ..Self::default()
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match UserData::parse(&text) {
                Ok(data) => store.data = data,
                Err(error) => {
                    // 解析失败：保留原文件（可能来自更新的版本），本次只读。
                    log::warn!(
                        "player data {}: {error}; keeping the file untouched",
                        path.display()
                    );
                    store.read_only = true;
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                log::warn!("player data {}: {error}", path.display());
                store.read_only = true;
            }
        }
        store
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn data(&self) -> &UserData {
        &self.data
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key)
    }

    /// Returns an engine-owned persistent flag. Missing flags are `false`.
    pub fn get_flag(&self, key: &str) -> bool {
        self.data
            .get(&format!("{FLAG_STORAGE_PREFIX}{key}"))
            .is_some_and(|value| value == "1")
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        if self.read_only {
            log::warn!("player data is read-only (newer version on disk); ignoring write");
            return;
        }
        self.data.set(key, value);
        self.dirty = true;
    }

    /// Sets an engine-owned persistent flag. The caller decides when to flush.
    pub fn set_flag(&mut self, key: &str, value: bool) {
        self.set(
            format!("{FLAG_STORAGE_PREFIX}{key}"),
            if value { "1" } else { "0" },
        );
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        if self.read_only {
            return None;
        }
        let removed = self.data.remove(key);
        if removed.is_some() {
            self.dirty = true;
        }
        removed
    }

    /// 有没有还没落盘的改动。
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 写入磁盘（临时文件 + rename，断电不会留半个存档）。
    ///
    /// 没有绑定路径、只读、或没有改动时直接返回 `Ok(false)`。
    pub fn flush(&mut self) -> io::Result<bool> {
        if self.read_only || !self.dirty {
            return Ok(false);
        }
        let Some(path) = self.path.clone() else {
            self.dirty = false;
            return Ok(false);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, self.data.to_json())?;
        std::fs::rename(&temp, &path)?;
        self.dirty = false;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "user_data_tests.rs"]
mod user_data_tests;
