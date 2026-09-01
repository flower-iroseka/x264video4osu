//! 输出封装格式（对应旧项目 Services/OutputFormat.cs）。

/// 输出封装格式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    /// FLV（osu! stable 兼容性最好）
    Flv,
    /// AVI
    Avi,
    /// MP4（osu! stable 存在随机崩溃 bug）
    Mp4,
}

impl OutputFormat {
    /// 从 UI 下拉框索引映射（0=flv, 1=avi, 2=mp4），非法值回落为 Flv，
    /// 与旧程序 `Enum.TryParse` 失败时返回 Flv 的行为一致。
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Avi,
            2 => Self::Mp4,
            _ => Self::Flv,
        }
    }

    /// 每种格式对应的文件扩展名
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Flv => ".flv",
            Self::Avi => ".avi",
            Self::Mp4 => ".mp4",
        }
    }

    /// 2pass 模式下 pass1 空输出（NUL）使用的容器 muxer 名称
    pub fn pass1_muxer(&self) -> &'static str {
        match self {
            Self::Flv => "flv",
            Self::Avi => "avi",
            Self::Mp4 => "mp4",
        }
    }

    /// 各格式特有的容器参数（例如 mp4 需要 faststart 以便渐进式播放）
    pub fn container_args(&self) -> &'static str {
        match self {
            Self::Mp4 => "-movflags +faststart ",
            _ => "",
        }
    }
}
