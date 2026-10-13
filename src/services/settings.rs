//! 应用设置持久化：界面语言保存在 exe 同目录的 `settings.ini`。
//!
//! 格式为简单的 `key=value` 单行（与旧程序 Settings 的扁平 INI 风格一致）：
//!
//! ```ini
//! lang=0
//! preview=0
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

/// 应用设置。
#[derive(Debug, Clone, Copy)]
pub struct AppSettings {
    /// 界面语言，对应 i18n::Lang（0=中文，1=English）
    pub lang_index: i32,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings { lang_index: 0 }
    }
}

/// 应用设置目录：exe 所在目录。
///
/// 程序是免安装的，设置跟着 exe 走 —— 把整个文件夹拷到别的机器上，语言选择
/// 也一起带过去。不用 `%APPDATA%`，否则「绿色版」换台机器设置就丢了。
///
/// 这个目录不一定可写（比如放在 Program Files 下面）。写不进去时 `save()`
/// 会打一条警告然后放弃，只是记不住语言，不影响程序运行。
fn settings_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_default()
}

fn settings_path() -> PathBuf {
    settings_dir().join("settings.ini")
}

/// 读取设置文件；不存在或解析失败时返回默认值（不报错）。
pub fn load() -> AppSettings {
    match fs::read_to_string(settings_path()) {
        Ok(content) => parse_settings(&content),
        Err(_) => AppSettings::default(),
    }
}

/// 解析设置文件内容。无法识别的行、以及超范围的值一律忽略，保留默认。
/// 单独拆出来是为了能无头测试 —— `load()` 要读磁盘上的真实文件，测不了。
fn parse_settings(content: &str) -> AppSettings {
    let mut settings = AppSettings::default();
    for line in content.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "lang" => {
                if let Ok(v) = value.trim().parse::<i32>() {
                    settings.lang_index = clamp_lang_index(v);
                }
            }
            // 旧版本存过 "preview="（预览面板展开状态）。现在预览一律从收起开始，
            // 不再记这个值，读到就忽略，免得老设置文件被当成坏文件。
            _ => {}
        }
    }
    settings
}

/// 保存设置；best-effort，IO 错误忽略（exe 放在只读目录里时就会写失败）。
pub fn save(settings: &AppSettings) {
    let content = format!("lang={}\n", settings.lang_index);
    if let Err(e) = fs::write(settings_path(), content) {
        eprintln!("warning: failed to save settings: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_file_sits_next_to_the_exe() {
        // 绿色版的要求：设置放在 exe 旁边，拷走文件夹就带走了语言选择。
        // 别改回 %APPDATA%，那样换台机器设置就没了。
        let exe_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(settings_path(), exe_dir.join("settings.ini"));
    }

    #[test]
    fn lang_index_is_clamped_to_known_languages() {
        assert_eq!(clamp_lang_index(0), 0);
        assert_eq!(clamp_lang_index(1), 1);
        // 配置文件被手改成 5，或者从加了第三门语言的版本退回旧版
        assert_eq!(clamp_lang_index(5), 1);
        assert_eq!(clamp_lang_index(-3), 0);
        assert_eq!(clamp_lang_index(i32::MAX), 1);
    }

    #[test]
    fn parses_lang_and_ignores_other_keys() {
        assert_eq!(parse_settings("lang=1\n").lang_index, 1);
        assert_eq!(parse_settings("lang=0\n").lang_index, 0);
        // 老版本写过的 preview 行现在被忽略，但不能让它把文件当成坏的
        assert_eq!(parse_settings("lang=1\npreview=1\n").lang_index, 1);
        assert_eq!(parse_settings("preview=1\nlang=1\n").lang_index, 1);
    }

    #[test]
    fn missing_or_broken_values_keep_defaults() {
        // 空文件 / 垃圾内容
        assert_eq!(parse_settings("").lang_index, 0);
        assert_eq!(parse_settings("这不是配置").lang_index, 0);
        assert_eq!(parse_settings("preview=yes\n").lang_index, 0);

        // lang 越界会被夹回来（和已有的那条测试同一个入口）
        assert_eq!(parse_settings("lang=9\n").lang_index, 1);
    }
}
