//! 统一错误类型。所有可恢复的失败都用 `Result<T, AppError>` 表达，
//! 避免到处 `unwrap()` 或 `Box<dyn Error>`。

use std::fmt;

#[derive(Debug)]
pub enum AppError {
    /// 底层 IO / 进程启动失败
    Io(std::io::Error),
    /// 参数不合法（对应旧程序的 ArgumentException）
    InvalidArgument(String),
    /// 输入文件缺失或非法（对应 FileNotFoundException）
    InputFileNotFound(String),
    /// FFmpeg 工具缺失，附带缺失项说明（• ffmpeg.exe 等）
    ToolsMissing(String),
    /// 打开外部资源失败（URL / 文件夹）
    OpenExternal(String),
    /// 编码子进程失败
    EncodeFailed(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Io(e) => write!(f, "{e}"),
            AppError::InvalidArgument(m) => write!(f, "{m}"),
            AppError::InputFileNotFound(m) => write!(f, "{m}"),
            AppError::ToolsMissing(m) => write!(f, "{m}"),
            AppError::OpenExternal(m) => write!(f, "{m}"),
            AppError::EncodeFailed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AppError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e)
    }
}

/// 便捷别名
pub type AppResult<T> = Result<T, AppError>;
