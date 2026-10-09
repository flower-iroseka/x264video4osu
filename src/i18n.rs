//! UI 字符串（对应旧程序 `Resources/Strings.zh-CN.xaml` 与 `Strings.en-US.xaml`，
//! 以及旧界面中硬编码的文案）。
//!
//! 旧程序设计成不随语言切换的那几处文案（`Open Output Folder` 按钮、进度行的
//! Pass / ETA / Elapsed、结果行的 Output size），3.2 起也一起翻了。日志页里的
//! `[CONFIG]` `[START]` 这类行仍是英文：那是技术日志，按惯例不翻。
//!
//! 校验提示和错误提示旧程序也是写死的英文，这里补上了中文（见 `title_*` / `msg_*`）。
//! 里面带 `{...}` 的是模板，由 app.rs 在运行时替换。

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
    /// 消息对话框标题：校验不通过 / 真正的失败
    pub title_validation: String,
    pub title_error: String,
    /// 输入校验提示
    pub msg_empty_input: String,
    pub msg_invalid_input: String,
    pub msg_input_not_found: String,
    pub msg_invalid_drop: String,
    /// 编码启动失败提示模板：`{detail}` 是底层错误原文
    pub msg_invalid_parameter: String,
    pub msg_encode_failed: String,
    /// 裁剪参数校验提示；`{text}` 是用户填的原文
    pub msg_no_fps: String,
    pub msg_invalid_frame: String,
    pub msg_invalid_time: String,
    pub msg_trim_order: String,
    /// 「视频里有两遍内容」对话框：标题、正文模板、两个按钮
    ///
    /// 正文里的 `{once}` / `{twice}` 由 app.rs 在运行时替换成长度文本。
    pub repeat_title: String,
    pub repeat_message: String,
    pub repeat_once_text: String,
    pub repeat_twice_text: String,
    /// 把秒数写成用户看得懂的长度：中文「2 分 55 秒」，英文「2:55」
    pub duration_text: fn(f64) -> String,
    /// 进度行的三格。`{pass}` `{total}` `{time}` 由 app.rs 替换
    pub progress_pass: String,
    pub progress_eta: String,
    pub progress_elapsed: String,
    /// 编码结束后的结果行，`{size}` 是 MB 数
    pub result_output_size: String,
    /// 选择输入文件的原生对话框里的过滤项名称
    pub dialog_filter_video: String,
}

fn zh_duration(secs: f64) -> String {
    let total = secs.round().max(0.0) as u64;
    format!("{} 分 {} 秒", total / 60, total % 60)
}

