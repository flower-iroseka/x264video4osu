//! FFmpeg 编码编排服务（对应旧程序 Services/FfmpegService.cs）。
//!
//! 在后台线程运行编码流程，通过 mpsc 通道向 UI 发送事件。
//! 停止通过原子标志 + 写入 "q" + taskkill 进程树实现。

use crate::error::AppError;
use crate::platform::time;
use crate::services::args::EncodePlan;
use crate::services::ffmpeg_config;
use regex::Regex;
use std::io::{BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 后台编码流程发给 UI 的事件
#[derive(Debug, Clone)]
pub enum Event {
    /// 日志行（状态横幅 + ffmpeg stderr 行），逐行追加到日志框
    Log(String),
    /// 进度更新
    Progress {
        pass_index: u32,
        pass_count: u32,
        percent: f64,
        elapsed_secs: f64,
        remaining_secs: f64,
    },
    /// 正常完成
    Completed(String),
    /// 用户取消（不触发 Completed）
    Cancelled,
    /// 编码失败（启动子进程失败等）
    Failed(String),
}

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct FfmpegService {
    tx: Sender<Event>,
    stop_flag: Arc<AtomicBool>,
    /// 是否有正在运行的编码子进程（供 "[STOP] No active processes" 判断）
    active: Arc<AtomicBool>,
    /// 当前正在运行的 ffmpeg 子进程 PID（窗口关闭时同步强杀用）
    current_pid: Arc<Mutex<Option<u32>>>,
}

impl FfmpegService {
    pub fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            Self {
                tx,
                stop_flag: Arc::new(AtomicBool::new(false)),
                active: Arc::new(AtomicBool::new(false)),
                current_pid: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }

    /// 停止当前编码（对应 `FfmpegService.Stop`）。
    pub fn stop(&self) {
        let _ = self.tx.send(Event::Log(String::new()));
        let _ = self.tx.send(Event::Log("[STOP] Stopping encoding process...".into()));
        self.stop_flag.store(true, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            let _ = self.tx.send(Event::Log("[STOP] No active processes to stop".into()));
        }
    }

    /// 立即强制终止当前 ffmpeg 进程树（窗口关闭时使用，避免留下孤儿进程）。
    /// 正常停止路径由工作线程执行优雅停止，不调用此方法。
    pub fn kill_now(&self) {
        if let Some(pid) = *self.current_pid.lock().unwrap() {
            let _ = kill_process_tree(pid);
        }
    }

    /// 重置停止标志（仅在启动新的编码前调用）
    pub fn reset(&self) {
        self.stop_flag.store(false, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
        *self.current_pid.lock().unwrap() = None;
    }

    /// 在后台线程启动编码流程
    pub fn start(&self, plan: EncodePlan) {
        let tx = self.tx.clone();
        let stop_flag = self.stop_flag.clone();
        let active = self.active.clone();
        let current_pid = self.current_pid.clone();
        let ffmpeg = ffmpeg_config::ffmpeg_path();
        let ffprobe = ffmpeg_config::ffprobe_path();
        thread_spawn(move || {
            if let Err(e) = run_encoding(&tx, &stop_flag, &active, &current_pid, &ffmpeg, &ffprobe, &plan) {
                let _ = tx.send(Event::Failed(format!("{e}")));
            }
        });
    }
}

fn thread_spawn<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let _ = std::thread::Builder::new().name("ffmpeg-worker".into()).spawn(f);
}

fn log(tx: &Sender<Event>, msg: String) {
    let _ = tx.send(Event::Log(msg));
}

fn emit_cancelled(tx: &Sender<Event>) {
    log(tx, String::new());
    log(tx, format!("[CANCELLED] Encoding cancelled by user at {}", time::local_timestamp()));
    log(tx, "========================================".into());
    let _ = tx.send(Event::Cancelled);
}

/// 从 token 中提取 `-i` 之后的输入路径
fn extract_input(tokens: &[String]) -> String {
    tokens
        .windows(2)
        .find(|w| w[0] == "-i")
        .map(|w| w[1].clone())
        .unwrap_or_default()
}

fn run_encoding(
    tx: &Sender<Event>,
    stop: &AtomicBool,
    active: &AtomicBool,
    current_pid: &Mutex<Option<u32>>,
    ffmpeg: &Path,
    ffprobe: &Path,
    plan: &EncodePlan,
) -> Result<(), AppError> {
    let start_time = Instant::now();

    // 从参数中提取输入路径（pass1 为空则取 pass2）
    let input_path = if plan.pass1.is_empty() {
        extract_input(&plan.pass2)
    } else {
        extract_input(&plan.pass1)
    };

    // 记录开始时间和输入文件信息
    log(tx, String::new());
    log(tx, "========================================".into());
    log(tx, format!("[START] Encoding started at {}", time::local_timestamp()));
    log(tx, "========================================".into());

    if !input_path.is_empty() && Path::new(&input_path).exists() {
        let size = std::fs::metadata(&input_path)
            .map(|m| m.len())
            .unwrap_or(0);
        log(tx, format!("[INPUT] File: {input_path}"));
        log(tx, format!("[INPUT] Size: {:.2} MB", size as f64 / 1024.0 / 1024.0));
    } else {
        let shown = if input_path.is_empty() { "(unknown)" } else { input_path.as_str() };
        log(tx, format!("[INPUT] File: {shown}"));
        log(tx, "[WARN] Input file not found or cannot be read".into());
    }

    log(tx, format!("[OUTPUT] Path: {}", plan.output.display()));

    // 通过 ffprobe 获取总时长
    let total_duration = get_media_duration(tx, ffprobe, &input_path);
    if total_duration > 0.0 {
        log(tx, format!(
            "[INFO] Duration: {} ({total_duration:.2}s)",
            time::format_hms_ms(total_duration)
        ));
    }

    // 动态计算总 pass 数
    let has_audio = plan.audio_extract.is_some();
    let pass_count: u32 = if plan.pass1.is_empty() {
        if has_audio { 2 } else { 1 }
    } else {
        if has_audio { 3 } else { 2 }
    };

    let mut current_pass: u32 = 1;

    // ---------- PASS 1（仅 2pass） ----------
    if !plan.pass1.is_empty() {
        log(tx, String::new());
        log(tx, "[PASS 1/2] Starting first pass (analysis)...".into());
        log(tx, format!("[CMD] ffmpeg {}", plan.display_pass1));
        let cancelled = run_process(
            tx, stop, active, current_pid, ffmpeg, &plan.pass1, &plan.output,
            start_time, current_pass, pass_count, total_duration,
        )?;
        if cancelled || stop.load(Ordering::SeqCst) {
            emit_cancelled(tx);
            return Ok(());
        }
        log(tx, "[PASS 1/2] First pass completed".into());
        current_pass += 1;
    }

    // ---------- PASS 2 / CRF 编码 ----------
    let is_crf = plan.pass1.is_empty();
    let pass_description = if is_crf { "CRF encoding" } else { "Second pass" };
    let display_pass = if is_crf { 1 } else { 2 };
    let display_total = if is_crf {
        if has_audio { 2 } else { 1 }
    } else {
        if has_audio { 3 } else { 2 }
    };

    log(tx, String::new());
    log(tx, format!("[PASS {display_pass}/{display_total}] Starting {pass_description}..."));
    log(tx, format!("[CMD] ffmpeg {}", plan.display_pass2));
    let cancelled = run_process(
        tx, stop, active, current_pid, ffmpeg, &plan.pass2, &plan.output,
        start_time, current_pass, pass_count, total_duration,
    )?;
    if cancelled || stop.load(Ordering::SeqCst) {
        emit_cancelled(tx);
        return Ok(());
    }
    log(tx, format!("[PASS {display_pass}/{display_total}] {pass_description} completed"));

    // ---------- AUDIO EXTRACT（可选） ----------
    if let Some(audio_args) = &plan.audio_extract {
        log(tx, String::new());
        log(tx, "[PASS 3/3] Starting audio extraction...".into());
        log(tx, format!("[CMD] ffmpeg {}", plan.display_audio));
        let cancelled = run_process(
            tx, stop, active, current_pid, ffmpeg, audio_args, &plan.output,
            start_time, current_pass + 1, pass_count, total_duration,
        )?;
        if cancelled || stop.load(Ordering::SeqCst) {
            emit_cancelled(tx);
            return Ok(());
        }
        log(tx, "[PASS 3/3] Audio extraction completed".into());
    }

    // 清理临时文件（仅 2pass 模式需要）
    if !plan.log_file_name.is_empty() {
        cleanup_temp_files(tx, &plan.output, &plan.log_file_name);
    }

    // 记录完成信息
    let total_elapsed = start_time.elapsed().as_secs_f64();
    log(tx, String::new());
    log(tx, "========================================".into());
    log(tx, format!("[COMPLETE] Encoding finished at {}", time::local_timestamp()));
    log(tx, format!("[COMPLETE] Total elapsed time: {}", time::format_hms(total_elapsed)));
    log(tx, "========================================".into());

    if plan.output.exists() {
        let size = std::fs::metadata(&plan.output).map(|m| m.len()).unwrap_or(0);
        log(tx, format!("[OUTPUT] File created: {}", plan.output.display()));
        log(tx, format!("[OUTPUT] Size: {:.2} MB", size as f64 / 1024.0 / 1024.0));
    } else {
        log(tx, format!("[ERROR] Output file not found: {}", plan.output.display()));
    }
    log(tx, String::new());

    let _ = tx.send(Event::Completed(plan.output.to_string_lossy().into_owned()));
    Ok(())
}

/// 运行单个 ffmpeg 子进程，返回是否被取消。
fn run_process(
    tx: &Sender<Event>,
    stop: &AtomicBool,
    active: &AtomicBool,
    current_pid: &Mutex<Option<u32>>,
    ffmpeg: &Path,
    tokens: &[String],
    output: &Path,
    start_time: Instant,
    pass_index: u32,
    pass_count: u32,
    total_duration: f64,
) -> Result<bool, AppError> {
    let cwd: PathBuf = output
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();

    let mut cmd = Command::new(ffmpeg);
    cmd.args(tokens)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::EncodeFailed(format!("Failed to start ffmpeg: {e}")))?;
    let pid = child.id();
    active.store(true, Ordering::SeqCst);
    *current_pid.lock().unwrap() = Some(pid);

    // ---------- stderr 读取线程 ----------
    let tx2 = tx.clone();
    let mut stderr = child.stderr.take().expect("stderr piped");
    let reader = thread_spawn_and_join(move || {
        read_stderr_and_report(&mut stderr, &tx2, start_time, pass_index, pass_count, total_duration);
    });

    // ---------- 等待退出 / 取消 ----------
    let mut cancelled = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            cancelled = true;
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => {
                active.store(false, Ordering::SeqCst);
                return Err(AppError::EncodeFailed(format!("Failed to wait ffmpeg: {e}")));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if cancelled {
        // 优雅停止：先写 'q'，等待 2 秒，超时再强制终止进程树
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "q");
            let _ = stdin.flush();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Ok(Some(_)) = child.try_wait() {
            let _ = child.wait();
        } else {
            let _ = kill_process_tree(pid);
            let _ = child.wait();
        }
    } else {
        let _ = child.wait();
    }

    active.store(false, Ordering::SeqCst);
    *current_pid.lock().unwrap() = None;
    reader.join().ok();
    Ok(cancelled)
}

