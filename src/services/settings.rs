//! 应用设置持久化：主题与语言选择保存到 `%APPDATA%\x264video4osu\settings.ini`。
//!
//! 格式为简单的 `key=value` 两行（与旧程序 Settings 的扁平 INI 风格一致）：
//!
//! ```ini
//! lang=0
//! theme=1
//! ```
//!
//! 解析时对非法值静默忽略（回落到默认），写入时 best-effort 忽略 IO 错误，
//! 设置文件损坏/缺失不会影响程序启动。

use std::fs;
use std::path::PathBuf;

/// 主题枚举。索引即设置文件里存的值，也即 Slint 侧 `theme-index`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// 默认主题（老主题）：`#FAFAFA` 白底、`#0078D4` 蓝、系统原生标题栏
    Default = 0,
    /// Aero 主题：天蓝天空渐变底、aqua 主色、自绘玻璃标题栏
    Aero = 1,
}

impl Theme {
    pub fn is_aero(self) -> bool {
        self == Theme::Aero
    }

    pub fn from_index(i: i32) -> Theme {
        match i {
            1 => Theme::Aero,
            _ => Theme::Default,
        }
    }
}

/// 应用设置。`lang_index` 对应 i18n::Lang（0=中文，1=English）。
#[derive(Debug, Clone, Copy)]
pub struct AppSettings {
    pub lang_index: i32,
    pub theme_index: i32,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            lang_index: 0,
            theme_index: 0, // 首次运行默认主题（老主题）
        }
    }
}

impl AppSettings {
    pub fn theme(&self) -> Theme {
        Theme::from_index(self.theme_index)
    }
}

/// 应用设置目录：`%APPDATA%\x264video4osu`；APPDATA 缺省时回落 exe 所在目录。
fn settings_dir() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("x264video4osu"),
        _ => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_default(),
    }
}

fn settings_path() -> PathBuf {
    settings_dir().join("settings.ini")
}

/// 读取设置文件；不存在或解析失败时返回默认值（不报错）。
pub fn load() -> AppSettings {
    let mut settings = AppSettings::default();
    let Ok(content) = fs::read_to_string(settings_path()) else {
        return settings;
    };

    for line in content.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "lang" => {
                    if let Ok(v) = value.trim().parse::<i32>() {
                        settings.lang_index = v;
                    }
                }
                "theme" => {
                    if let Ok(v) = value.trim().parse::<i32>() {
                        settings.theme_index = v;
                    }
                }
                _ => {}
            }
        }
    }
    settings
}

/// 保存设置；best-effort，目录不存在则创建，IO 错误忽略。
pub fn save(settings: &AppSettings) {
    let dir = settings_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("warning: failed to create settings dir {}: {e}", dir.display());
        return;
    }
    let content = format!("lang={}\ntheme={}\n", settings.lang_index, settings.theme_index);
    if let Err(e) = fs::write(settings_path(), content) {
        eprintln!("warning: failed to save settings: {e}");
    }
}
