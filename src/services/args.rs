//! FFmpeg 参数构建（对应旧程序 `MainWindow.BuildFfmpegArgs`）。
//!
//! 为精确等价，产出两套表示：
//! - token 向量（`pass1`/`pass2`/`audio_extract`）：交给 `std::process::Command.args()`，
//!   Rust 会按 Windows CommandLineToArgvW 规则自行加引号，避免手工拼接的坑。
//! - 展示串（`display_*`）：逐字复刻旧程序拼出的命令行文本，用于 `[CMD] ffmpeg {args}` 日志。

use crate::error::{AppError, AppResult};
use crate::services::format::OutputFormat;
use crate::services::pathutil::{escape_path_argument, full_path, validate_path};
use crate::services::scale::build_scale;

/// 用户从 UI 传入的原始输入
pub struct EncodeInput {
    pub input_text: String,
    pub output_text: String,
    pub format_index: i32,
    /// 0 = CRF，1 = 2pass
    pub mode_index: i32,
    pub value_text: String,
    pub width_text: String,
    pub height_text: String,
    pub scale_up: bool,
    pub extract_audio: bool,
}

/// 构建完成的编码方案
pub struct EncodePlan {
    /// 执行用 token（2pass 的 pass1；CRF 模式下为空向量）
    pub pass1: Vec<String>,
    /// 执行用 token（实际编码）
    pub pass2: Vec<String>,
    /// 可选的音轨提取 token
    pub audio_extract: Option<Vec<String>>,
    /// 展示用命令行文本
    pub display_pass1: String,
    pub display_pass2: String,
    pub display_audio: String,
    /// 最终输出路径
    pub output: std::path::PathBuf,
    /// 2pass 日志文件名（`log_<guid>`）；CRF 模式下为空串
    pub log_file_name: String,
}

const FPS: i32 = 24;
const GOP_SIZE: i32 = 300;
const KEYINT_MIN: i32 = 240;

/// x264 高级参数（照搬旧程序）
const X264_PARAMS: &str =
    "scenecut=0:ref=16:bframes=16:b-adapt=2:direct=auto:me=umh:subme=11:trellis=2:\
     rc-lookahead=60:aq-mode=3:aq-strength=1.0:psy-rd=1.0,0.15:deblock=-1,-1:\
     weightp=2:cabac=1:merange=32";

/// 把文本解析为整数，失败按 0 处理（对应 `int.TryParse` 的 out 初值）
fn parse_int(s: &str) -> i32 {
    s.trim().parse().unwrap_or(0)
}

/// 等价于 .NET `Path.ChangeExtension(path, ".ext")`：替换当前扩展名；没有则追加。
fn change_extension(path: &std::path::Path, new_ext: &str) -> std::path::PathBuf {
    let trimmed = new_ext.trim_start_matches('.');
    if trimmed.is_empty() {
        return path.to_path_buf();
    }
    path.with_extension(trimmed)
}

/// 等价于 .NET `Path.Combine(dir, filename)`
fn combine(dir: &std::path::Path, filename: &str) -> std::path::PathBuf {
    dir.join(filename)
}

