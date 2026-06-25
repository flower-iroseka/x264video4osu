using Microsoft.Win32;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Windows;
using x264video4osu.Services;

namespace x264video4osu;

public partial class MainWindow : Window
{
    private readonly FfmpegService _ffmpeg = new();
    private readonly StringBuilder _log = new();

    private string? _lastOutputPath;
    private string? _currentLogFileName;

    public MainWindow()
    {
        InitializeComponent();
        LanguageBox.SelectedIndex = 0;

        // 检查 FFmpeg 工具
        CheckFfmpegTools();

        StartButton.IsEnabled = true;
        StopButton.IsEnabled = false;

        _ffmpeg.LogReceived += AppendLog;
        _ffmpeg.LogMessage += AppendLog;
        _ffmpeg.ProgressChanged += OnProgress;
        _ffmpeg.Completed += OnCompleted;

        // 注册窗口关闭事件以进行资源清理
        Closing += MainWindow_Closing;
    }

    // =============================================================
    // FFmpeg 工具检查
    // =============================================================

    private static void CheckFfmpegTools()
    {
        try
        {
            FfmpegConfig.ValidateTools();
        }
        catch (FileNotFoundException)
        {
            ShowFfmpegNotFoundDialog();
            Environment.Exit(1);
        }
    }

    private static void ShowFfmpegNotFoundDialog()
    {
        string messageTemplate = Application.Current.TryFindResource("FfmpegNotFoundMessage") as string ??
            "FFmpeg tools not found!\n\nPlease place the following files in the tools folder:\n• ffmpeg.exe\n• ffprobe.exe\n\ntools folder location: {0}";

        System.Diagnostics.Debug.WriteLine($"FfmpegNotFoundMessage template: [{messageTemplate}]");
        System.Diagnostics.Debug.WriteLine($"Length: {messageTemplate.Length}");

        string message = string.Format(messageTemplate, FfmpegConfig.ToolsFolder);

        var dialog = new FfmpegNotFoundDialog(message);
        dialog.ShowDialog();
    }


    // =============================================================
    // UI eventsoutput
    // =============================================================

    private async void Start_Click(object sender, RoutedEventArgs e)
    {
        string inputPath = InputPathBox.Text.Trim();

        // 验证输入路径
        if (string.IsNullOrWhiteSpace(inputPath))
        {
            MessageBox.Show("Input file path cannot be empty.", "Validation Error",
                MessageBoxButton.OK, MessageBoxImage.Warning);
            return;
        }

        // 验证路径安全性和文件存在性
        if (!ValidatePath(inputPath))
        {
            MessageBox.Show("Invalid input file path.", "Validation Error",
                MessageBoxButton.OK, MessageBoxImage.Warning);
            return;
        }

        if (!File.Exists(Path.GetFullPath(inputPath)))
        {
            MessageBox.Show("Input file not found.", "Error",
                MessageBoxButton.OK, MessageBoxImage.Warning);
            return;
        }

        StartButton.IsEnabled = false;
        StopButton.IsEnabled = true;

        try
        {
            var (pass1, pass2, output, logFileName, audioExtractArgs) = BuildFfmpegArgs();

            _lastOutputPath = output;
            _currentLogFileName = logFileName;

            ProgressPanel.Visibility = Visibility.Visible;
            ResultPanel.Visibility = Visibility.Collapsed;
            ProgressBar.Value = 0;

            // 记录编码参数
            LogEncodingParameters();

            await _ffmpeg.StartAsync(pass1, pass2, output, logFileName, audioExtractArgs);
        }
        catch (ArgumentException ex)
        {
            MessageBox.Show($"Invalid parameter: {ex.Message}", "Validation Error",
                MessageBoxButton.OK, MessageBoxImage.Warning);
            StartButton.IsEnabled = true;
            StopButton.IsEnabled = false;
        }
        catch (Exception ex)
        {
            MessageBox.Show($"Failed to start encoding: {ex.Message}", "Error",
                MessageBoxButton.OK, MessageBoxImage.Error);
            StartButton.IsEnabled = true;
            StopButton.IsEnabled = false;
        }
    }

    private void AppendLog(string message)
    {
        Dispatcher.Invoke(() =>
        {
            _log.AppendLine(message);
            LogBox.AppendText(message + Environment.NewLine);
            if (AutoScrollCheck.IsChecked == true)
                LogBox.ScrollToEnd();
        });
    }