fn en_duration(secs: f64) -> String {
    let total = secs.round().max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
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
            button_open_folder: "打开输出文件夹".into(),
            // ---- 音频模式 ----
            label_audio_mode: "音频模式".into(),
            audio_none: "无音频".into(),
            audio_copy: "复制音频流".into(),
            audio_transcode: "转码音频".into(),
            label_audio_codec: "音频编码".into(),
            // ---- 视频裁剪 ----
            check_trim: "裁剪视频".into(),
            label_trim_unit: "裁剪单位".into(),
            trim_frames: "帧".into(),
            trim_time: "时间".into(),
            label_trim_range: "裁剪范围".into(),
            label_trim_start: "起点".into(),
            label_trim_end: "终点".into(),
            // ---- 悬停工具提示 ----
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
            tip_audio_mode: "输出视频中的音频处理方式".into(),
            tip_audio_codec: "转码音频时使用的编码器（FLAC 仅支持 mp4）".into(),
            tip_trim: "裁剪视频的起止范围".into(),
            // 提示只讲控件是什么，不讲界面会怎么反应（比如「改动后另一个框会跟着换算」）
            tip_trim_unit: "按帧号或按时间裁剪".into(),
            tip_trim_start: "裁剪起点".into(),
            tip_trim_end: "裁剪终点".into(),
            // ---- 视频预览 ----
            tip_preview_expand: "展开视频预览".into(),
            tip_preview_collapse: "收起视频预览".into(),
            button_play: "播放".into(),
            button_pause: "暂停".into(),
            button_set_start: "设为开头".into(),
            button_set_end: "设为结尾".into(),
            button_jump: "跳转".into(),
            tip_step_back: "后退一帧".into(),
            tip_step_forward: "前进一帧".into(),
            tip_goto_start: "跳到视频开头".into(),
            tip_goto_end: "跳到视频结尾".into(),
            tip_jump: "跳到输入框里的位置".into(),
            tip_frame_input: "帧号".into(),
            tip_time_input: "时间戳，格式 时:分:秒.毫秒".into(),
            tip_set_start: "把预览的当前位置设为裁剪起点".into(),
            tip_set_end: "把预览的当前位置设为裁剪终点".into(),
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
        title_validation: "校验失败".into(),
        title_error: "错误".into(),
        msg_empty_input: "视频输入路径不能为空。".into(),
        msg_invalid_input: "视频输入路径无效。".into(),
        msg_input_not_found: "找不到输入的视频文件。".into(),
        msg_invalid_drop: "拖入的文件路径无效。".into(),
        msg_invalid_parameter: "参数不合法：{detail}".into(),
        msg_encode_failed: "启动编码失败：{detail}".into(),
        msg_no_fps: "读不出源视频的帧率，请改用按时间裁剪。".into(),
        msg_invalid_frame: "帧号不合法：{text}".into(),
        msg_invalid_time: "时间不合法：{text}".into(),
        msg_trim_order: "裁剪终点必须大于起点。".into(),
        repeat_title: "检测到edit list".into(),
        repeat_message: "视频轨的edit list将同一段视频重复播放了两次。\n\
                         记录的真实时长： {once}\n\
                         实际播放的时长： {twice}\n\n\
                         忽略编辑列表，则只转换一遍正片。保留编辑列表，则输出重复两遍的视频。\
                         （若您未编辑过本视频，推荐选择忽略。）"
            .into(),
        repeat_once_text: "忽略编辑列表".into(),
        repeat_twice_text: "保留编辑列表".into(),
        duration_text: zh_duration,
        progress_pass: "第 {pass}/{total} 遍".into(),
        progress_eta: "剩余 {time}".into(),
        progress_elapsed: "已用 {time}".into(),
        result_output_size: "输出大小：{size} MB".into(),
        dialog_filter_video: "视频文件".into(),
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
            // ---- 音频模式 ----
            label_audio_mode: "Audio mode".into(),
            audio_none: "No audio".into(),
            audio_copy: "Copy audio stream".into(),
            audio_transcode: "Transcode audio".into(),
            label_audio_codec: "Audio codec".into(),
            // ---- 视频裁剪 ----
            check_trim: "Trim video".into(),
            label_trim_unit: "Trim unit".into(),
            trim_frames: "Frames".into(),
            trim_time: "Time".into(),
            label_trim_range: "Trim range".into(),
            label_trim_start: "Start".into(),
            label_trim_end: "End".into(),
            // ---- 悬停工具提示 ----
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
            tip_audio_mode: "How audio is handled in the output video".into(),
            tip_audio_codec: "Audio codec used when transcoding (FLAC is mp4-only)".into(),
            tip_trim: "Trim the start/end range of the video".into(),
            tip_trim_unit: "Trim by frame number or by time".into(),
            tip_trim_start: "Trim start".into(),
            tip_trim_end: "Trim end".into(),
            // ---- 视频预览 ----
            tip_preview_expand: "Expand the video preview".into(),
            tip_preview_collapse: "Collapse the video preview".into(),
            button_play: "Play".into(),
            button_pause: "Pause".into(),
            button_set_start: "Set as start".into(),
            button_set_end: "Set as end".into(),
            button_jump: "Go".into(),
            tip_step_back: "Step back one frame".into(),
            tip_step_forward: "Step forward one frame".into(),
            tip_goto_start: "Jump to the start of the video".into(),
            tip_goto_end: "Jump to the end of the video".into(),
            tip_jump: "Jump to the typed position".into(),
            tip_frame_input: "Frame number".into(),
            tip_time_input: "Timestamp, HH:MM:SS.mmm".into(),
            tip_set_start: "Use the current preview position as the trim start".into(),
            tip_set_end: "Use the current preview position as the trim end".into(),
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
        title_validation: "Validation Error".into(),
        title_error: "Error".into(),
        msg_empty_input: "Input file path cannot be empty.".into(),
        msg_invalid_input: "Invalid input file path.".into(),
        msg_input_not_found: "Input file not found.".into(),
        msg_invalid_drop: "Invalid file path dropped.".into(),
        msg_invalid_parameter: "Invalid parameter: {detail}".into(),
        msg_encode_failed: "Failed to start encoding: {detail}".into(),
        msg_no_fps: "Cannot determine the source frame rate; use time input instead.".into(),
        msg_invalid_frame: "Invalid frame number: {text}".into(),
        msg_invalid_time: "Invalid time: {text}".into(),
        msg_trim_order: "Trim end must be greater than start.".into(),
        repeat_title: "Edit list detected".into(),
        repeat_message: "The video track's edit list plays the same video twice.\n\
                         Recorded duration: {once}\n\
                         Playback duration: {twice}\n\n\
                         Ignoring the edit list converts only the actual footage; keeping it outputs \
                         a video that repeats. (If you did not edit this video yourself, ignoring is recommended.)"
            .into(),
        repeat_once_text: "Ignore edit list".into(),
        repeat_twice_text: "Keep edit list".into(),
        duration_text: en_duration,
        progress_pass: "Pass {pass}/{total}".into(),
        progress_eta: "ETA: {time}".into(),
        progress_elapsed: "Elapsed: {time}".into(),
        result_output_size: "Output size: {size} MB".into(),
        dialog_filter_video: "Video Files".into(),
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
