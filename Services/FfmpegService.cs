using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading;
using System.Threading.Tasks;

namespace x264video4osu.Services;

public class FfmpegService
{
    private Process? _process;
    private Process? _ffprobeProcess;
    private readonly List<Process> _trackedProcesses = new();
    private CancellationTokenSource? _cts;

    public event Action<ProgressInfo>? ProgressChanged;
    public event Action<string>? Completed;
    public event Action<string>? LogReceived;
    public event Action<string>? LogMessage;

    /// <summary>
    /// Start ffmpeg encoding asynchronously.
    /// Supports both 2-pass and CRF (single-pass) encoding modes.
    /// </summary>
    public async Task StartAsync(
        string ffmpegArgsPass1,
        string ffmpegArgsPass2,
        string outputPath,
        string logFileName,
        string? audioExtractArgs = null)
    {
        _cts = new CancellationTokenSource();
        var startTime = DateTime.Now;

        // Extract input path from arguments
        string inputPath = ExtractInputPath(string.IsNullOrEmpty(ffmpegArgsPass1) ? ffmpegArgsPass2 : ffmpegArgsPass1);

        // 记录开始时间和输入文件信息
        LogMessage?.Invoke($"");
        LogMessage?.Invoke($"========================================");
        LogMessage?.Invoke($"[START] Encoding started at {startTime:yyyy-MM-dd HH:mm:ss}");
        LogMessage?.Invoke($"========================================");

        // 记录输入文件信息
        if (!string.IsNullOrEmpty(inputPath) && File.Exists(inputPath))
        {
            var inputFileInfo = new FileInfo(inputPath);
            LogMessage?.Invoke($"[INPUT] File: {inputPath}");
            LogMessage?.Invoke($"[INPUT] Size: {inputFileInfo.Length / 1024.0 / 1024.0:F2} MB");
        }
        else
        {
            LogMessage?.Invoke($"[INPUT] File: {inputPath ?? "(unknown)"}");
            LogMessage?.Invoke($"[WARN] Input file not found or cannot be read");
        }

        // 记录输出路径
        LogMessage?.Invoke($"[OUTPUT] Path: {outputPath}");

        // Get total duration using ffprobe
        double totalDuration = await GetMediaDurationAsync(inputPath ?? "");
        if (totalDuration > 0)
        {
            var durationTs = TimeSpan.FromSeconds(totalDuration);
            LogMessage?.Invoke($"[INFO] Duration: {durationTs:hh\\:mm\\:ss\\.fff} ({totalDuration:F2}s)");
        }

        // 动态计算总 pass 数
        // 如果pass1为空，则是CRF模式（单次编码）
        int passCount = string.IsNullOrEmpty(ffmpegArgsPass1)
            ? (string.IsNullOrEmpty(audioExtractArgs) ? 1 : 2)   // CRF: 编码(+音轨提取)
            : (string.IsNullOrEmpty(audioExtractArgs) ? 2 : 3);  // 2pass: pass1+pass2(+音轨提取)

        try
        {
            int currentPass = 1;

            // ---------- PASS 1 (仅在2pass模式执行) ----------
            if (!string.IsNullOrEmpty(ffmpegArgsPass1))
            {
                LogMessage?.Invoke($"");
                LogMessage?.Invoke($"[PASS 1/2] Starting first pass (analysis)...");
                LogMessage?.Invoke($"[CMD] ffmpeg {ffmpegArgsPass1}");

                await RunProcessAsync(
                    ffmpegArgsPass1,
                    passIndex: currentPass,
                    passCount: passCount,
                    startTime,
                    outputPath,
                    totalDuration);

                if (_cts.IsCancellationRequested)
                {
                    LogMessage?.Invoke($"");
                    LogMessage?.Invoke($"[CANCELLED] Encoding cancelled by user at {DateTime.Now:yyyy-MM-dd HH:mm:ss}");
                    LogMessage?.Invoke($"========================================");
                    return;
                }

                LogMessage?.Invoke($"[PASS 1/2] First pass completed");
                currentPass++;
            }

            // ---------- PASS 2 / CRF 编码 ----------
            bool isCrfMode = string.IsNullOrEmpty(ffmpegArgsPass1);
            string passDescription = isCrfMode ? "CRF encoding" : "Second pass";
            int displayPass = isCrfMode ? 1 : 2;
            int displayTotal = isCrfMode ? (string.IsNullOrEmpty(audioExtractArgs) ? 1 : 2) : (string.IsNullOrEmpty(audioExtractArgs) ? 2 : 3);

            LogMessage?.Invoke($"");
            LogMessage?.Invoke($"[PASS {displayPass}/{displayTotal}] Starting {passDescription}...");
            LogMessage?.Invoke($"[CMD] ffmpeg {ffmpegArgsPass2}");

            await RunProcessAsync(
                ffmpegArgsPass2,
                passIndex: currentPass,
                passCount: passCount,
                startTime,
                outputPath,
                totalDuration);

            if (_cts.IsCancellationRequested)
            {
                LogMessage?.Invoke($"");
                LogMessage?.Invoke($"[CANCELLED] Encoding cancelled by user at {DateTime.Now:yyyy-MM-dd HH:mm:ss}");
                LogMessage?.Invoke($"========================================");
                return;
            }

            LogMessage?.Invoke($"[PASS {displayPass}/{displayTotal}] {passDescription} completed");

            // ---------- AUDIO EXTRACT (Optional PASS) ----------
            if (!string.IsNullOrEmpty(audioExtractArgs))
            {
                LogMessage?.Invoke($"");
                LogMessage?.Invoke($"[PASS 3/3] Starting audio extraction...");
                LogMessage?.Invoke($"[CMD] ffmpeg {audioExtractArgs}");

                await RunProcessAsync(
                    audioExtractArgs,
                    passIndex: currentPass + 1,
                    passCount: passCount,
                    startTime,
                    outputPath,
                    totalDuration);

                if (_cts.IsCancellationRequested)
                {
                    LogMessage?.Invoke($"");
                    LogMessage?.Invoke($"[CANCELLED] Encoding cancelled by user at {DateTime.Now:yyyy-MM-dd HH:mm:ss}");
                    LogMessage?.Invoke($"========================================");
                    return;
                }

                LogMessage?.Invoke($"[PASS 3/3] Audio extraction completed");
            }

            // 清理临时文件（仅2pass模式需要）
            if (!string.IsNullOrEmpty(logFileName))
            {
                CleanupTempFiles(outputPath, logFileName);
            }

            // 记录完成信息
            var endTime = DateTime.Now;
            var totalElapsed = endTime - startTime;

            LogMessage?.Invoke($"");
            LogMessage?.Invoke($"========================================");
            LogMessage?.Invoke($"[COMPLETE] Encoding finished at {endTime:yyyy-MM-dd HH:mm:ss}");
            LogMessage?.Invoke($"[COMPLETE] Total elapsed time: {totalElapsed:hh\\:mm\\:ss}");
            LogMessage?.Invoke($"========================================");

            // 记录输出文件信息
            if (File.Exists(outputPath))
            {
                var outputFileInfo = new FileInfo(outputPath);
                LogMessage?.Invoke($"[OUTPUT] File created: {outputPath}");
                LogMessage?.Invoke($"[OUTPUT] Size: {outputFileInfo.Length / 1024.0 / 1024.0:F2} MB");
            }
            else
            {
                LogMessage?.Invoke($"[ERROR] Output file not found: {outputPath}");
            }

            LogMessage?.Invoke($"");

            Completed?.Invoke(outputPath);
        }
        catch (OperationCanceledException)
        {
            // 用户主动取消，不触发 Completed 事件，直接退出
            LogMessage?.Invoke($"");
            LogMessage?.Invoke($"[CANCELLED] Encoding cancelled by user at {DateTime.Now:yyyy-MM-dd HH:mm:ss}");
            LogMessage?.Invoke($"========================================");
        }
        catch (Exception ex)
        {
            LogMessage?.Invoke($"");
            LogMessage?.Invoke($"[ERROR] Encoding failed: {ex.Message}");
            LogMessage?.Invoke($"[ERROR] {ex.StackTrace}");
            LogMessage?.Invoke($"========================================");
            throw;
        }
    }

