//! FFmpeg / ffprobe 工具定位与校验（对应旧程序 Services/FfmpegConfig.cs）。

use crate::error::{AppError, AppResult};
use std::path::PathBuf;

const FFMPEG_DOWNLOAD_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

/// 应用可执行文件所在目录
fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_default()
}

/// 与应用同级的 tools 文件夹（对应 `AppDomain.CurrentDomain.BaseDirectory + "tools"`）
pub fn tools_folder() -> PathBuf {
    exe_dir().join("tools")
}

/// 优先 `<exe>/tools/<exeName>`，其次相对 cwd 的 `tools/<exeName>`，
/// 都不存在时返回前者（由调用者检查）。对应 `GetToolPath`。
fn get_tool_path(exe_name: &str) -> PathBuf {
    let primary = exe_dir().join("tools").join(exe_name);
    if primary.exists() {
        return primary;
    }

    let relative = PathBuf::from("tools").join(exe_name);
    if relative.exists() {
        return relative;
    }

    primary
}

pub fn ffmpeg_path() -> PathBuf {
    get_tool_path("ffmpeg.exe")
}

pub fn ffprobe_path() -> PathBuf {
    get_tool_path("ffprobe.exe")
}

pub fn download_url() -> &'static str {
    FFMPEG_DOWNLOAD_URL
}

/// 校验工具是否存在，缺失时返回带缺失清单的错误（对应 `ValidateTools` 抛
/// `FileNotFoundException`，消息包含 `• ffmpeg.exe\n`）。
pub fn validate_tools() -> AppResult<()> {
    let mut missing = String::new();
    if !ffmpeg_path().exists() {
        missing.push_str("• ffmpeg.exe\n");
    }
    if !ffprobe_path().exists() {
        missing.push_str("• ffprobe.exe\n");
    }
    if !missing.is_empty() {
        return Err(AppError::ToolsMissing(missing));
    }
    Ok(())
}