/// 读取 stderr：把 `\r` 和 `\n` 都当作行分隔符（ffmpeg 进度用 `\r` 原地刷新），
/// 逐行发日志并解析进度。
fn read_stderr_and_report(
    stderr: &mut impl std::io::Read,
    tx: &Sender<Event>,
    start_time: Instant,
    pass_index: u32,
    pass_count: u32,
    total_duration: f64,
) {
    let progress_regex = Regex::new(r"time=(\d+):(\d+):(\d+\.\d+)").expect("regex ok");
    let mut reader = BufReader::new(stderr);
    let mut chunk: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        chunk.clear();
        loop {
            match reader.read(&mut byte) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    // byte 只有 1 字节，read 最多返回 1
                    if byte[0] == b'\r' || byte[0] == b'\n' {
                        break;
                    }
                    chunk.push(byte[0]);
                }
                Err(_) => return,
            }
        }
        if chunk.is_empty() {
            // 连续的分隔符，或 EOF
            match reader.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    // 有更多内容但 chunk 为空说明是空行或前导分隔符
                    if byte[0] != b'\r' && byte[0] != b'\n' {
                        chunk.push(byte[0]);
                        continue;
                    }
                    // 连续分隔符，重新走外层循环
                }
                Err(_) => break,
            }
        }
        let line = String::from_utf8_lossy(&chunk);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = tx.send(Event::Log(trimmed.to_string()));

        // 进度解析
        if total_duration <= 0.0 {
            continue;
        }
        if let Some(caps) = progress_regex.captures(&line) {
            let h: f64 = caps[1].parse().unwrap_or(0.0);
            let m: f64 = caps[2].parse().unwrap_or(0.0);
            let s: f64 = caps[3].parse().unwrap_or(0.0);
            let current = h * 3600.0 + m * 60.0 + s;
            let percent = (current / total_duration * 100.0).clamp(0.0, 100.0);
            let elapsed = start_time.elapsed().as_secs_f64();
            let speed = if elapsed > 0.0 { current / elapsed } else { 0.0 };
            let remaining = if speed > 0.0 {
                (total_duration - current) / speed
            } else {
                0.0
            };
            let _ = tx.send(Event::Progress {
                pass_index,
                pass_count,
                percent,
                elapsed_secs: elapsed,
                remaining_secs: remaining,
            });
        }
    }
}