    /// <summary>
    /// Stop current encoding process.
    /// </summary>
    public void Stop()
    {
        LogMessage?.Invoke($"");
        LogMessage?.Invoke($"[STOP] Stopping encoding process...");

        // 取消所有异步操作
        _cts?.Cancel();

        // 清理所有跟踪的进程
        KillAllTrackedProcesses();
    }

    /// <summary>
    /// Kill all tracked processes (ffmpeg and ffprobe).
    /// </summary>
    private void KillAllTrackedProcesses()
    {
        var processesToKill = new List<Process>();

        if (_process != null && !_process.HasExited)
            processesToKill.Add(_process);

        if (_ffprobeProcess != null && !_ffprobeProcess.HasExited)
            processesToKill.Add(_ffprobeProcess);

        processesToKill.AddRange(_trackedProcesses.Where(p => !p.HasExited));

        if (processesToKill.Count == 0)
        {
            LogMessage?.Invoke($"[STOP] No active processes to stop");
            return;
        }

        foreach (var proc in processesToKill)
        {
            try
            {
                if (proc.StandardInput?.BaseStream.CanWrite == true)
                {
                    proc.StandardInput.WriteLine("q");
                    proc.StandardInput.Flush();
                }
            }
            catch (Exception ex)
            {
                LogMessage?.Invoke($"[WARN] Failed to send 'q' to process: {ex.Message}");
            }
        }

        foreach (var proc in processesToKill)
        {
            try
            {
                // 先尝试优雅终止
                proc.CloseMainWindow();

                // 等待最多 2 秒让进程正常退出
                if (!proc.WaitForExit(2000))
                {
                    // 如果超时，强制终止进程树
                    proc.Kill(true);
                }
            }
            catch (Exception ex)
            {
                LogMessage?.Invoke($"[WARN] Failed to terminate process: {ex.Message}");
            }
            finally
            {
                proc.Dispose();
            }
        }

        // 清空跟踪列表
        _process = null;
        _ffprobeProcess = null;
        _trackedProcesses.Clear();
    }

