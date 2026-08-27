namespace x264video4osu.Services;

/// <summary>
/// 输出封装格式
/// </summary>
public enum OutputFormat
{
    /// <summary>FLV（osu! stable 兼容性最好）</summary>
    Flv,

    /// <summary>AVI</summary>
    Avi,

    /// <summary>MP4（osu! stable 存在随机崩溃 bug）</summary>
    Mp4
}

/// <summary>
/// 输出格式相关的 FFmpeg 参数辅助类
/// </summary>
public static class OutputFormatHelper
{
    /// <summary>
    /// 每种格式对应的文件扩展名
    /// </summary>
    public static string Extension(this OutputFormat format) => format switch
    {
        OutputFormat.Flv => ".flv",
        OutputFormat.Avi => ".avi",
        OutputFormat.Mp4 => ".mp4",
        _ => ".mp4"
    };

    /// <summary>
    /// 2pass 模式下 pass1 空输出（NUL）使用的容器 muxer 名称
    /// </summary>
    public static string Pass1Muxer(this OutputFormat format) => format switch
    {
        OutputFormat.Flv => "flv",
        OutputFormat.Avi => "avi",
        OutputFormat.Mp4 => "mp4",
        _ => "mp4"
    };

    /// <summary>
    /// 各格式特有的容器参数（例如 mp4 需要 faststart 以便渐进式播放）
    /// </summary>
    public static string ContainerArgs(this OutputFormat format) => format switch
    {
        OutputFormat.Mp4 => "-movflags +faststart ",
        _ => ""
    };
}
