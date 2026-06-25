using System.Reflection;
using System.Windows;
using System.Windows.Documents;
using x264video4osu.Services;

namespace x264video4osu;

public partial class AboutDialog : Window
{
    public AboutDialog()
    {
        InitializeComponent();

        // 获取版本号
        var version = Assembly.GetExecutingAssembly()
            .GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion
            ?? Assembly.GetExecutingAssembly().GetName().Version?.ToString()
            ?? "1.0.0";

        // 显示版本号（格式：1.0.0 或 1.0.0+commit_hash）
        var displayVersion = version.Split('+')[0];
        VersionText.Text = Application.Current.TryFindResource("AboutVersion") is string versionTemplate
            ? string.Format(versionTemplate, displayVersion)
            : $"Version {displayVersion}";

        // 判断当前语言
        var isZh = Application.Current.TryFindResource("MainTab") as string == "视频";

        // 设置描述文本（带换行）
        DescriptionText.Inlines.Clear();
        if (isZh)
        {
            DescriptionText.Inlines.Add(new Run("osu! 视频压制工具"));
            DescriptionText.Inlines.Add(new LineBreak());
            DescriptionText.Inlines.Add(new LineBreak());
            DescriptionText.Inlines.Add(new Run("使用 x264 编码器为 osu! 制作兼容的背景视频。"));
        }
        else
        {
            DescriptionText.Inlines.Add(new Run("osu! video compression tool"));
            DescriptionText.Inlines.Add(new LineBreak());
            DescriptionText.Inlines.Add(new LineBreak());
            DescriptionText.Inlines.Add(new Run("Uses x264 encoder to create osu! compatible background video."));
        }
    }

    private void Repository_Click(object sender, RoutedEventArgs e)
    {
        AppConfig.OpenUrl(AppConfig.GitHubRepositoryUrl);
    }

    private void Wiki_Click(object sender, RoutedEventArgs e)
    {
        AppConfig.OpenUrl(AppConfig.GitHubWikiUrl);
    }

    private void Close_Click(object sender, RoutedEventArgs e)
    {
        Close();
    }
}