/// 读取 stderr 的辅助线程（返回 JoinHandle 供外层 join）
fn thread_spawn_and_join<F>(f: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name("stderr-reader".into())
        .spawn(f)
        .expect("spawn stderr reader")
}

/// 通过 ffprobe 获取媒体总时长（秒），失败返回 0。
fn get_media_duration(tx: &Sender<Event>, ffprobe: &Path, input_path: &str) -> f64 {
    if input_path.is_empty() {
        return 0.0;
    }

    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ])
    .arg(input_path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .creation_flags(CREATE_NO_WINDOW);

    match cmd.output() {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.trim().parse::<f64>().unwrap_or(0.0)
        }
        Err(e) => {
            log(tx, format!("[WARN] Failed to get media duration: {e}"));
            0.0
        }
    }
}

/// 强制终止进程树（`taskkill /F /T /PID <pid>`）。
fn kill_process_tree(pid: u32) -> std::io::Result<()> {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    cmd.status()?;
    Ok(())
}

/// 删除 2pass 生成的临时日志文件（只删除确切名称，不用通配符）。
/// 注意：与旧程序一致，这里检查输出目录；由于 passlogfile 被解析为应用
/// cwd 下的绝对路径，实际临时文件落在应用 cwd——此清理在旧程序中同样
/// 不会命中，我们保持等价行为。
fn cleanup_temp_files(tx: &Sender<Event>, output: &Path, log_file_name: &str) {
    let dir = match output.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => return,
    };
    if !dir.exists() {
        return;
    }
    let base = Path::new(log_file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| log_file_name.to_string());

    for pattern in [format!("{base}.log"), format!("{base}.log.mbtree")] {
        let file = dir.join(&pattern);
        if file.exists() {
            match std::fs::remove_file(&file) {
                Ok(()) => {
                    let name = file.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or(pattern);
                    log(tx, format!("[Cleanup] Deleted: {name}"));
                }
                Err(e) => {
                    log(tx, format!("[CleanupFail] {e}"));
                }
            }
        }
    }
}