    // =============================================================
    // Core process runner
    // =============================================================

    private async Task RunProcessAsync(
        string args,
        int passIndex,
        int passCount,
        DateTime startTime,
        string outputPath,
        double totalDuration)
    {
        var progressRegex = new Regex(@"time=(\d+):(\d+):(\d+\.\d+)",
            RegexOptions.Compiled);

        using var process = new Process
        {
            StartInfo = new ProcessStartInfo
            {
                FileName = FfmpegConfig.FfmpegPath,
                Arguments = args,
                RedirectStandardError = true,
                UseShellExecute = false,
                CreateNoWindow = true,
                StandardErrorEncoding = Encoding.UTF8,
                WorkingDirectory = Path.GetDirectoryName(outputPath)
            }
        };

        _process = process;

        process.ErrorDataReceived += (_, e) =>
        {
            if (string.IsNullOrWhiteSpace(e.Data))
                return;

            LogReceived?.Invoke(e.Data);

            var match = progressRegex.Match(e.Data);
            if (!match.Success || totalDuration <= 0)
                return;

            // Parse encoded timestamp
            int h = int.Parse(match.Groups[1].Value);
            int m = int.Parse(match.Groups[2].Value);
            double s = double.Parse(match.Groups[3].Value);

            double currentSeconds = h * 3600 + m * 60 + s;

            double percent = currentSeconds / totalDuration * 100.0;
            percent = Math.Clamp(percent, 0, 100);

            var elapsed = DateTime.Now - startTime;

            double speed = elapsed.TotalSeconds > 0
                ? currentSeconds / elapsed.TotalSeconds
                : 0;

            double remainingSeconds = speed > 0
                ? (totalDuration - currentSeconds) / speed
                : 0;

            var info = new ProgressInfo
            {
                PassIndex = passIndex,
                PassCount = passCount,
                Percent = percent,
                Elapsed = elapsed,
                EstimatedRemaining = TimeSpan.FromSeconds(remainingSeconds)
            };

            ProgressChanged?.Invoke(info);
        };

        process.Start();
        process.BeginErrorReadLine();

        try
        {
            await process.WaitForExitAsync(_cts!.Token);
        }
        catch (OperationCanceledException)
        {
            // 取消时不抛出异常，正常退出 using 块并释放资源
        }
        finally
        {
            _process = null;
        }
    }

