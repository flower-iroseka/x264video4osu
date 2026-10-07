//! 裁剪区间解析与换算。纯函数，便于单测。

/// 把 `时:分:秒.毫秒`（也接受 `分:秒` 和纯秒）解析成秒。空串/非法/负数返回 None。
pub fn parse_timecode(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return None;
    }

    let mut seconds = 0.0_f64;
    for (i, part) in parts.iter().enumerate() {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        let value: f64 = part.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        // 最后一段是秒，可能带小数；前面的段是整数分钟/小时
        if i + 1 < parts.len() {
            if value.fract() != 0.0 {
                return None;
            }
            if value > 59.0 {
                return None;
            }
        }
        seconds = seconds * 60.0 + value;
    }

    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(seconds)
}

/// 源帧号换算成秒（第 0 帧 = 源视频第一帧）。fps <= 0 时返回 0，调用方需先校验。
pub fn frame_to_secs(frame: i64, fps: f64) -> f64 {
    if fps <= 0.0 {
        return 0.0;
    }
    frame as f64 / fps
}

/// 把秒格式化成 `时:分:秒.毫秒`，用于 [CONFIG] 日志。
pub fn format_timecode(secs: f64) -> String {
    let total_ms = (secs.max(0.0) * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let s = total_secs % 60;
    let m = (total_secs / 60) % 60;
    let h = total_secs / 3600;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_full() {
        assert_eq!(parse_timecode("01:02:03.500"), Some(3723.5));
    }

    #[test]
    fn timecode_minutes_seconds() {
        assert_eq!(parse_timecode("02:03"), Some(123.0));
    }

    #[test]
    fn timecode_bare_seconds() {
        assert_eq!(parse_timecode("5.25"), Some(5.25));
        assert_eq!(parse_timecode(" 12 "), Some(12.0));
    }

    #[test]
    fn timecode_rejects_bad() {
        assert_eq!(parse_timecode(""), None);
        assert_eq!(parse_timecode("   "), None);
        assert_eq!(parse_timecode("abc"), None);
        assert_eq!(parse_timecode("-1"), None);
        assert_eq!(parse_timecode("1:2:3:4"), None);
        assert_eq!(parse_timecode("1:99:00"), None);
        assert_eq!(parse_timecode("1.5:00"), None);
    }

    #[test]
    fn frame_conversion() {
        assert!((frame_to_secs(240, 24.0) - 10.0).abs() < 1e-9);
        assert!((frame_to_secs(24, 23.976) - 1.001001).abs() < 1e-5);
        assert_eq!(frame_to_secs(10, 0.0), 0.0);
    }

    #[test]
    fn timecode_format() {
        assert_eq!(format_timecode(3723.5), "01:02:03.500");
        assert_eq!(format_timecode(0.0), "00:00:00.000");
        assert_eq!(format_timecode(59.9996), "00:01:00.000");
    }
}
