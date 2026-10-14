//! FFmpeg 工具定位与校验（对应旧程序 Services/FfmpegConfig.cs）。

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

/// 优先 `<exe>/tools/<name>`，其次相对 cwd 的 `tools/<name>`，
/// 都不存在时返回前者（由调用者检查）。对应 `GetToolPath`。
///
/// 不限定扩展名、也不限定必须是 exe，方便以后往 tools/ 里放别的东西。
pub(crate) fn tool_path(name: &str) -> PathBuf {
    let primary = exe_dir().join("tools").join(name);
    if primary.exists() {
        return primary;
    }

    let relative = PathBuf::from("tools").join(name);
    if relative.exists() {
        return relative;
    }

    primary
}

pub fn ffmpeg_path() -> PathBuf {
    tool_path("ffmpeg.exe")
}

pub fn download_url() -> &'static str {
    FFMPEG_DOWNLOAD_URL
}

/// 校验工具是否存在，缺失时返回带缺失清单的错误（对应 `ValidateTools` 抛
/// `FileNotFoundException`，消息包含 `• ffmpeg.exe\n`）。
pub fn validate_tools() -> AppResult<()> {
    if ffmpeg_path().exists() {
        return Ok(());
    }
    Err(AppError::ToolsMissing("• ffmpeg.exe\n".into()))
}