/// 清理编码产生的临时文件（供 UI 停止 / 关窗时调用）。
pub fn cleanup_temp_files_public(output: &str, log_file_name: &str) {
    if log_file_name.is_empty() {
        return;
    }
    let dir = Path::new(output).parent().map(|p| p.to_path_buf());
    if let Some(dir) = dir {
        if dir.as_os_str().is_empty() || !dir.exists() {
            return;
        }
        let base = Path::new(log_file_name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| log_file_name.to_string());
        for pattern in [format!("{base}.log"), format!("{base}.log.mbtree")] {
            let file = dir.join(&pattern);
            if file.exists() {
                let _ = std::fs::remove_file(&file);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::args::{build, EncodeInput};

    /// 定位仓库自带的 ffmpeg/ffprobe。
    /// cargo test 的 cwd 是 crate 根目录，tools 位于其上一级（仓库根）下。
    fn tools_dir() -> Option<PathBuf> {
        let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools");
        if tools.join("ffmpeg.exe").exists() && tools.join("ffprobe.exe").exists() {
            Some(tools)
        } else {
            None
        }
    }

    /// 用真实的 ffmpeg 生成 1 秒 64×64 测试输入（libx264 + yuv420p，与编码参数兼容）。
    /// tag 保证并行测试各自使用独立的输入/输出文件（同进程内 PID 相同）。
    fn make_input(ffmpeg: &Path, tag: &str) -> PathBuf {
        let input = std::env::temp_dir()
            .join(format!("x264video4osu_itest_{}_{}.mkv", tag, std::process::id()));
        let _ = std::fs::remove_file(&input);
        let status = Command::new(ffmpeg)
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x64:r=24:d=1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .expect("spawn ffmpeg to make test input");
        assert!(status.success(), "failed to generate test input");
        input
    }

    /// 端到端验证：用真实 ffmpeg 执行 args::build 生成的命令，确认
    /// ffmpeg 接受参数、成功写出输出文件，并触发 [START]/[COMPLETE]/Completed 事件。
    #[test]
    fn end_to_end_crf_encode_with_real_ffmpeg() {
        let Some(tools) = tools_dir() else {
            eprintln!("tools/ffmpeg.exe not found — skipping end-to-end encode test");
            return;
        };
        let ffmpeg = tools.join("ffmpeg.exe");
        let ffprobe = tools.join("ffprobe.exe");
        let input = make_input(&ffmpeg, "crf");

        let plan = build(EncodeInput {
            input_text: input.to_string_lossy().into_owned(),
            output_text: String::new(),
            format_index: 0, // flv
            mode_index: 0,   // CRF
            value_text: "26".into(),
            width_text: "0".into(),
            height_text: "0".into(), // scale=iw:ih，保持 64×64 快速编码
            scale_up: false,
            extract_audio: false,
        })
        .expect("plan should build");
        assert_eq!(plan.output.extension().map(|e| e.to_string_lossy().into_owned()), Some("flv".into()));

        let output = plan.output.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));
        let current_pid = Arc::new(Mutex::new(None));

        let res = run_encoding(&tx, &stop, &active, &current_pid, &ffmpeg, &ffprobe, &plan);
        assert!(res.is_ok(), "run_encoding failed: {:?}", res.as_ref().err());

        // 事件：必须出现 Completed，且日志含 [START] / [COMPLETE] / [OUTPUT] Size
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(
            events.iter().any(|e| matches!(e, Event::Completed(_))),
            "Expected Completed event, got: {events:?}"
        );
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[START]"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[COMPLETE]"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[OUTPUT] Size"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[INFO] Duration"))));

        // 输出文件真实存在且非空（证明 ffmpeg 接受了生成的参数并成功写出）
        let meta = std::fs::metadata(&output).expect("output should exist");
        assert!(meta.len() > 0, "output should not be empty");

        // 清理
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(&input);
    }

    /// 端到端验证 2pass：pass1 写 NUL + passlogfile，pass2 实际编码。
    /// 与旧程序一致，passlogfile 被解析为应用 cwd 下的绝对路径，
    /// 因此临时日志文件落在 cwd（cargo test 时即 crate 根），测试后手动清理。
    #[test]
    fn end_to_end_two_pass_encode_with_real_ffmpeg() {
        let Some(tools) = tools_dir() else {
            eprintln!("tools/ffmpeg.exe not found — skipping end-to-end 2pass test");
            return;
        };
        let ffmpeg = tools.join("ffmpeg.exe");
        let ffprobe = tools.join("ffprobe.exe");
        let input = make_input(&ffmpeg, "twopass");

        let plan = build(EncodeInput {
            input_text: input.to_string_lossy().into_owned(),
            output_text: String::new(),
            format_index: 0, // flv
            mode_index: 1,   // 2pass
            value_text: "1200".into(),
            width_text: "0".into(),
            height_text: "0".into(),
            scale_up: false,
            extract_audio: false,
        })
        .expect("plan should build");
        assert!(!plan.pass1.is_empty(), "2pass must have a pass1");

        let output = plan.output.clone();
        let log_name = plan.log_file_name.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));
        let current_pid = Arc::new(Mutex::new(None));

        let res = run_encoding(&tx, &stop, &active, &current_pid, &ffmpeg, &ffprobe, &plan);
        assert!(res.is_ok(), "run_encoding failed: {:?}", res.as_ref().err());

        let events: Vec<Event> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::Completed(_))), "Expected Completed");
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[PASS 1/2]"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[PASS 2/2]"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.contains("-pass 1"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[COMPLETE]"))));

        let meta = std::fs::metadata(&output).expect("output should exist");
        assert!(meta.len() > 0, "output should not be empty");

        // 清理（输出 + 输入 + 落在 cwd 的 passlog 临时文件）
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(&input);
        let cwd = std::env::current_dir().unwrap();
        for suffix in [".log", ".log.mbtree"] {
            let _ = std::fs::remove_file(cwd.join(format!("{log_name}{suffix}")));
        }
    }
}
