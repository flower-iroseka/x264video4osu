//! UI 字符串（对应旧程序 `Resources/Strings.zh-CN.xaml` 与 `Strings.en-US.xaml`，
//! 以及旧界面中硬编码的英文文案）。
//!
//! 注意：旧程序中「Open Output Folder」按钮文案是硬编码英文，不随语言切换；
//! 进度/结果文本（Pass / ETA / Elapsed / Output size）同样是硬编码英文。
//! 校验失败提示（Input file path cannot be empty. 等）也是硬编码英文，
//! 因此这里两种语言下保持一致。

use crate::services::ffmpeg_config::tools_folder;
use crate::UiStrings;

/// 语言
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

/// 全部可切换文本 + 各对话框文案
pub struct Strings {
    /// 主窗口控件文本
    pub ui: UiStrings,
    /// 关于对话框：标题、版本行、描述
    pub about_title: String,
    pub about_version_line: String,
    pub about_description: String,
    pub repository_text: String,
    pub close_text: String,
    /// FFmpeg 缺失对话框
    pub not_found_title: String,
    pub not_found_message: String,
    pub download_text: String,
    pub exit_text: String,
    /// 消息对话框确认按钮
    pub ok_text: String,
}

fn zh() -> Strings {
    Strings {
        ui: UiStrings {
            tab_video: "视频".into(),
            tab_log: "日志".into(),
            label_input: "视频输入".into(),
            label_output: "视频输出".into(),
            button_browse: "浏览".into(),
            label_format: "输出格式".into(),
            format_tip: "osu stable版本存在既有bug，mp4格式的视频会导致游戏随机崩溃，建议使用flv或者avi格式".into(),
            label_method: "编码方法".into(),
            label_resolution: "分辨率".into(),
            label_width: "宽度".into(),
            label_height: "高度".into(),
            check_scale_up: "允许强制放大".into(),
            check_extract_audio: "分离音频流".into(),
            button_start: "开始".into(),
            button_stop: "停止".into(),
            check_auto_scroll: "自动滚动".into(),
            button_save_log: "保存日志".into(),
            button_about: "关于".into(),
            // 旧程序硬编码英文，不随语言切换
            button_open_folder: "Open Output Folder".into(),
        },
        about_title: "关于".into(),
        about_version_line: format!("版本 {0}", display_version()),
        about_description: "osu! 视频压制工具\n\n使用 x264 编码器为 osu! 制作兼容的背景视频。".into(),
        repository_text: "项目仓库".into(),
        close_text: "关闭".into(),
        not_found_title: "未找到 FFmpeg".into(),
        not_found_message: format!(
            "FFmpeg 工具未找到！\n\n请将以下文件放入 tools 文件夹：\n• ffmpeg.exe\n• ffprobe.exe\n\ntools 文件夹位置：{0}",
            tools_folder().to_string_lossy()
        ),
        download_text: "下载 FFmpeg".into(),
        exit_text: "退出".into(),
        ok_text: "确定".into(),
    }
}

fn en() -> Strings {
    Strings {
        ui: UiStrings {
            tab_video: "Video".into(),
            tab_log: "Log".into(),
            label_input: "Video Input".into(),
            label_output: "Video Output".into(),
            button_browse: "Browse".into(),
            label_format: "Format".into(),
            format_tip: "osu! stable has a known bug where mp4 videos cause random game crashes. We recommend using flv or avi format.".into(),
            label_method: "Method".into(),
            label_resolution: "Resolution".into(),
            label_width: "Width".into(),
            label_height: "Height".into(),
            check_scale_up: "Allow upscale".into(),
            check_extract_audio: "Extract audio stream".into(),
            button_start: "Start".into(),
            button_stop: "Stop".into(),
            check_auto_scroll: "Auto scroll".into(),
            button_save_log: "Save Log".into(),
            button_about: "About".into(),
            button_open_folder: "Open Output Folder".into(),
        },
        about_title: "About".into(),
        about_version_line: format!("Version {0}", display_version()),
        about_description: "osu! video compression tool\n\nUses x264 encoder to create osu! compatible background video.".into(),
        repository_text: "Project Repository".into(),
        close_text: "Close".into(),
        not_found_title: "FFmpeg Not Found".into(),
        not_found_message: format!(
            "FFmpeg tools not found!\n\nPlease place the following files in the tools folder:\n• ffmpeg.exe\n• ffprobe.exe\n\ntools folder location: {0}",
            tools_folder().to_string_lossy()
        ),
        download_text: "Download FFmpeg".into(),
        exit_text: "Exit".into(),
        ok_text: "OK".into(),
    }
}

/// 构造指定语言的字符串集
pub fn for_lang(lang: Lang) -> Strings {
    match lang {
        Lang::Zh => zh(),
        Lang::En => en(),
    }
}

/// 默认语言（旧程序启动加载 zh-CN，语言框默认索引 0）
pub fn default_lang() -> Lang {
    Lang::Zh
}

/// 版本号展示：只取 `+` 之前的部分（对应 `version.Split('+')[0]`）。
/// 用 `AssemblyInformationalVersion` 的等价替代：Cargo 包版本。
fn display_version() -> &'static str {
    let v = env!("CARGO_PKG_VERSION");
    v.split('+').next().unwrap_or(v)
}
