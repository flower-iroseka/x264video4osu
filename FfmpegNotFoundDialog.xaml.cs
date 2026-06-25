using System.Diagnostics;
using System.Windows;

namespace x264video4osu;

public partial class FfmpegNotFoundDialog : Window
{
    public FfmpegNotFoundDialog(string message)
    {
        InitializeComponent();
        MessageText.Text = message;
    }

    private void Download_Click(object sender, RoutedEventArgs e)
    {
        Process.Start(new ProcessStartInfo
        {
            FileName = Services.FfmpegConfig.DownloadUrl,
            UseShellExecute = true
        });
        DialogResult = true;
    }

    private void Exit_Click(object sender, RoutedEventArgs e)
    {
        DialogResult = false;
    }
}