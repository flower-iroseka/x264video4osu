using System.Diagnostics;

namespace x264video4osu.Services;

/// <summary>
/// 应用程序配置
/// </summary>
public static class AppConfig
{
    private const string GitHubRepositoryUrlValue = "https://github.com/flower-iroseka/x264video4osu";
    private const string GitHubIssuesUrlValue = "https://github.com/flower-iroseka/x264video4osu/issues";
    private const string GitHubWikiUrlValue = "https://github.com/flower-iroseka/x264video4osu/wiki";

    /// <summary>
    /// GitHub 仓库地址
    /// </summary>
    public static string GitHubRepositoryUrl => GitHubRepositoryUrlValue;

    /// <summary>
    /// GitHub Issues 页面
    /// </summary>
    public static string GitHubIssuesUrl => GitHubIssuesUrlValue;

    /// <summary>
    /// 项目 Wiki
    /// </summary>
    public static string GitHubWikiUrl => GitHubWikiUrlValue;

    /// <summary>
    /// 在浏览器中打开指定 URL
    /// </summary>
    public static void OpenUrl(string url)
    {
        Process.Start(new ProcessStartInfo
        {
            FileName = url,
            UseShellExecute = true
        });
    }
}