//! FFmpeg 参数构建（对应旧程序 `MainWindow.BuildFfmpegArgs`）。
//!
//! 为精确等价，产出两套表示：
//! - token 向量（`pass1`/`pass2`/`audio_extract`）：交给 `std::process::Command.args()`，
//!   Rust 会按 Windows CommandLineToArgvW 规则自行加引号，避免手工拼接的坑。
//! - 展示串（`display_*`）：逐字复刻旧程序拼出的命令行文本，用于 `[CMD] ffmpeg {args}` 日志。

use crate::error::{AppError, AppResult};
use crate::services::audio::{self, AudioMode, AAC_BITRATE_KBPS};
use crate::services::ffmpeg::probe_audio_codec;
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
    /// 0 = 无音频，1 = 复制音频流，2 = 转码音频
    pub audio_mode_index: i32,
    /// 0 = AAC，1 = FLAC（仅 mp4）
    pub audio_codec_index: i32,
    /// 裁剪起点（秒）；None = 从开头
    pub trim_start_secs: Option<f64>,
    /// 裁剪终点（秒）；None = 到结尾
    pub trim_end_secs: Option<f64>,
    /// 是否忽略视频轨的 edit list（只编一遍）。"战渣浪"那类文件的视频轨被 edit list
    /// 整段复制过，ffmpeg 默认照着编两遍，这个开关让它只编一遍。
    pub ignore_editlist: bool,
    /// 进度用的总时长（秒）；None = 由 ffprobe 现场探测。见 `EncodePlan::duration_override`。
    pub duration_override: Option<f64>,
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
    /// 2pass 日志的绝对路径基名（不含 ffmpeg 自己加的 `-0` 后缀）；CRF 模式下为 None。
    ///
    /// 清理临时文件要用这个而不是输出目录：passlogfile 传的是绝对路径，文件落在
    /// 应用的当前目录，不在输出文件旁边。
    pub log_file_path: Option<std::path::PathBuf>,
    /// 进度用的总时长（秒）；None = 由 ffprobe 现场探测。
    ///
    /// 视频轨被 edit list 复制、用户又选了编两遍时，ffprobe 报的只是容器声明的那一遍，
    /// 得用编辑区间算出来的实际长度覆盖，否则进度条会提前跑到 100%、ETA 全是 0。
    pub duration_override: Option<f64>,
    /// 裁剪区间（秒）。`-ss`/`-to` 都是输入侧选项，ffmpeg 的 `time=` 只覆盖这一段，
    /// 所以进度要按它算，不能按源视频全长算，否则进度条永远走不满。
    pub trim_start_secs: Option<f64>,
    pub trim_end_secs: Option<f64>,
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

    // ---------- 裁剪区间 ----------
    // -ss/-to 都放在 -i 前面（输入侧）：直接对应界面上的起点和终点，不用自己算时长。
    // 实测 10 秒源上 `-ss 2 -to 5` 得到 3 秒 / 90 帧，与 `-ss 2 -t 3` 等价。
    if let (Some(s), Some(e)) = (input.trim_start_secs, input.trim_end_secs) {
        if e <= s {
            return Err(AppError::InvalidArgument(
                "Trim end must be greater than start.".into(),
            ));
        }
    }
    let mut trim_tokens: Vec<String> = Vec::new();
    let mut trim_display = String::new();
    if let Some(s) = input.trim_start_secs {
        trim_tokens.push("-ss".into());
        trim_tokens.push(format!("{s:.3}"));
        trim_display.push_str(&format!("-ss {s:.3} "));
    }
    if let Some(e) = input.trim_end_secs {
        trim_tokens.push("-to".into());
        trim_tokens.push(format!("{e:.3}"));
        trim_display.push_str(&format!("-to {e:.3} "));
    }

    // -ignore_editlist 和 -ss/-to 一样是输入侧选项，得排在 -i 前面。
    // 音轨本身没有 edit list，所以只加给视频的编码命令，音轨提取不带。
    //
    // `-y` 是全局选项，放最前面：输出文件在执行到这一刻前被别人建出来的话，
    // ffmpeg 会停下来问 "Overwrite? [y/N]"。它的 stdin 是管道，我们不写东西进去，
    // 于是就卡在那儿不动了。加 `-y` 直接覆盖。视频输出路径在上面的去重逻辑里
    // 已经避开了已存在的文件，这里防的是「算完路径之后才冒出来的文件」。
    let mut pre_input_tokens: Vec<String> = vec!["-y".into()];
    let mut pre_input_display = String::from("-y ");
    if input.ignore_editlist {
        pre_input_tokens.extend(["-ignore_editlist".to_string(), "1".to_string()]);
        pre_input_display.push_str("-ignore_editlist 1 ");
    }
    pre_input_tokens.extend(trim_tokens);
    pre_input_display.push_str(&trim_display);

    // ---------- 音频（输出侧） ----------
    let audio_mode = AudioMode::from_index(input.audio_mode_index);
    let audio_codec = audio::resolve_codec(input.audio_codec_index, format);
    let (audio_tokens, audio_display) = audio::output_args(audio_mode, audio_codec);

    // ---------- 公共参数 ----------
    let escaped_input = escape_path_argument(&input_full.to_string_lossy());
    let scale = build_scale(width, height, input.scale_up);

    // 展示用（逐字复刻旧程序）：[-ss/-to] -i "<in>" -vf "<scale>" -r 24 -c:v libx264 ...
    let common_display = format!(
        "{pre_input_display}-i {escaped_input} -vf \"{scale}\" -r {fps} -c:v libx264 -preset veryslow \
         -profile:v high -level 5.2 -g {gop} -keyint_min {kimin} -x264-params \"{x264}\" \
         -pix_fmt yuv420p ",
        fps = FPS,
        gop = GOP_SIZE,
        kimin = KEYINT_MIN,
        x264 = X264_PARAMS,
    );

    // 执行用 token
    let mut common_tokens: Vec<String> = pre_input_tokens;
    common_tokens.extend([
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
    ]);

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
    let log_file_path = if is_two_pass {
        Some(std::path::PathBuf::from(&log_abs))
    } else {
        None
    };

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

        // pass1 输出到 NUL，muxer 需与所选格式对应。pass1 只做视频分析，
        // 音频固定 -an，不浪费时间去编码一遍。`-an` 是输出侧选项，必须排在
        // 输出文件名 NUL 前面，写在后面 ffmpeg 会当成多余参数直接忽略。
        let mut p1 = two_pass_common_tokens.clone();
        p1.extend([
            "-pass".into(),
            "1".into(),
            "-passlogfile".into(),
            log_abs.clone(),
            "-an".into(),
            "-f".into(),
            format.pass1_muxer().into(),
            "NUL".into(),
        ]);
        let d1 = format!(
            "{two_pass_common_display}-pass 1 -passlogfile {escaped_log} -an -f {} NUL",
            format.pass1_muxer()
        );

        // pass2
        let mut p2 = two_pass_common_tokens;
        p2.extend(["-pass".into(), "2".into(), "-passlogfile".into(), log_abs]);
        p2.extend(audio_tokens);
        p2.extend(container_tokens);
        p2.push(output_str);
        let d2 = format!(
            "{two_pass_common_display}-pass 2 -passlogfile {escaped_log} {audio_display}{container_display}{escaped_output}"
        );

        (p1, p2, d1, d2)
    } else {
        // ---------- CRF 模式 ----------
        let mut p2 = common_tokens;
        p2.extend(["-crf".into(), crf.to_string()]);
        p2.extend(audio_tokens);
        p2.extend(container_tokens);
        p2.push(output_str);
        let d2 = format!("{common_display}-crf {crf} {audio_display}{container_display}{escaped_output}");

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

        // m4a（ipod muxer）只装得下 aac / alac / ac3。别的编码直接复制会报
        // "Could not find tag for codec ... in stream" 然后写出一个 0 字节文件
        // —— 实测 opus、vorbis、flac、mp3、pcm 都不行。所以源码流不在名单里时
        // 转成 AAC，宁可重编码也不要失败。探不到编码（没有音轨、ffprobe 出错）
        // 时按可复制处理，保持旧行为。
        let source_codec = probe_audio_codec(&input_full.to_string_lossy());
        let copyable = match source_codec.as_deref() {
            Some(c) => matches!(c, "aac" | "alac" | "ac3"),
            None => true,
        };
        let codec_tokens: Vec<String> = if copyable {
            vec!["-c:a".into(), "copy".into()]
        } else {
            vec![
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{AAC_BITRATE_KBPS}k"),
            ]
        };
        let codec_display = codec_tokens.join(" ");

        // 音轨与视频用同一个裁剪区间，导出的音频和裁过的视频对得上。
        let mut tokens: Vec<String> = vec!["-y".into()];
        if let Some(s) = input.trim_start_secs {
            tokens.push("-ss".into());
            tokens.push(format!("{s:.3}"));
        }
        if let Some(e) = input.trim_end_secs {
            tokens.push("-to".into());
            tokens.push(format!("{e:.3}"));
        }
        tokens.push("-i".into());
        tokens.push(input_full.to_string_lossy().into_owned());
        tokens.push("-vn".into());
        tokens.extend(codec_tokens);
        tokens.push(audio_str);
        let display = format!(
            "-y {trim_display}-i {escaped_input} -vn {codec_display} {}",
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
        log_file_path,
        duration_override: input.duration_override,
        trim_start_secs: input.trim_start_secs,
        trim_end_secs: input.trim_end_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个真实存在的输入文件，绕过 build 的存在性校验。内容无所谓，只用到「存在」。
    /// 放在项目里的 tests-001/，固定文件名每次覆盖同一个，不会一次堆一个。
    fn temp_input() -> std::path::PathBuf {
        let p = crate::testutil::scratch_path("args_fake_input.mkv");
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
            audio_mode_index: 0,
            audio_codec_index: 0,
            trim_start_secs: None,
            trim_end_secs: None,
            ignore_editlist: false,
            duration_override: None,
        }
    }

    /// token 向量里某个值的下标（用于断言先后顺序）
    fn pos(tokens: &[String], needle: &str) -> Option<usize> {
        tokens.iter().position(|t| t == needle)
    }

    /// 取 `-key value` 里 key 后面跟着的值
    fn value_of<'a>(tokens: &'a [String], key: &str) -> Option<&'a String> {
        tokens.windows(2).find(|w| w[0] == key).map(|w| &w[1])
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
        i.input_text = crate::testutil::scratch_path("definitely_missing_input_98765.mkv")
            .to_string_lossy()
            .into_owned();
        assert!(matches!(build(i), Err(AppError::InputFileNotFound(_))));
    }

    #[test]
    fn crf_mode_uses_crf_value() {
        let i = base_input();
        let plan = build(i).unwrap();
        assert!(plan.pass1.is_empty());
        assert!(plan.log_file_path.is_none());
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-crf" && w[1] == "26"));
    }

    #[test]
    fn two_pass_uses_bitrate() {
        let mut i = base_input();
        i.mode_index = 1;
        i.value_text = "1200".into();
        let plan = build(i).unwrap();
        assert!(!plan.pass1.is_empty());
        let base = plan.log_file_path.as_ref().expect("2pass 应带 passlog 路径");
        let name = base.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("log_"));
        assert!(name.len() == 4 + 32);
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-b:v" && w[1] == "1200k"));
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-maxrate" && w[1] == "1200k"));
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-bufsize" && w[1] == "2400k"));
    }

    #[test]
    fn two_pass_has_passlogfile_absolute() {
        let mut i = base_input();
        i.mode_index = 1;
        let plan = build(i).unwrap();
        // passlogfile 必须是绝对路径（resolve 到应用 cwd）
        let lf_abs = plan
            .log_file_path
            .as_ref()
            .expect("2pass 应带 passlog 路径")
            .to_string_lossy()
            .into_owned();
        assert!(plan.pass1.iter().any(|t| t == &lf_abs));
        assert!(plan.pass2.iter().any(|t| t == &lf_abs));
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
        // 探不到源编码（测试用的是假文件）时按可复制处理
        assert!(audio.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"));
        assert!(plan.display_audio.starts_with("-y "));
    }

    /// ffmpeg 会停下来问 "Overwrite? [y/N]"，而它的 stdin 是管道，没人回答，
    /// 于是整个编码卡死。三个命令都得带 `-y`。
    #[test]
    fn all_commands_force_overwrite() {
        let mut i = base_input();
        i.mode_index = 1; // 2pass，三条命令都会生成
        i.extract_audio = true;
        let plan = build(i).unwrap();
        assert_eq!(plan.pass1.first().map(String::as_str), Some("-y"));
        assert_eq!(plan.pass2.first().map(String::as_str), Some("-y"));
        assert_eq!(
            plan.audio_extract.unwrap().first().map(String::as_str),
            Some("-y")
        );
    }

    // ---------- 音频模式 ----------

    #[test]
    fn audio_none_emits_an() {
        let plan = build(base_input()).unwrap();
        assert!(plan.pass2.iter().any(|t| t == "-an"));
        assert!(!plan.pass2.iter().any(|t| t == "-c:a"));
    }

    #[test]
    fn audio_copy_emits_stream_copy() {
        let mut i = base_input();
        i.audio_mode_index = 1;
        let plan = build(i).unwrap();
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"));
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a:0?"));
        assert!(!plan.pass2.iter().any(|t| t == "-an"));
    }

    #[test]
    fn audio_transcode_aac() {
        let mut i = base_input();
        i.audio_mode_index = 2;
        i.audio_codec_index = 0;
        let plan = build(i).unwrap();
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"));
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-b:a" && w[1] == "192k"));
    }

    #[test]
    fn audio_transcode_flac_in_mp4() {
        let mut i = base_input();
        i.audio_mode_index = 2;
        i.audio_codec_index = 1;
        i.format_index = 2; // mp4
        let plan = build(i).unwrap();
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-c:a" && w[1] == "flac"));
    }

    #[test]
    fn audio_transcode_flac_falls_back_outside_mp4() {
        let mut i = base_input();
        i.audio_mode_index = 2;
        i.audio_codec_index = 1;
        i.format_index = 0; // flv
        let plan = build(i).unwrap();
        assert!(!plan.pass2.iter().any(|t| t == "flac"));
        assert!(plan.pass2.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"));
    }

    #[test]
    fn pass1_strips_audio() {
        let mut i = base_input();
        i.mode_index = 1;
        i.audio_mode_index = 1;
        let plan = build(i).unwrap();
        assert!(plan.pass1.iter().any(|t| t == "-an"));
        assert!(!plan.pass1.iter().any(|t| t == "-c:a"));
    }

    // ---------- 裁剪 ----------

    #[test]
    fn trim_tokens_precede_input() {
        let mut i = base_input();
        i.trim_start_secs = Some(2.0);
        i.trim_end_secs = Some(5.0);
        let plan = build(i).unwrap();
        let ss = pos(&plan.pass2, "-ss").expect("-ss missing");
        let to = pos(&plan.pass2, "-to").expect("-to missing");
        let input = pos(&plan.pass2, "-i").expect("-i missing");
        assert!(ss < input, "-ss must come before -i");
        assert!(to < input, "-to must come before -i");
        assert_eq!(value_of(&plan.pass2, "-ss").unwrap(), "2.000");
        assert_eq!(value_of(&plan.pass2, "-to").unwrap(), "5.000");
    }

    #[test]
    fn trim_start_only() {
        let mut i = base_input();
        i.trim_start_secs = Some(1.5);
        let plan = build(i).unwrap();
        assert_eq!(value_of(&plan.pass2, "-ss").unwrap(), "1.500");
        assert!(!plan.pass2.iter().any(|t| t == "-to"));
    }

    #[test]
    fn trim_end_only() {
        let mut i = base_input();
        i.trim_end_secs = Some(8.25);
        let plan = build(i).unwrap();
        assert_eq!(value_of(&plan.pass2, "-to").unwrap(), "8.250");
        assert!(!plan.pass2.iter().any(|t| t == "-ss"));
    }

    #[test]
    fn trim_none_adds_nothing() {
        let plan = build(base_input()).unwrap();
        assert!(!plan.pass2.iter().any(|t| t == "-ss" || t == "-to"));
    }

    #[test]
    fn trim_in_both_passes() {
        let mut i = base_input();
        i.mode_index = 1;
        i.trim_start_secs = Some(3.0);
        i.trim_end_secs = Some(9.0);
        let plan = build(i).unwrap();
        for tokens in [&plan.pass1, &plan.pass2] {
            assert!(tokens.windows(2).any(|w| w[0] == "-ss" && w[1] == "3.000"));
            assert!(tokens.windows(2).any(|w| w[0] == "-to" && w[1] == "9.000"));
            let ss = pos(tokens, "-ss").unwrap();
            let input = pos(tokens, "-i").unwrap();
            assert!(ss < input);
        }
    }

    #[test]
    fn trim_end_before_start_rejected() {
        let mut i = base_input();
        i.trim_start_secs = Some(5.0);
        i.trim_end_secs = Some(2.0);
        assert!(matches!(build(i), Err(AppError::InvalidArgument(_))));
    }

    #[test]
    fn audio_extract_is_trimmed() {
        let mut i = base_input();
        i.extract_audio = true;
        i.trim_start_secs = Some(4.0);
        i.trim_end_secs = Some(6.0);
        let plan = build(i).unwrap();
        let audio = plan.audio_extract.unwrap();
        assert_eq!(value_of(&audio, "-ss").unwrap(), "4.000");
        assert_eq!(value_of(&audio, "-to").unwrap(), "6.000");
        let ss = pos(&audio, "-ss").unwrap();
        let input = pos(&audio, "-i").unwrap();
        assert!(ss < input);
    }

    #[test]
    fn display_matches_tokens_for_audio_and_trim() {
        let mut i = base_input();
        i.format_index = 2; // mp4
        i.audio_mode_index = 2;
        i.audio_codec_index = 1; // flac
        i.trim_start_secs = Some(10.0);
        i.trim_end_secs = Some(20.0);
        let plan = build(i).unwrap();
        let d = &plan.display_pass2;
        assert!(d.contains("-ss 10.000"), "display: {d}");
        assert!(d.contains("-to 20.000"), "display: {d}");
        assert!(d.contains("-c:a flac"), "display: {d}");
        assert!(plan.display_pass1.is_empty());
    }

    #[test]
    fn ignore_editlist_goes_before_input() {
        let mut i = base_input();
        i.ignore_editlist = true;
        let plan = build(i).unwrap();
        let t = pos(&plan.pass2, "-ignore_editlist").expect("-ignore_editlist missing");
        assert_eq!(value_of(&plan.pass2, "-ignore_editlist"), Some(&"1".to_string()));
        let input = pos(&plan.pass2, "-i").unwrap();
        assert!(t < input, "-ignore_editlist must come before -i");
        assert!(plan.display_pass2.contains("-ignore_editlist 1 -i"));
    }

    #[test]
    fn ignore_editlist_off_by_default() {
        let plan = build(base_input()).unwrap();
        assert!(!plan.pass2.iter().any(|t| t == "-ignore_editlist"));
        assert!(!plan.display_pass2.contains("-ignore_editlist"));
    }

    /// edit list 只影响视频轨，音轨提取不带这个开关
    #[test]
    fn ignore_editlist_not_in_audio_extract() {
        let mut i = base_input();
        i.ignore_editlist = true;
        i.extract_audio = true;
        let plan = build(i).unwrap();
        let audio = plan.audio_extract.expect("audio extract tokens");
        assert!(!audio.iter().any(|t| t == "-ignore_editlist"));
        assert!(!plan.display_audio.contains("-ignore_editlist"));
    }

    #[test]
    fn two_pass_both_carry_ignore_editlist() {
        let mut i = base_input();
        i.mode_index = 1;
        i.ignore_editlist = true;
        let plan = build(i).unwrap();
        assert!(plan.pass1.iter().any(|t| t == "-ignore_editlist"));
        assert!(plan.pass2.iter().any(|t| t == "-ignore_editlist"));
        assert!(plan.display_pass1.contains("-ignore_editlist 1"));
    }

    #[test]
    fn duration_override_passed_through() {
        let mut i = base_input();
        i.duration_override = Some(349.2);
        let plan = build(i).unwrap();
        assert_eq!(plan.duration_override, Some(349.2));
    }
}