/// 构建 FFmpeg 编码方案。任何一步失败都会把错误带回上层（与旧程序抛异常一致）。
pub fn build(input: EncodeInput) -> AppResult<EncodePlan> {
    let value_box = parse_int(&input.value_text);
    let width = parse_int(&input.width_text);
    let height = parse_int(&input.height_text);

    let is_two_pass = input.mode_index == 1;
    let format = OutputFormat::from_index(input.format_index);

    // 根据模式设置值
    let mut bitrate = 800i32;
    let mut crf = 23i32;
    if is_two_pass {
        // 2pass 模式：valueBox 是比特率
        if value_box > 0 {
            bitrate = value_box;
        }
    } else {
        // CRF 模式：valueBox 是 CRF 值
        if (0..=51).contains(&value_box) {
            crf = value_box;
        }
    }

    let input_text = input.input_text.trim();
    let output_text = input.output_text.trim();

    // 验证并规范化输入路径
    if input_text.is_empty() {
        return Err(AppError::InvalidArgument("Input path cannot be empty.".into()));
    }
    if !validate_path(input_text) || !std::path::Path::new(input_text).exists() {
        return Err(AppError::InputFileNotFound("Input file not found or invalid.".into()));
    }
    let input_full = full_path(input_text)
        .ok_or_else(|| AppError::InvalidArgument("Invalid input path.".into()))?;

    // 输出路径
    let output_raw = if output_text.is_empty() {
        // 无输出时：<输入目录>/<输入名>_output<扩展名>
        let dir = input_full
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let name = input_full
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        combine(&dir, &format!("{name}_output{}", format.extension()))
    } else {
        // 验证输出路径目录是否存在且可写
        if let Some(output_dir) = std::path::Path::new(output_text).parent() {
            if !output_dir.as_os_str().is_empty() && !output_dir.exists() {
                std::fs::create_dir_all(output_dir).map_err(AppError::Io)?;
            }
        }
        full_path(output_text)
            .ok_or_else(|| AppError::InvalidArgument("Invalid output path.".into()))?
    };
    // 确保输出扩展名与所选格式一致
    let mut output = change_extension(&output_raw, format.extension());

    // 输出文件已存在时自动追加 (1)、(2)… 后缀，直到名字不冲突（新增强，旧程序会直接覆盖）。
    // 基准名固定取自首次算出的路径，避免对 (1) 再次加后缀形成 (1)(2)。
    let output_dir = output
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    let output_stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let output_ext = output
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext_part = if output_ext.is_empty() {
        String::new()
    } else {
        format!(".{output_ext}")
    };
    let mut dup_index = 1u32;
    while output.exists() {
        output = output_dir.join(format!("{output_stem}({dup_index}){ext_part}"));
        dup_index += 1;
    }

    // 日志文件名（仅 2pass 需要）- 使用 GUID 避免冲突
    let log_file_name = if is_two_pass {
        format!("log_{}", uuid::Uuid::new_v4().simple())
    } else {
        String::new()
    };

    let maxrate = format!("{bitrate}k");
    let bufsize = format!("{}k", bitrate * 2);

    // ---------- 公共参数 ----------
    let escaped_input = escape_path_argument(&input_full.to_string_lossy());
    let scale = build_scale(width, height, input.scale_up);

    // 展示用（逐字复刻旧程序）：-i "<in>" -vf "<scale>" -r 24 -c:v libx264 ... -an
    let common_display = format!(
        "-i {escaped_input} -vf \"{scale}\" -r {fps} -c:v libx264 -preset veryslow \
         -profile:v high -level 5.2 -g {gop} -keyint_min {kimin} -x264-params \"{x264}\" \
         -pix_fmt yuv420p -an ",
        fps = FPS,
        gop = GOP_SIZE,
        kimin = KEYINT_MIN,
        x264 = X264_PARAMS,
    );

    // 执行用 token
    let common_tokens: Vec<String> = vec![
        "-i".into(),
        input_full.to_string_lossy().into_owned(),
        "-vf".into(),
        scale,
        "-r".into(),
        FPS.to_string(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryslow".into(),
        "-profile:v".into(),
        "high".into(),
        "-level".into(),
        "5.2".into(),
        "-g".into(),
        GOP_SIZE.to_string(),
        "-keyint_min".into(),
        KEYINT_MIN.to_string(),
        "-x264-params".into(),
        X264_PARAMS.into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-an".into(),
    ];

    // 各格式特有的容器参数
    let container_display = format.container_args();
    let container_tokens: Vec<String> = match format {
        OutputFormat::Mp4 => vec!["-movflags".into(), "+faststart".into()],
        _ => Vec::new(),
    };

    let output_str = output.to_string_lossy().into_owned();
    let escaped_output = escape_path_argument(&output_str);

    // 2pass 的 passlogfile：旧程序对 `log_<guid>` 做 GetFullPath（相对应用 cwd），
    // 因此日志文件落在应用的当前工作目录。此处保持同样的解析行为，把绝对路径
    // 作为 token 传给 ffmpeg，ffmpeg 会写到该绝对路径。
    let log_abs = full_path(&log_file_name)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| log_file_name.clone());
    let escaped_log = escape_path_argument(&log_file_name);

    let (pass1, pass2, display_pass1, display_pass2) = if is_two_pass {
        // ---------- 2pass 模式 ----------
        let two_pass_common_display = format!(
            "{common_display}-b:v {bitrate}k -maxrate {maxrate} -bufsize {bufsize} "
        );
        let mut two_pass_common_tokens = common_tokens;
        two_pass_common_tokens.extend([
            "-b:v".into(),
            format!("{bitrate}k"),
            "-maxrate".into(),
            maxrate.clone(),
            "-bufsize".into(),
            bufsize.clone(),
        ]);

        // pass1 输出到 NUL，muxer 需与所选格式对应
        let mut p1 = two_pass_common_tokens.clone();
        p1.extend([
            "-pass".into(),
            "1".into(),
            "-passlogfile".into(),
            log_abs.clone(),
            "-f".into(),
            format.pass1_muxer().into(),
            "NUL".into(),
        ]);
        let d1 = format!(
            "{two_pass_common_display}-pass 1 -passlogfile {escaped_log} -f {} NUL",
            format.pass1_muxer()
        );

        // pass2
        let mut p2 = two_pass_common_tokens;
        p2.extend(["-pass".into(), "2".into(), "-passlogfile".into(), log_abs]);
        p2.extend(container_tokens);
        p2.push(output_str);
        let d2 = format!(
            "{two_pass_common_display}-pass 2 -passlogfile {escaped_log} {container_display}{escaped_output}"
        );

        (p1, p2, d1, d2)
    } else {
        // ---------- CRF 模式 ----------
        let mut p2 = common_tokens;
        p2.extend(["-crf".into(), crf.to_string()]);
        p2.extend(container_tokens);
        p2.push(output_str);
        let d2 = format!("{common_display}-crf {crf} {container_display}{escaped_output}");

        (Vec::new(), p2, String::new(), d2)
    };

    // ---------- 音轨提取参数 ----------
    let (audio_extract, display_audio) = if input.extract_audio {
        let dir = output
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let stem = output
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let audio_output = combine(&dir, &format!("{stem}_audio.m4a"));
        let audio_str = audio_output.to_string_lossy().into_owned();

        let tokens = vec![
            "-i".into(),
            input_full.to_string_lossy().into_owned(),
            "-vn".into(),
            "-c:a".into(),
            "copy".into(),
            audio_str,
        ];
        let display = format!(
            "-i {escaped_input} -vn -c:a copy {}",
            escape_path_argument(&audio_output.to_string_lossy())
        );
        (Some(tokens), display)
    } else {
        (None, String::new())
    };

    Ok(EncodePlan {
        pass1,
        pass2,
        audio_extract,
        display_pass1,
        display_pass2,
        display_audio,
        output,
        log_file_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个真实存在的临时输入文件，绕过 build 的存在性校验
    fn temp_input() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("x264video4osu_test_{}.mkv", std::process::id()));
        std::fs::write(&p, b"fake").unwrap();
        p
    }

    fn base_input() -> EncodeInput {
        EncodeInput {
            input_text: temp_input().to_string_lossy().into_owned(),
            output_text: String::new(),
            format_index: 0,
            mode_index: 0,
            value_text: "26".into(),
            width_text: "0".into(),
            height_text: "720".into(),
            scale_up: false,
            extract_audio: false,
        }
    }

    #[test]
    fn empty_input_rejected() {
        let mut i = base_input();
        i.input_text = "   ".into();
        assert!(build(i).is_err());
    }

    #[test]
    fn missing_file_rejected() {
        let mut i = base_input();
        i.input_text = std::env::temp_dir()
            .join("definitely_missing_input_98765.mkv")
            .to_string_lossy()
            .into_owned();
        assert!(matches!(build(i), Err(AppError::InputFileNotFound(_))));
    }

    #[test]
    fn crf_mode_uses_crf_value() {
        let i = base_input();
        let plan = build(i).unwrap();
        assert!(plan.pass1.is_empty());
        assert!(plan.log_file_name.is_empty());
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-crf" && w[1] == "26"));
    }

    #[test]
    fn two_pass_uses_bitrate() {
        let mut i = base_input();
        i.mode_index = 1;
        i.value_text = "1200".into();
        let plan = build(i).unwrap();
        assert!(!plan.pass1.is_empty());
        assert!(!plan.log_file_name.is_empty());
        assert!(plan.log_file_name.starts_with("log_"));
        assert!(plan.log_file_name.len() == 4 + 32);
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-b:v" && w[1] == "1200k"));
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-maxrate" && w[1] == "1200k"));
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-bufsize" && w[1] == "2400k"));
    }

    #[test]
    fn two_pass_has_passlogfile_absolute() {
        let mut i = base_input();
        i.mode_index = 1;
        let plan = build(i).unwrap();
        let lf = &plan.log_file_name;
        assert!(plan.pass1.iter().any(|t| t.contains(lf)));
        assert!(plan.pass2.iter().any(|t| t.contains(lf)));
        // passlogfile 必须是绝对路径（resolve 到应用 cwd）
        let lf_abs = full_path(lf).unwrap().to_string_lossy().into_owned();
        assert!(plan.pass1.iter().any(|t| t == &lf_abs));
        assert!(plan.display_pass1.contains(&format!("-passlogfile \"{lf_abs}\"")));
    }

    #[test]
    fn crf_invalid_value_falls_back_to_default() {
        let mut i = base_input();
        i.mode_index = 0;
        i.value_text = "99".into(); // 超出 0..=51
        let plan = build(i).unwrap();
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-crf" && w[1] == "23"));
    }

    #[test]
    fn crf_valid_value_used() {
        let mut i = base_input();
        i.mode_index = 0;
        i.value_text = "20".into();
        let plan = build(i).unwrap();
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-crf" && w[1] == "20"));
    }

    #[test]
    fn two_pass_zero_value_falls_back() {
        let mut i = base_input();
        i.mode_index = 1;
        i.value_text = "0".into(); // 0 不大于 0 → 用默认 800
        let plan = build(i).unwrap();
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-b:v" && w[1] == "800k"));
    }

    #[test]
    fn mp4_gets_faststart() {
        let mut i = base_input();
        i.format_index = 2;
        let plan = build(i).unwrap();
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-movflags" && w[1] == "+faststart"));
    }

    #[test]
    fn flv_no_faststart() {
        let i = base_input();
        let plan = build(i).unwrap();
        assert!(!plan.pass2.iter().any(|t| t == "-movflags"));
    }

    #[test]
    fn audio_extract_built() {
        let mut i = base_input();
        i.extract_audio = true;
        let plan = build(i).unwrap();
        let audio = plan.audio_extract.unwrap();
        assert!(audio.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"));
        assert!(plan.display_audio.starts_with("-i "));
    }
}
