//! 缩放参数生成（对应旧项目 Services/ScaleHelper.cs）。

/// 生成 ffmpeg 的 scale 滤镜参数。
/// 结构照搬旧 `ScaleHelper.Build(w, h, upscale)`：
/// - 0×0      → `scale=iw:ih`（保持原始分辨率）
/// - w=0      → 等比缩放高度；不放大时用 `if(gt(ih,{h}),{h},ih)` 只缩不放
/// - h=0      → 等比缩放宽度；同上
/// - 两者都有 → 直接指定宽高；不放大时两个维度都只缩不放
///
/// 与旧程序的差别：宽高一律取偶数。输出固定 `-pix_fmt yuv420p`，libx264 碰到奇数
/// 宽或高会直接拒绝（`width not divisible by 2`）。旧程序用 `-1` 让 ffmpeg 自己算
/// 等比那一维，算出来的可能正好是奇数 —— 比如 1366x768 的源只填高度 720，宽度是
/// 1281，于是整个编码失败。这里改用 `-2`（自动算并取偶），用户填的数值和
/// `if(gt(...))` 保留原始尺寸的那条分支也套一层取偶。
pub fn build_scale(w: i32, h: i32, upscale: bool) -> String {
    let w = even(w);
    let h = even(h);

    if w == 0 && h == 0 {
        return "scale=iw:ih".to_string();
    }

    if w == 0 {
        return if upscale {
            format!("scale=-2:{h}")
        } else {
            format!("scale=-2:'{}'", even_expr(&format!("if(gt(ih,{h}),{h},ih)")))
        };
    }

    if h == 0 {
        return if upscale {
            format!("scale={w}:-2")
        } else {
            format!("scale='{}':-2", even_expr(&format!("if(gt(iw,{w}),{w},iw)")))
        };
    }

    if upscale {
        format!("scale={w}:{h}")
    } else {
        format!(
            "scale='{}':'{}'",
            even_expr(&format!("if(gt(iw,{w}),{w},iw)")),
            even_expr(&format!("if(gt(ih,{h}),{h},ih)"))
        )
    }
}

/// 取到不大于自身的偶数，并且不小于 2。0 和负数当作「没填」返回 0，
/// 免得算出 `scale=1:...` 这种还是会被 libx264 拒掉的尺寸。
fn even(v: i32) -> i32 {
    if v <= 0 {
        0
    } else {
        (v & !1).max(2)
    }
}

/// 把一段 ffmpeg 表达式的结果也取到偶数。源视频本身的宽高可能是奇数，
/// 只缩不放的分支会原样保留它。
fn even_expr(expr: &str) -> String {
    format!("trunc(({expr})/2)*2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_zero() {
        assert_eq!(build_scale(0, 0, false), "scale=iw:ih");
        assert_eq!(build_scale(0, 0, true), "scale=iw:ih");
    }

    #[test]
    fn width_only() {
        assert_eq!(build_scale(0, 720, true), "scale=-2:720");
        assert_eq!(
            build_scale(0, 720, false),
            "scale=-2:'trunc((if(gt(ih,720),720,ih))/2)*2'"
        );
    }

    #[test]
    fn height_only() {
        assert_eq!(build_scale(1280, 0, true), "scale=1280:-2");
        assert_eq!(
            build_scale(1280, 0, false),
            "scale='trunc((if(gt(iw,1280),1280,iw))/2)*2':-2"
        );
    }

    #[test]
    fn both() {
        assert_eq!(build_scale(1280, 720, true), "scale=1280:720");
        assert_eq!(
            build_scale(1280, 720, false),
            "scale='trunc((if(gt(iw,1280),1280,iw))/2)*2':'trunc((if(gt(ih,720),720,ih))/2)*2'"
        );
    }

    #[test]
    fn odd_sizes_are_rounded_down_to_even() {
        // 1281 会被 libx264 拒掉，写成 1280
        assert_eq!(build_scale(1281, 721, true), "scale=1280:720");
        // 1 取偶后是 0 的话会变成「保持原始分辨率」，所以下限是 2
        assert_eq!(build_scale(1, 1, true), "scale=2:2");
        // 负数/0 当作没填
        assert_eq!(build_scale(-4, 720, true), "scale=-2:720");
    }
}
