//! 路径工具：与旧程序 `MainWindow.ValidatePath` / `EscapePathArgument`
//! 以及 .NET `Path.GetFullPath` 行为等价。
//!
//! 注意：Rust 的 `std::path::absolute` 不会因非法字符报错，所以必须手动
//! 检查 Windows 非法路径字符；长度按 UTF-16 单元计数以匹配 .NET 的
//! `String.Length`。

use std::path::PathBuf;

/// Windows `Path.GetInvalidPathChars()` 返回的字符集合：
/// 控制字符 0-31 以及 `" < > |`。
fn is_invalid_char(c: char) -> bool {
    (c as u32) <= 31 || c == '"' || c == '<' || c == '>' || c == '|'
}

/// 等价于 .NET `Path.GetFullPath`：把相对路径解析为基于当前工作目录的
/// 绝对路径，并做 `.` / `..` 归一化。失败返回 None（对应 GetFullPath 抛异常）。
pub fn full_path(path: &str) -> Option<PathBuf> {
    std::path::absolute(path).ok()
}

/// 验证路径是否安全，防止路径遍历 / 注入（对应 `ValidatePath`）。
/// 不检查文件是否存在。
pub fn validate_path(path: &str) -> bool {
    let full = match full_path(path) {
        Some(p) => p,
        None => return false,
    };
    let s = full.to_string_lossy();

    // 检查路径是否包含非法字符（控制字符 + `" < > |`）
    if s.chars().any(is_invalid_char) {
        return false;
    }

    // 检查路径长度是否合理（> 260 个 UTF-16 单元拒绝）
    let utf16_len = s.encode_utf16().count();
    if utf16_len > 260 {
        return false;
    }

    true
}

/// 安全转义路径参数以防止命令注入（对应 `EscapePathArgument`）。
/// 与旧行为一致：空路径 / 无法解析 / 校验失败时返回空串。
/// 返回值带双引号包裹，用于展示用命令行文本。
pub fn escape_path_argument(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }

    let full = match full_path(path) {
        Some(p) => p,
        None => return String::new(),
    };
    let s = full.to_string_lossy();

    if !validate_path(&s) {
        return String::new();
    }

    // Windows 下将内部双引号替换为转义序列（实际校验后不会出现，仅保持等价）
    let escaped = s.replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_wraps_in_quotes() {
        let out = escape_path_argument("some\\relative\\path");
        assert!(out.starts_with('"') && out.ends_with('"'));
        assert!(!out.contains('\n'));
    }

    #[test]
    fn escape_empty_returns_empty() {
        assert_eq!(escape_path_argument(""), "");
    }

    #[test]
    fn invalid_control_char_rejected() {
        assert!(!validate_path("bad\npath"));
        assert!(!validate_path("bad\tpath"));
    }

    #[test]
    fn invalid_special_chars_rejected() {
        assert!(!validate_path("bad|path"));
        assert!(!validate_path("bad<path"));
        assert!(!validate_path("bad>path"));
        assert!(!validate_path("bad\"path"));
    }

    #[test]
    fn valid_path_accepted() {
        assert!(validate_path("C:\\Users\\test\\file.mkv"));
        assert!(validate_path("plain_relative_file.mkv"));
    }

    #[test]
    fn length_limit_utf16() {
        // 生成 261 个字符的路径
        let long = "C:\\".to_string() + &"a".repeat(258);
        assert!(!validate_path(&long));
    }
}
