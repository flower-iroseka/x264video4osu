//! 打开外部资源（URL / 文件夹），对应旧程序 `AppConfig.OpenUrl`
//! （`Process.Start` + `UseShellExecute = true`）。

use crate::error::{AppError, AppResult};
use std::path::Path;
use std::process::Command;

/// 在默认浏览器中打开 URL
pub fn open_url(url: &str) -> AppResult<()> {
    open::that(url).map_err(|e| AppError::OpenExternal(format!("Failed to open URL '{url}': {e}")))
}

/// 打开文件所在的文件夹，并选中这个文件。
///
/// 不能用 `open::that`：那是把路径本身交给 shell 打开，只会打开文件夹、不选中文件。
/// 选中要靠 `explorer /select,<路径>`。斜杠和路径拼成一个参数交给 Rust 加引号，
/// 出来是 `explorer "/select,C:\a b\c.mp4"` —— 逗号后面紧跟路径，explorer 认这个写法；
/// 拆成 `"/select," "C:\a b\c.mp4"` 两个参数时逗号后面隔了个空格，经常就不选中了。
pub fn open_folder_selecting(path: &Path) -> AppResult<()> {
    Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            AppError::OpenExternal(format!(
                "Failed to open folder for '{}': {e}",
                path.display()
            ))
        })
}
