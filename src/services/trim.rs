//! 裁剪区间解析与换算。纯函数，便于单测。
//!
//! 帧号 ↔ 时间的**数学**在 [`crate::services::probe::Framing`] 上（恒定帧率线性算、
//! 可变帧率查表），这里只管输入框里的文本怎么解析、怎么格式化。

use crate::services::probe::Framing;

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

/// 把裁剪输入框里的值换到另一个单位。`to_frames` 为真表示时间 → 帧号，
/// 为假表示帧号 → 时间。
///
/// 换不了就**原样返回**：空串、格式不对、帧率拿不到（按帧那一侧要用它换算）都算换不了。
/// 用户切一下单位，填错的那半截输入不该被抹掉。
pub fn convert_unit_text(text: &str, to_frames: bool, timing: &Framing) -> String {
    if !timing.is_valid() {
        return text.to_string();
    }
    let converted = if to_frames {
        // 这里不用再补容差：时间戳只写到毫秒，30fps 下第 1 帧是 33.33ms，存成
        // 00:00:00.033 之后乘回去只剩 0.99 帧，本来会 floor 到第 0 帧。这半毫秒的
        // 舍入误差现在由 secs_to_frame 自己补（见 probe.rs 的 floor_eps），
        // 在这里再加一次就会把边界下方本该属于上一帧的值也推上去。
        parse_timecode(text).map(|secs| timing.secs_to_frame(secs).to_string())
    } else {
        // 负数帧号按帧编码时本来就会被拒，这里也不接受。
        // 越界的帧号先夹进范围再换算。按帧编码时 `resolve_trim` 夹的就是它，时间框
        // 应该显示真正会生效的那个位置。不夹就会把这个纯数字原样留在时间框里，
        // 而它在那边是**秒**（`parse_timecode` 把裸数字当秒），VFR 文件上切一次
        // 单位，800 帧就变成了 800 秒。
        text.trim()
            .parse::<i64>()
            .ok()
            .filter(|f| *f >= 0)
            .map(|frame| format_timecode(timing.frame_to_secs(timing.clamp_frame(frame))))
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
    use crate::services::probe::{FrameTable, Rational};

    fn cfr(num: u32, den: u32) -> Framing {
        Framing::Cfr(Rational::new(num, den))
    }

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
    fn timecode_format() {
        assert_eq!(format_timecode(3723.5), "01:02:03.500");
        assert_eq!(format_timecode(0.0), "00:00:00.000");
        assert_eq!(format_timecode(59.9996), "00:01:00.000");
    }

    #[test]
    fn unit_conversion_both_ways() {
        // 帧 → 时间
        assert_eq!(convert_unit_text("240", false, &cfr(24, 1)), "00:00:10.000");
        assert_eq!(convert_unit_text(" 0 ", false, &cfr(30, 1)), "00:00:00.000");
        // 时间 → 帧
        assert_eq!(convert_unit_text("00:00:10.000", true, &cfr(24, 1)), "240");
        assert_eq!(convert_unit_text("10", true, &cfr(24, 1)), "240");
        // 来回换一轮回到原值
        for f in [0_i64, 1, 90, 240, 1000] {
            let time = convert_unit_text(&f.to_string(), false, &cfr(30, 1));
            assert_eq!(convert_unit_text(&time, true, &cfr(30, 1)), f.to_string());
        }
    }

    #[test]
    fn unit_conversion_keeps_text_it_cannot_convert() {
        // 帧率拿不到，两边都没法换
        let unknown = Framing::Unknown;
        assert_eq!(convert_unit_text("240", false, &unknown), "240");
        assert_eq!(
            convert_unit_text("00:00:10.000", true, &unknown),
            "00:00:10.000"
        );

        // 空串和写错的值原样留着，不能抹成 0
        assert_eq!(convert_unit_text("", true, &cfr(30, 1)), "");
        assert_eq!(convert_unit_text("", false, &cfr(30, 1)), "");
        assert_eq!(convert_unit_text("abc", true, &cfr(30, 1)), "abc");
        assert_eq!(convert_unit_text("abc", false, &cfr(30, 1)), "abc");
        assert_eq!(convert_unit_text("1:99:00", true, &cfr(30, 1)), "1:99:00");

        // 负数帧号按帧编码时会被拒，换算时也不认
        assert_eq!(convert_unit_text("-5", false, &cfr(30, 1)), "-5");
    }

    #[test]
    fn unit_conversion_goes_through_the_table_for_vfr() {
        // 三种帧间隔的 VFR：帧 5 在第 7168 刻度（= 0.4667s），不是线性换算的 0.1667s
        let vfr = Framing::Vfr {
            fps: Rational::new(30, 1),
            table: FrameTable {
                timebase: Rational::new(1, 15360),
                pts: vec![0, 512, 1024, 1536, 2048, 7168],
            },
        };
        assert_eq!(convert_unit_text("5", false, &vfr), "00:00:00.467");
        assert_eq!(convert_unit_text("00:00:00.467", true, &vfr), "5");

        // 越界的帧号先夹到最后一帧（第 5 帧）再换算，时间框显示的就是它的时间。
        // 以前是原样留着，那串数字在时间框里会被当成"秒"读。
        assert_eq!(convert_unit_text("6", false, &vfr), "00:00:00.467");
        assert_eq!(convert_unit_text("9999", false, &vfr), "00:00:00.467");
    }
}