    private void LogEncodingParameters()
    {
        // 解析用户输入
        _ = int.TryParse(BitrateBox.Text, out int valueBox);
        _ = int.TryParse(WidthBox.Text, out int width);
        _ = int.TryParse(HeightBox.Text, out int height);

        // 确定编码模式
        bool isTwoPass = TwoPassRadio.IsChecked == true;
        EncodeMode mode = isTwoPass ? EncodeMode.TwoPass : EncodeMode.CRF;

        // 根据模式设置值
        int bitrate = 800;
        int crfValue = 23;

        if (isTwoPass)
        {
            if (valueBox > 0) bitrate = valueBox;
        }
        else
        {
            if (valueBox >= 0 && valueBox <= 51) crfValue = valueBox;
        }

        AppendLog($"[CONFIG] Encoding mode: {(mode == EncodeMode.TwoPass ? "2-Pass (VBR)" : "CRF (Constant Rate Factor)")}");

        if (mode == EncodeMode.TwoPass)
        {
            AppendLog($"[CONFIG] Target bitrate: {bitrate} kbps");
        }
        else
        {
            AppendLog($"[CONFIG] CRF value: {crfValue}");
        }

        AppendLog($"[CONFIG] Resolution: {(width > 0 ? width.ToString() : "original")} x {(height > 0 ? height.ToString() : "original")}");
        AppendLog($"[CONFIG] FPS: 24");
        AppendLog($"[CONFIG] Preset: veryslow");
        AppendLog($"[CONFIG] Profile: high, Level: 5.2");

        if (ExtractAudioCheck.IsChecked == true)
        {
            AppendLog($"[CONFIG] Audio extraction: enabled");
        }
        else
        {
            AppendLog($"[CONFIG] Audio extraction: disabled");
        }
    }

    private void Stop_Click(object sender, RoutedEventArgs e)
    {
        StartButton.IsEnabled = true;
        StopButton.IsEnabled = false;

        _ffmpeg.Stop();
        if (_lastOutputPath != null && _currentLogFileName != null)
        {
            _ffmpeg.CleanupTempFiles(_lastOutputPath, _currentLogFileName);
        }
    }


    private void BrowseInput_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new OpenFileDialog();
        if (dlg.ShowDialog() == true)
            InputPathBox.Text = dlg.FileName;
    }

    private void BrowseOutput_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new SaveFileDialog
        {
            Filter = "MP4 Video|*.mp4"
        };
        if (dlg.ShowDialog() == true)
            OutputPathBox.Text = dlg.FileName;
    }

    private void OpenFolder_Click(object sender, RoutedEventArgs e)
    {
        if (_lastOutputPath == null)
            return;

        Process.Start(new ProcessStartInfo
        {
            FileName = Path.GetDirectoryName(_lastOutputPath)!,
            UseShellExecute = true
        });
    }

    // =============================================================
    // FFmpeg callbacks
    // =============================================================

    private void OnProgress(ProgressInfo info)
    {
        Dispatcher.Invoke(() =>
        {
            PassText.Text = $"Pass {info.PassIndex}/{info.PassCount}";
            PercentText.Text = $"{info.Percent:0}%";
            EtaText.Text = $"ETA: {info.EstimatedRemaining:mm\\:ss}";
            ElapsedText.Text = $"Elapsed: {info.Elapsed:mm\\:ss}";
            ProgressBar.Value = info.Percent;
        });
    }

    private void OnCompleted(string outputPath)
    {
        Dispatcher.Invoke(() =>
        {
            StartButton.IsEnabled = true;
            StopButton.IsEnabled = false;

            ProgressPanel.Visibility = Visibility.Collapsed;
            ResultPanel.Visibility = Visibility.Visible;

            if (File.Exists(outputPath))
            {
                var sizeMb = new FileInfo(outputPath).Length / 1024.0 / 1024.0;
                OutputSizeText.Text = $"Output size: {sizeMb:F2} MB";
            }
        });
    }

// =============================================================
// Argument builder
// =============================================================

