using System.IO;

namespace x264video4osu.Services;

/// <summary>
/// FFmpeg 配置类
/// </summary>
public static class FfmpegConfig
{
    private const string FfmpegDownloadUrl = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

    /// <summary>
    /// FFmpeg 可执行文件路径
    /// </summary>
    public static string FfmpegPath => GetToolPath("ffmpeg.exe");

    /// <summary>
    /// FFprobe 可执行文件路径
    /// </summary>
    public static string FfprobePath => GetToolPath("ffprobe.exe");

    /// <summary>
    /// FFmpeg 下载地址
    /// </summary>
    public static string DownloadUrl => FfmpegDownloadUrl;

    /// <summary>
    /// Tools 文件夹路径
    /// </summary>
    public static string ToolsFolder => Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "tools");

    /// <summary>
    /// 验证 FFmpeg 工具是否存在，不存在则抛出异常
    /// </summary>
    public static void ValidateTools()
    {
        string missingTools = string.Empty;

        if (!File.Exists(FfmpegPath))
            missingTools += "• ffmpeg.exe\n";

        if (!File.Exists(FfprobePath))
            missingTools += "• ffprobe.exe\n";

        if (!string.IsNullOrEmpty(missingTools))
        {
            throw new FileNotFoundException(missingTools, ToolsFolder);
        }
    }

    /// <summary>
    /// 获取工具路径，只从本地 tools 文件夹获取，不存在则抛出异常
    /// </summary>
    private static string GetToolPath(string exeName)
    {
        // 优先使用项目根目录下的 tools 文件夹
        string toolsPath = Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "tools", exeName);
        if (File.Exists(toolsPath))
            return toolsPath;

        // 尝试使用相对路径 tools 文件夹
        string relativeToolsPath = Path.Combine("tools", exeName);
        if (File.Exists(relativeToolsPath))
            return relativeToolsPath;

        // 不存在时返回路径（由调用者检查）
        return toolsPath;
    }
}