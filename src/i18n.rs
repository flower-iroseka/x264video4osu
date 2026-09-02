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
    /// 标题栏按钮工具提示
    pub titlebar_minimize: String,
    pub titlebar_maximize: String,
    pub titlebar_close: String,
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
            // ---- 主题下拉框 ----
            theme_default: "默认".into(),
            theme_aero: "Aero".into(),
            // ---- 悬停工具提示 ----
            tip_theme: "切换主题".into(),
            tip_browse_input: "选择要压缩的视频文件".into(),
            tip_browse_output: "选择输出文件的保存位置".into(),
            tip_start: "开始编码".into(),
            tip_stop: "停止编码".into(),
            tip_about: "查看程序信息".into(),
            tip_open_folder: "打开输出文件所在文件夹".into(),
            tip_save_log: "将日志保存为文本文件".into(),
            tip_format: "选择输出视频的封装格式".into(),
            tip_language: "切换界面语言".into(),
            tip_scale_up: "允许将视频放大到指定分辨率".into(),
            tip_extract_audio: "将音频流分离为独立文件".into(),
            tip_auto_scroll: "追加日志时自动滚动到底部".into(),
            tip_titlebar_minimize: "最小化".into(),
            tip_titlebar_maximize: "最大化".into(),
            tip_titlebar_close: "关闭窗口".into(),
            tip_input_path: "输入视频文件路径".into(),
            tip_output_path: "输出视频文件路径".into(),
            tip_value: "数值：2pass 为目标码率（kbps），CRF 为质量（0-51）".into(),
            tip_width: "输出宽度，0 表示保持原始宽度".into(),
            tip_height: "输出高度，0 表示保持原始高度".into(),
            tip_method: "编码方法：CRF 恒定质量，或 2pass 精确控制码率".into(),
            tip_resolution: "输出分辨率".into(),
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
        titlebar_minimize: "最小化".into(),
        titlebar_maximize: "最大化".into(),
        titlebar_close: "关闭窗口".into(),
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
            // ---- 主题下拉框 ----
            theme_default: "Default".into(),
            theme_aero: "Aero".into(),
            // ---- 悬停工具提示 ----
            tip_theme: "Switch theme".into(),
            tip_browse_input: "Select the video file to compress".into(),
            tip_browse_output: "Choose where to save the output file".into(),
            tip_start: "Start encoding".into(),
            tip_stop: "Stop encoding".into(),
            tip_about: "Show program information".into(),
            tip_open_folder: "Open the folder containing the output file".into(),
            tip_save_log: "Save the log to a text file".into(),
            tip_format: "Choose the output video container format".into(),
            tip_language: "Switch the interface language".into(),
            tip_scale_up: "Allow the video to be upscaled to the specified resolution".into(),
            tip_extract_audio: "Separate the audio stream into its own file".into(),
            tip_auto_scroll: "Scroll to the bottom automatically as new log lines are added".into(),
            tip_titlebar_minimize: "Minimize".into(),
            tip_titlebar_maximize: "Maximize".into(),
            tip_titlebar_close: "Close window".into(),
            tip_input_path: "Input video file path".into(),
            tip_output_path: "Output video file path".into(),
            tip_value: "Value: target bitrate (kbps) for 2pass, quality (0-51) for CRF".into(),
            tip_width: "Output width; 0 keeps the original width".into(),
            tip_height: "Output height; 0 keeps the original height".into(),
            tip_method: "Encoding method: CRF constant quality, or 2pass for exact bitrate".into(),
            tip_resolution: "Output resolution".into(),
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
        titlebar_minimize: "Minimize".into(),
        titlebar_maximize: "Maximize".into(),
        titlebar_close: "Close window".into(),
    }
}

/// 构造指定语言的字符串集
pub fn for_lang(lang: Lang) -> Strings {
    match lang {
        Lang::Zh => zh(),
        Lang::En => en(),
    }
}

/// 版本号展示：只取 `+` 之前的部分（对应 `version.Split('+')[0]`）。
/// 用 `AssemblyInformationalVersion` 的等价替代：Cargo 包版本。
fn display_version() -> &'static str {
    let v = env!("CARGO_PKG_VERSION");
    v.split('+').next().unwrap_or(v)
}
