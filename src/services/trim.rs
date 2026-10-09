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

/// 秒换算成源帧号，是 [`frame_to_secs`] 的反方向。预览位置回填到帧号输入框要用它。
///
/// 取的是**当前正在显示的那一帧**：预览停在 3.4567s、fps = 30 时返回 103
/// （帧 103 占 [3.4333, 3.4667)），而不是「离得最近的那一帧」。
/// 「把当前预览位置设为起点」要的是屏幕上那一帧，四舍五入会让它往后偏一帧。
///
/// 末尾那个 1e-6 是防浮点误差：`3.4333… × 30` 有可能算出 102.99999999999999，
/// 直接 floor 会得到 102。
///
/// fps <= 0、秒数为负或非有限时返回 0，和 `frame_to_secs` 一样把校验留给调用方。
pub fn secs_to_frame(secs: f64, fps: f64) -> i64 {
    if fps <= 0.0 || !secs.is_finite() || secs < 0.0 {
        return 0;
    }
    ((secs * fps) + 1e-6).floor() as i64
}

/// 把裁剪输入框里的值换到另一个单位。`to_frames` 为真表示时间 → 帧号，
/// 为假表示帧号 → 时间。
///
/// 换不了就**原样返回**：空串、格式不对、帧率拿不到（按帧那一侧要用它换算）都算换不了。
/// 用户切一下单位，填错的那半截输入不该被抹掉。
pub fn convert_unit_text(text: &str, to_frames: bool, fps: f64) -> String {
    if fps <= 0.0 {
        return text.to_string();
    }
    let converted = if to_frames {
        // 时间戳只写到毫秒，帧号换算成时间再换回来会差那么一点点，
        // 30fps 下第 1 帧是 33.33ms，存成 00:00:00.033 之后乘回去只有 0.99 帧，
        // floor 会掉到第 0 帧。补上半个毫秒正好抵消这次取整，来回切换不会掉帧。
        // （半个毫秒乘以再高的帧率也不到一帧，不会冲到下一帧去。）
        parse_timecode(text).map(|secs| secs_to_frame(secs + 0.0005, fps).to_string())
    } else {
        // 负数帧号按帧编码时本来就会被拒，这里也不接受
        text.trim()
            .parse::<i64>()
            .ok()
            .filter(|f| *f >= 0)
            .map(|frame| format_timecode(frame_to_secs(frame, fps)))
    };
    converted.unwrap_or_else(|| text.to_string())
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
    fn secs_to_frame_takes_the_displayed_frame() {
        // 帧 103 占 [3.4333, 3.4667)，3.4567 落在这区间里
        assert_eq!(secs_to_frame(3.4567, 30.0), 103);
        // 帧边界上不能因为浮点误差退回上一帧
        assert_eq!(secs_to_frame(103.0 / 30.0, 30.0), 103);
        assert_eq!(secs_to_frame(240.0 / 24.0, 24.0), 240);
        // 往回换算一轮应当回到原值
        for f in [0_i64, 1, 29, 240, 1000] {
            assert_eq!(secs_to_frame(frame_to_secs(f, 30.0), 30.0), f);
        }
    }

    #[test]
    fn secs_to_frame_guards_bad_input() {
        assert_eq!(secs_to_frame(1.0, 0.0), 0);
        assert_eq!(secs_to_frame(-3.0, 30.0), 0);
        assert_eq!(secs_to_frame(f64::NAN, 30.0), 0);
        assert_eq!(secs_to_frame(f64::INFINITY, 30.0), 0);
    }

    #[test]
    fn timecode_format() {
        assert_eq!(format_timecode(3723.5), "01:02:03.500");
        assert_eq!(format_timecode(0.0), "00:00:00.000");
        assert_eq!(format_timecode(59.9996), "00:01:00.000");
    }

    #[test]
    fn unit_conversion_both_ways() {
        // 帧 → 时间
        assert_eq!(convert_unit_text("240", false, 24.0), "00:00:10.000");
        assert_eq!(convert_unit_text(" 0 ", false, 30.0), "00:00:00.000");
        // 时间 → 帧
        assert_eq!(convert_unit_text("00:00:10.000", true, 24.0), "240");
        assert_eq!(convert_unit_text("10", true, 24.0), "240");
        // 来回换一轮回到原值
        for f in [0_i64, 1, 90, 240, 1000] {
            let time = convert_unit_text(&f.to_string(), false, 30.0);
            assert_eq!(convert_unit_text(&time, true, 30.0), f.to_string());
        }
    }

    #[test]
    fn unit_conversion_keeps_text_it_cannot_convert() {
        // 帧率拿不到，两边都没法换
        assert_eq!(convert_unit_text("240", false, 0.0), "240");
        assert_eq!(convert_unit_text("00:00:10.000", true, 0.0), "00:00:10.000");

        // 空串和写错的值原样留着，不能抹成 0
        assert_eq!(convert_unit_text("", true, 30.0), "");
        assert_eq!(convert_unit_text("", false, 30.0), "");
        assert_eq!(convert_unit_text("abc", true, 30.0), "abc");
        assert_eq!(convert_unit_text("abc", false, 30.0), "abc");
        assert_eq!(convert_unit_text("1:99:00", true, 30.0), "1:99:00");

        // 负数帧号按帧编码时会被拒，换算时也不认
        assert_eq!(convert_unit_text("-5", false, 30.0), "-5");
    }
}
