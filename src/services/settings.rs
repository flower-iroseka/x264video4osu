//! 应用设置持久化：界面语言保存到 `%APPDATA%\x264video4osu\settings.ini`。
//!
//! 格式为简单的 `key=value` 单行（与旧程序 Settings 的扁平 INI 风格一致）：
//!
//! ```ini
//! lang=0
//! ```
//!
//! 解析时对非法值静默忽略（回落到默认），写入时 best-effort 忽略 IO 错误，
//! 设置文件损坏/缺失不会影响程序启动。旧版本写过的 `theme=` 行会被忽略。

use std::fs;
use std::path::PathBuf;

/// 界面语言一共几种（0=中文，1=English）。
const LANG_COUNT: i32 = 2;

/// 把语言编号夹回有效范围。
///
/// 文件里的值不受程序控制：可能被手改过，可能写坏了，也可能来自以后加了
/// 第三门语言的版本。不夹的话，这个数字会直接传给界面上的语言下拉框当
/// 「选中第几项」，项不存在时下拉框就显示成空白。
fn clamp_lang_index(v: i32) -> i32 {
    v.clamp(0, LANG_COUNT - 1)
}

/// 应用设置。`lang_index` 对应 i18n::Lang（0=中文，1=English）。
#[derive(Debug, Clone, Copy)]
pub struct AppSettings {
    pub lang_index: i32,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings { lang_index: 0 }
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
                        settings.lang_index = clamp_lang_index(v);
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
    let content = format!("lang={}\n", settings.lang_index);
    if let Err(e) = fs::write(settings_path(), content) {
        eprintln!("warning: failed to save settings: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_index_is_clamped_to_known_languages() {
        assert_eq!(clamp_lang_index(0), 0);
        assert_eq!(clamp_lang_index(1), 1);
        // 配置文件被手改成 5，或者从加了第三门语言的版本退回旧版
        assert_eq!(clamp_lang_index(5), 1);
        assert_eq!(clamp_lang_index(-3), 0);
        assert_eq!(clamp_lang_index(i32::MAX), 1);
    }
}
