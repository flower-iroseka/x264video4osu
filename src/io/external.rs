//! 打开外部资源（URL / 文件夹），对应旧程序 `AppConfig.OpenUrl`
//! （`Process.Start` + `UseShellExecute = true`）。

use crate::error::{AppError, AppResult};

/// 在默认浏览器中打开 URL
pub fn open_url(url: &str) -> AppResult<()> {
    open::that(url).map_err(|e| AppError::OpenExternal(format!("Failed to open URL '{url}': {e}")))
}

/// 在文件管理器中打开文件夹
pub fn open_folder(path: &str) -> AppResult<()> {
    open::that(path)
        .map_err(|e| AppError::OpenExternal(format!("Failed to open folder '{path}': {e}")))
}