private (string pass1, string pass2, string output, string logFileName, string? audioExtractArgs) BuildFfmpegArgs()
{
    // 读取用户输入
    _ = int.TryParse(BitrateBox.Text, out int valueBox);
    _ = int.TryParse(WidthBox.Text, out int width);
    _ = int.TryParse(HeightBox.Text, out int height);

    // 确定编码模式
    bool isTwoPass = TwoPassRadio.IsChecked == true;
    EncodeMode mode = isTwoPass ? EncodeMode.TwoPass : EncodeMode.CRF;

    // 根据模式设置值
    int bitrate = 800;
    int crfValue = 23;

    if (isTwoPass)
    {
        // 2pass模式：valueBox是比特率
        if (valueBox > 0) bitrate = valueBox;
    }
    else
    {
        // CRF模式：valueBox是CRF值
        if (valueBox >= 0 && valueBox <= 51) crfValue = valueBox;
    }

    // 输入输出路径处理
    string input = InputPathBox.Text.Trim();
    string output = OutputPathBox.Text.Trim();

    // 验证并规范化输入路径
    if (string.IsNullOrWhiteSpace(input))
        throw new ArgumentException("Input path cannot be empty.");

    if (!ValidatePath(input) || !File.Exists(input))
        throw new FileNotFoundException("Input file not found or invalid.", input);

    // 获取完整路径
    input = Path.GetFullPath(input);

    if (string.IsNullOrWhiteSpace(output))
    {
        var dir = Path.GetDirectoryName(input) ?? "";
        var name = Path.GetFileNameWithoutExtension(input);
        output = Path.Combine(dir, $"{name}_output.mp4");
    }
    else
    {
        // 验证输出路径目录是否存在且可写
        string? outputDir = Path.GetDirectoryName(output);
        if (!string.IsNullOrEmpty(outputDir) && !Directory.Exists(outputDir))
            Directory.CreateDirectory(outputDir);

        output = Path.GetFullPath(output);
    }

    // 日志文件名（仅在2pass时需要）- 使用 GUID 避免冲突
    string logFileName = $"log_{Guid.NewGuid():N}";

    // =====================================
    // 核心参数 - 参考你常用的命令行风格
    // =====================================
    const int fps = 24;
    const int gopSize = 300;       // 最大关键帧间隔
    const int keyintMin = 240;     // 最小关键帧间隔
    string maxrate = $"{bitrate}k";
    string bufsize = $"{bitrate * 2}k";   // 常见做法：bufsize = maxrate × 2

    // x264 高级参数（基本照搬你提供的）
    string x264Params =
        "scenecut=0:" +
        "ref=16:" +
        "bframes=16:" +
        "b-adapt=2:" +
        "direct=auto:" +
        "me=umh:" +
        "subme=11:" +
        "trellis=2:" +
        "rc-lookahead=60:" +
        "aq-mode=3:" +
        "aq-strength=1.0:" +
        "psy-rd=1.0,0.15:" +
        "deblock=-1,-1:" +
        "weightp=2:" +
        "cabac=1:" +
        "merange=32";

    // 公共部分（两种模式共用的基础参数）
    string commonBase =
        $"-i {EscapePathArgument(input)} " +
        $"-vf \"{ScaleHelper.Build(width, height, ScaleUpCheck.IsChecked == true)}\" " +
        $"-r {fps} " +
        "-c:v libx264 " +
        "-preset veryslow " +
        $"-profile:v high " +
        "-level 5.2 " +
        $"-g {gopSize} " +
        $"-keyint_min {keyintMin} " +
        $"-x264-params \"{x264Params}\" " +
        "-pix_fmt yuv420p " +
        "-an ";

    string pass1, pass2;

    if (isTwoPass)
    {
        // ============ 2pass 模式 ============
        string twoPassCommon = commonBase +
            $"-b:v {bitrate}k " +
            $"-maxrate {maxrate} " +
            $"-bufsize {bufsize} ";

        pass1 =
            $"{twoPassCommon} " +
            "-pass 1 " +
            $"-passlogfile {EscapePathArgument(logFileName)} " +
            "-f mp4 NUL";

        pass2 =
            $"{twoPassCommon} " +
            "-pass 2 " +
            $"-passlogfile {EscapePathArgument(logFileName)} " +
            $"{EscapePathArgument(output)}";
    }
    else
    {
        // ============ CRF 模式 ============
        // CRF模式是单次编码，使用 -crf 参数
        // 不需要 2pass 相关参数

        string crfArgs = commonBase +
            $"-crf {crfValue} ";

        // CRF模式下，pass1 为空（不执行），pass2 为实际编码命令
        pass1 = "";
        pass2 = $"{crfArgs}{EscapePathArgument(output)}";
        
        // CRF模式不需要日志文件
        logFileName = "";
    }

    // ===== 音轨提取参数 =====
    string? audioExtractArgs = null;
    if (ExtractAudioCheck.IsChecked == true)
    {
        string audioOutput = Path.Combine(
            Path.GetDirectoryName(output)!,
            Path.GetFileNameWithoutExtension(output) + "_audio.m4a");

        // 使用 -c:a copy 直接复制音频流，不重新编码，速度极快
        audioExtractArgs =
            $"-i {EscapePathArgument(input)} " +
            "-vn -c:a copy " +
            $"{EscapePathArgument(audioOutput)}";
    }

    return (pass1, pass2, output, logFileName, audioExtractArgs);
}

    // =============================================================
    // Encode mode change handler
    // =============================================================

    private void EncodeMode_Changed(object sender, RoutedEventArgs e)
    {
        if (BitrateBox == null) return;

        if (TwoPassRadio.IsChecked == true)
        {
            // 2pass模式：显示比特率，默认800
            BitrateBox.Text = "800";
        }
        else if (CrfRadio.IsChecked == true)
        {
            // CRF模式：显示CRF值，默认23
            BitrateBox.Text = "26";
        }
    }


    // =============================================================

    private void SaveLog_Click(object sender, RoutedEventArgs e)
    {
        var logOutputPath = Path.Combine(Path.GetDirectoryName(_lastOutputPath)!,
                        $"log_{DateTime.Now:yyyyMMddHHmmss}_{new Random().Next(1000, 9999)}.txt");
        File.WriteAllText(logOutputPath,
                        _log.ToString(),
                        Encoding.UTF8);
    }

    private void About_Click(object sender, RoutedEventArgs e)
    {
        var aboutDialog = new AboutDialog();
        aboutDialog.ShowDialog();
    }

    private void LanguageChanged(object sender, System.Windows.Controls.SelectionChangedEventArgs e)
    {
        string lang = LanguageBox.SelectedIndex == 0 ? "zh-CN" : "en-US";
        Application.Current.Resources.MergedDictionaries.Clear();
        Application.Current.Resources.MergedDictionaries.Add(
            new ResourceDictionary
            {
                Source = new Uri($"Resources/Strings.{lang}.xaml", UriKind.Relative)
            });
    }

    private void Input_Drop(object sender, DragEventArgs e)
    {
        if (e.Data.GetData(DataFormats.FileDrop) is string[] files && files.Length > 0)
        {
            string? droppedPath = files[0];
            // 验证路径是否合法且为文件
            if (!string.IsNullOrEmpty(droppedPath) &&
                ValidatePath(droppedPath) &&
                File.Exists(droppedPath))
            {
                InputPathBox.Text = droppedPath;
            }
            else
            {
                MessageBox.Show("Invalid file path dropped.", "Error", MessageBoxButton.OK, MessageBoxImage.Warning);
            }
        }
    }

    /// <summary>
    /// 验证路径是否安全，防止路径遍历攻击
    /// </summary>
    private static bool ValidatePath(string path)
    {
        try
        {
            // 获取完整路径以解析相对路径
            string fullPath = Path.GetFullPath(path);

            // 检查路径是否包含非法字符
            char[] invalidChars = Path.GetInvalidPathChars();
            if (fullPath.IndexOfAny(invalidChars) >= 0)
                return false;

            // 检查路径长度是否合理（防止超长路径攻击）
            if (fullPath.Length > 260) // Windows MAX_PATH 默认限制
                return false;

            return true;
        }
        catch
        {
            return false;
        }
    }

    /// <summary>
    /// 安全转义路径参数以防止命令注入
    /// FFmpeg 接受的路径需要用双引号包裹，并对内部引号进行转义
    /// </summary>
    private static string EscapePathArgument(string path)
    {
        if (string.IsNullOrEmpty(path))
            return "";

        // 获取完整路径并验证
        try
        {
            path = Path.GetFullPath(path);
        }
        catch
        {
            return "";
        }

        // 验证路径安全性
        if (!ValidatePath(path))
            return "";

        // Windows 下：将内部的双引号替换为转义序列
        // FFmpeg 命令行使用反斜杠转义引号
        string escaped = path.Replace("\"", "\\\"");

        // 用双引号包裹整个路径
        return $"\"{escaped}\"";
    }

    // =============================================================
    // Window cleanup
    // =============================================================

    private void MainWindow_Closing(object? sender, System.ComponentModel.CancelEventArgs e)
    {
        // 停止所有进程并清理临时文件
        try
        {
            _ffmpeg.CleanupAll(_lastOutputPath, _currentLogFileName);
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"Error during cleanup: {ex.Message}");
        }

        // 额外清理工作文件夹下由程序生成的临时文件
        try
        {
            if (!string.IsNullOrEmpty(_lastOutputPath))
            {
                string? outputDir = Path.GetDirectoryName(_lastOutputPath);
                if (!string.IsNullOrEmpty(outputDir) && Directory.Exists(outputDir))
                {
                    // 只删除我们生成的临时文件（使用确切的日志文件名前缀）
                    if (!string.IsNullOrEmpty(_currentLogFileName))
                    {
                        string baseName = Path.GetFileNameWithoutExtension(_currentLogFileName);
                        var patterns = new[] { $"{baseName}.log", $"{baseName}.log.mbtree" };

                        foreach (var pattern in patterns)
                        {
                            string tempFile = Path.Combine(outputDir, pattern);
                            if (File.Exists(tempFile))
                            {
                                try
                                {
                                    File.Delete(tempFile);
                                    Debug.WriteLine($"Deleted temp file: {tempFile}");
                                }
                                catch (Exception ex)
                                {
                                    Debug.WriteLine($"Failed to delete {tempFile}: {ex.Message}");
                                }
                            }
                        }
                    }
                }
            }
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"Error during directory cleanup: {ex.Message}");
        }
    }
}
