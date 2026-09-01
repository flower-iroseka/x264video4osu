//! 缩放参数生成（对应旧项目 Services/ScaleHelper.cs）。

/// 生成 ffmpeg 的 scale 滤镜参数。
/// 与旧 `ScaleHelper.Build(w, h, upscale)` 逐字等价：
/// - 0×0      → `scale=iw:ih`（保持原始分辨率）
/// - w=0      → 等比缩放高度；不放大时用 `if(gt(ih,{h}),{h},ih)` 只缩不放
/// - h=0      → 等比缩放宽度；同上
/// - 两者都有 → 直接指定宽高；不放大时两个维度都只缩不放
pub fn build_scale(w: i32, h: i32, upscale: bool) -> String {
    if w == 0 && h == 0 {
        return "scale=iw:ih".to_string();
    }

    if w == 0 {
        return if upscale {
            format!("scale=-1:{h}")
        } else {
            format!("scale=-1:'if(gt(ih,{h}),{h},ih)'")
        };
    }

    if h == 0 {
        return if upscale {
            format!("scale={w}:-1")
        } else {
            format!("scale='if(gt(iw,{w}),{w},iw)':-1")
        };
    }

    if upscale {
        format!("scale={w}:{h}")
    } else {
        format!("scale='if(gt(iw,{w}),{w},iw)':'if(gt(ih,{h}),{h},ih)'")
    }
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
        assert_eq!(build_scale(0, 720, true), "scale=-1:720");
        assert_eq!(build_scale(0, 720, false), "scale=-1:'if(gt(ih,720),720,ih)'");
    }

    #[test]
    fn height_only() {
        assert_eq!(build_scale(1280, 0, true), "scale=1280:-1");
        assert_eq!(build_scale(1280, 0, false), "scale='if(gt(iw,1280),1280,iw)':-1");
    }

    #[test]
    fn both() {
        assert_eq!(build_scale(1280, 720, true), "scale=1280:720");
        assert_eq!(
            build_scale(1280, 720, false),
            "scale='if(gt(iw,1280),1280,iw)':'if(gt(ih,720),720,ih)'"
        );
    }
}