    // =============================================================
    // Duration detection via ffprobe
    // =============================================================

    /// <summary>
    /// Get total media duration in seconds using ffprobe.
    /// Returns 0 if failed.
    /// </summary>
    private async Task<double> GetMediaDurationAsync(string inputPath)
    {
        if (string.IsNullOrWhiteSpace(inputPath))
            return 0;

        try
        {
            var psi = new ProcessStartInfo
            {
                FileName = FfmpegConfig.FfprobePath,
                Arguments =
                    $"-v error -show_entries format=duration " +
                    $"-of default=noprint_wrappers=1:nokey=1 \"{inputPath}\"",
                RedirectStandardOutput = true,
                UseShellExecute = false,
                CreateNoWindow = true
            };

            using var probe = Process.Start(psi);
            if (probe == null)
                return 0;

            _ffprobeProcess = probe;

            string output = await probe.StandardOutput.ReadToEndAsync();
            await probe.WaitForExitAsync();

            _ffprobeProcess = null;

            if (double.TryParse(output.Trim(), out double duration))
                return duration;

            return 0;
        }
        catch (Exception ex)
        {
            LogMessage?.Invoke($"[WARN] Failed to get media duration: {ex.Message}");
            return 0;
        }
    }

    /// <summary>
    /// Extract input file path from ffmpeg argument string.
    /// Assumes "-i \"path\"" exists.
    /// </summary>
    private string ExtractInputPath(string args)
    {
        var match = Regex.Match(args, "-i\\s+\"([^\"]+)\"");
        return match.Success ? match.Groups[1].Value : "";
    }

    // =============================================================
    // Cleanup
    // =============================================================

    /// <summary>
    /// Stop any running processes and clean up all temporary files.
    /// </summary>
    public void CleanupAll(string? outputPath = null, string? logFileName = null)
    {
        // 停止所有正在运行的进程
        Stop();

        // 清理临时文件
        if (!string.IsNullOrEmpty(outputPath) && !string.IsNullOrEmpty(logFileName))
        {
            CleanupTempFiles(outputPath, logFileName);
        }
    }

    /// <summary>
    /// Delete temporary log and mbtree files generated by ffmpeg 2-pass.
    /// 只删除确切名称的临时文件，不使用通配符以防止误删用户文件。
    /// </summary>
    public void CleanupTempFiles(string? outputPath, string? logFileName)
    {
        if (string.IsNullOrWhiteSpace(logFileName) ||
            string.IsNullOrWhiteSpace(outputPath))
            return;

        try
        {
            string? dir = Path.GetDirectoryName(outputPath);
            if (string.IsNullOrEmpty(dir) || !Directory.Exists(dir))
                return;

            // 只删除我们确切知道的临时文件，不使用通配符
            string baseName = Path.GetFileNameWithoutExtension(logFileName);
            var tempFiles = new[]
            {
                Path.Combine(dir, baseName + ".log"),
                Path.Combine(dir, baseName + ".log.mbtree")
            };

            foreach (var file in tempFiles)
            {
                if (File.Exists(file))
                {
                    try
                    {
                        File.Delete(file);
                        LogReceived?.Invoke($"[Cleanup] Deleted: {Path.GetFileName(file)}");
                    }
                    catch (Exception ex)
                    {
                        LogReceived?.Invoke($"[CleanupFail] {ex.Message}");
                    }
                }
            }
        }
        catch (Exception ex)
        {
            LogReceived?.Invoke($"[CleanupError] {ex.Message}");
        }
    }
}
