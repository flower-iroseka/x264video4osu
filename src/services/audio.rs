//! 输出视频的音频处理方式（对应旧程序恒为「无音频」的行为扩展）。

use crate::services::format::OutputFormat;

/// 转码时使用的音频编码器
pub const AAC_BITRATE_KBPS: u32 = 192;

/// 音频模式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioMode {
    /// 无音频（`-an`），旧程序的行为
    None,
    /// 直接复制源音频流
    Copy,
    /// 转码成指定编码器
    Transcode,
}

impl AudioMode {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Copy,
            2 => Self::Transcode,
            _ => Self::None,
        }
    }
}

/// 转码用的音频编码器
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AudioCodec {
    Aac,
    Flac,
}

impl AudioCodec {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Flac,
            _ => Self::Aac,
        }
    }

    /// [CONFIG] 日志里的名字
    pub fn log_name(self) -> &'static str {
        match self {
            Self::Aac => "AAC 192 kbps",
            Self::Flac => "FLAC",
        }
    }
}

/// FLAC 只在 mp4 下可选。实测这个 ffmpeg 三种容器都能封装 FLAC（FLV 走
/// enhanced FLV 扩展、AVI 是非标准写法），不会报错，但那些组合播放器基本放不出来，
/// 所以非 mp4 时兜底降级成 AAC。
pub fn resolve_codec(codec_index: i32, format: OutputFormat) -> AudioCodec {
    let codec = AudioCodec::from_index(codec_index);
    if codec == AudioCodec::Flac && format != OutputFormat::Mp4 {
        return AudioCodec::Aac;
    }
    codec
}

/// 生成输出侧的音频参数。同时返回展示串，避免 token 和展示串两边漂移
/// （展示串就是把 token 用空格拼起来）。
///
/// 注意：一旦出现 `-map`，ffmpeg 就不再自动挑流，所以视频和音频两条 `-map`
/// 必须成对给出；音频那条带 `?`，源里没有音轨时不会报错。
pub fn output_args(mode: AudioMode, codec: AudioCodec) -> (Vec<String>, String) {
    let tokens: Vec<String> = match mode {
        AudioMode::None => vec!["-an".into()],
        AudioMode::Copy => vec![
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "0:a:0?".into(),
            "-c:a".into(),
            "copy".into(),
        ],
        AudioMode::Transcode => match codec {
            AudioCodec::Aac => vec![
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "0:a:0?".into(),
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                format!("{AAC_BITRATE_KBPS}k"),
            ],
            AudioCodec::Flac => vec![
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "0:a:0?".into(),
                "-c:a".into(),
                "flac".into(),
            ],
        },
    };
    let display = format!("{} ", tokens.join(" "));
    (tokens, display)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_index_fallback() {
        assert_eq!(AudioMode::from_index(0), AudioMode::None);
        assert_eq!(AudioMode::from_index(1), AudioMode::Copy);
        assert_eq!(AudioMode::from_index(2), AudioMode::Transcode);
        assert_eq!(AudioMode::from_index(9), AudioMode::None);
    }

    #[test]
    fn codec_index_fallback() {
        assert_eq!(AudioCodec::from_index(0), AudioCodec::Aac);
        assert_eq!(AudioCodec::from_index(1), AudioCodec::Flac);
        assert_eq!(AudioCodec::from_index(7), AudioCodec::Aac);
    }

    #[test]
    fn flac_only_in_mp4() {
        assert_eq!(resolve_codec(1, OutputFormat::Mp4), AudioCodec::Flac);
        assert_eq!(resolve_codec(1, OutputFormat::Flv), AudioCodec::Aac);
        assert_eq!(resolve_codec(1, OutputFormat::Avi), AudioCodec::Aac);
        assert_eq!(resolve_codec(0, OutputFormat::Flv), AudioCodec::Aac);
    }

    #[test]
    fn none_emits_an_only() {
        let (tokens, display) = output_args(AudioMode::None, AudioCodec::Aac);
        assert_eq!(tokens, vec!["-an"]);
        assert_eq!(display, "-an ");
    }

    #[test]
    fn copy_maps_and_copies() {
        let (tokens, display) = output_args(AudioMode::Copy, AudioCodec::Aac);
        assert!(tokens.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"));
        assert!(tokens.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a:0?"));
        assert!(!tokens.iter().any(|t| t == "-an"));
        assert_eq!(display, tokens.join(" ") + " ");
    }

    #[test]
    fn transcode_aac_uses_bitrate() {
        let (tokens, display) = output_args(AudioMode::Transcode, AudioCodec::Aac);
        assert!(tokens.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"));
        assert!(tokens.windows(2).any(|w| w[0] == "-b:a" && w[1] == "192k"));
        assert_eq!(display, tokens.join(" ") + " ");
    }

    #[test]
    fn transcode_flac_has_no_bitrate() {
        let (tokens, _) = output_args(AudioMode::Transcode, AudioCodec::Flac);
        assert!(tokens.windows(2).any(|w| w[0] == "-c:a" && w[1] == "flac"));
        assert!(!tokens.iter().any(|t| t == "-b:a"));
    }
}
