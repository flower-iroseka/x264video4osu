//! 本地时间格式化（替代旧程序 `DateTime.Now.ToString(...)`）。
//!
//! 通过 `GetLocalTime` 获取本地时间；时长格式化直接从秒计算，
//! 与 .NET `TimeSpan.ToString("hh\\:mm\\:ss")` / `("mm\\:ss")` 语义一致。

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

fn now_local() -> SYSTEMTIME {
    let mut st = SYSTEMTIME::default();
    // 文档约定：失败时返回 FALSE，但系统时间几乎总能取到，失败保持 0 值即可
    unsafe {
        GetLocalTime(&mut st);
    }
    st
}

/// `yyyy-MM-dd HH:mm:ss`（用于 `[START] Encoding started at ...` 等横幅）
pub fn local_timestamp() -> String {
    let st = now_local();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// `yyyyMMddHHmmss`（用于保存日志的文件名）
pub fn local_compact() -> String {
    let st = now_local();
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// `mm:ss`，分钟可以超过 60（对应 .NET `TimeSpan.ToString("mm\\:ss")`）。
pub fn format_mmss(total_secs: f64) -> String {
    let total = total_secs.max(0.0) as u64;
    let minutes = total / 60;
    let secs = total % 60;
    format!("{minutes:02}:{secs:02}")
}

/// `hh:mm:ss`（对应 `TimeSpan.ToString("hh\\:mm\\:ss")`，小时可超过 24）。
pub fn format_hms(total_secs: f64) -> String {
    let total = total_secs.max(0.0) as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    format!("{hours:02}:{minutes:02}:{secs:02}")
}

/// `hh:mm:ss.fff`（对应 `TimeSpan.ToString("hh\\:mm\\:ss\\.fff")`）。
pub fn format_hms_ms(total_secs: f64) -> String {
    let total_ms = (total_secs.max(0.0) * 1000.0) as u64;
    let ms = total_ms % 1000;
    let total = total_ms / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    format!("{hours:02}:{minutes:02}:{secs:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmss() {
        assert_eq!(format_mmss(0.0), "00:00");
        assert_eq!(format_mmss(65.0), "01:05");
        assert_eq!(format_mmss(5400.0), "90:00");
    }

    #[test]
    fn hms() {
        assert_eq!(format_hms(3661.0), "01:01:01");
        assert_eq!(format_hms(0.0), "00:00:00");
    }

    #[test]
    fn hms_ms() {
        assert_eq!(format_hms_ms(3661.0), "01:01:01.000");
        assert_eq!(format_hms_ms(1.25), "00:00:01.250");
    }

    #[test]
    fn compact_has_no_separators() {
        let s = local_compact();
        assert_eq!(s.len(), 14);
        assert!(s.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn timestamp_shape() {
        let s = local_timestamp();
        assert_eq!(s.len(), 19);
    }
}
