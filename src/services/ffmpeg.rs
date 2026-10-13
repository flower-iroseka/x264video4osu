//! FFmpeg 编码编排服务（对应旧程序 Services/FfmpegService.cs）。
//!
//! 在后台线程运行编码流程，通过 mpsc 通道向 UI 发送事件。
//! 停止通过原子标志 + 写入 "q" + taskkill 进程树实现。

use crate::error::AppError;
use crate::platform::time;
use crate::services::args::EncodePlan;
use crate::services::ffmpeg_config;
use crate::services::probe;
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
    /// 当前这次运行的句柄。每次 start 都换一份新的；旧 worker 手里握着的是它
    /// 自己那份 Arc，收工时写不到这里来。
    run: Mutex<Option<Run>>,
}

/// 一次编码运行独占的状态。
///
/// 停止标志、PID 都按运行各一份：不然「停止之后马上再点开始」时，新一轮会把这些
/// 共用状态清掉，旧 worker 看到停止标志又变回 false 就永远停不下来，收工时还会把
/// 新一轮刚写进去的 PID 覆盖成 None，关窗口时强杀就找不到进程了。
#[derive(Clone)]
struct Run {
    /// 还能不能往 UI 发事件。被新一次运行顶掉之后置 false，旧 worker 的日志、
    /// 进度、取消事件就丢掉 —— 否则它会用一个迟到的 Cancelled 把新一轮刚设好的
    /// 「编码中」界面状态改回「已停止」。
    live: Arc<Mutex<bool>>,
    /// 停止标志
    stop_flag: Arc<AtomicBool>,
    /// 是否有 ffmpeg 子进程在跑（供 "[STOP] No active processes" 判断）
    active: Arc<AtomicBool>,
    /// 当前 ffmpeg 子进程 PID（窗口关闭时同步强杀用）
    current_pid: Arc<Mutex<Option<u32>>>,
}

impl Run {
    fn new() -> Self {
        Self {
            live: Arc::new(Mutex::new(true)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(false)),
            current_pid: Arc::new(Mutex::new(None)),
        }
    }
}

/// 带代次过滤的事件发送端。
#[derive(Clone)]
struct RunSender {
    tx: Sender<Event>,
    live: Arc<Mutex<bool>>,
}

impl RunSender {
    fn send(&self, event: Event) {
        // 判断和发送在同一把锁里做完，堵住「刚判断完还活着、就被新运行顶掉、
        // 事件却还是发出去了」这个空档。
        if *self.live.lock().unwrap() {
            let _ = self.tx.send(event);
        }
    }
}

impl FfmpegService {
    pub fn new() -> (Self, Receiver<Event>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx, run: Mutex::new(None) }, rx)
    }

    /// 停止当前编码（对应 `FfmpegService.Stop`）。
    pub fn stop(&self) {
        let _ = self.tx.send(Event::Log(String::new()));
        let _ = self.tx.send(Event::Log("[STOP] Stopping encoding process...".into()));
        let run = self.run.lock().unwrap();
        let active = run
            .as_ref()
            .map(|r| {
                r.stop_flag.store(true, Ordering::SeqCst);
                r.active.load(Ordering::SeqCst)
            })
            .unwrap_or(false);
        if !active {
            let _ = self.tx.send(Event::Log("[STOP] No active processes to stop".into()));
        }
    }

    /// 立即强制终止当前 ffmpeg 进程树（窗口关闭时使用，避免留下孤儿进程）。
    /// 正常停止路径由工作线程执行优雅停止，不调用此方法。
    pub fn kill_now(&self) {
        let run = self.run.lock().unwrap();
        if let Some(r) = run.as_ref() {
            if let Some(pid) = *r.current_pid.lock().unwrap() {
                let _ = kill_process_tree(pid);
            }
        }
    }

    /// 在后台线程启动编码流程。
    ///
    /// 上一轮还没收工就再点一次开始（停止之后马上开始）时，先把它的进程杀掉、
    /// 标记作废。输出路径是按「文件不存在」算出来的，两次运行很可能落到同一个
    /// 文件上，同时写会写出坏文件。
    pub fn start(&self, plan: EncodePlan) {
        let previous = self.run.lock().unwrap().take();
        if let Some(prev) = previous {
            *prev.live.lock().unwrap() = false;
            prev.stop_flag.store(true, Ordering::SeqCst);
            if let Some(pid) = *prev.current_pid.lock().unwrap() {
                let _ = kill_process_tree(pid);
            }
        }

        let run = Run::new();
        *self.run.lock().unwrap() = Some(run.clone());

        let sender = RunSender {
            tx: self.tx.clone(),
            live: run.live.clone(),
        };
        let ffmpeg = ffmpeg_config::ffmpeg_path();
        let tx_fail = self.tx.clone();
        let spawned = thread_spawn(move || {
            if let Err(e) = run_encoding(
                &sender,
                &run.stop_flag,
                &run.active,
                &run.current_pid,
                &ffmpeg,
                &plan,
            ) {
                // 失败这条路以前不清理：2pass 的 passlog/mbtree 会永远留在输出目录里。
                // 取消那条路由上的清理在 app.rs（停止按钮、关窗口都会调）。
                if let Some(log_base) = &plan.log_file_path {
                    cleanup_temp_files(&sender, log_base);
                }
                sender.send(Event::Failed(format!("{e}")));
            }
        });
        if let Err(e) = spawned {
            // 线程起不来就一个事件都不会来，界面上会永远停在「编码中」。
            // 这里用原始 sender 直接发，绕过 RunSender 的存活过滤。
            let _ = tx_fail.send(Event::Failed(format!(
                "Failed to start encoder thread: {e}"
            )));
        }
    }
}

fn thread_spawn<F>(f: F) -> std::io::Result<()>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name("ffmpeg-worker".into())
        .spawn(f)
        .map(|_| ())
}

fn log(tx: &RunSender, msg: String) {
    tx.send(Event::Log(msg));
}

fn emit_cancelled(tx: &RunSender, label: &str) {
    log(tx, String::new());
    log(tx, format!("[CANCELLED] {label} cancelled by user at {}", time::local_timestamp()));
    log(tx, "========================================".into());
    tx.send(Event::Cancelled);
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
    tx: &RunSender,
    stop: &AtomicBool,
    active: &AtomicBool,
    current_pid: &Mutex<Option<u32>>,
    ffmpeg: &Path,
    plan: &EncodePlan,
) -> Result<(), AppError> {
    let start_time = Instant::now();

    // 从参数中提取输入路径（pass1 为空则取 pass2）
    let input_path = if plan.pass1.is_empty() {
        extract_input(&plan.pass2)
    } else {
        extract_input(&plan.pass1)
    };

    // 纯音频方案（只提取音轨）走的是同一个 worker，日志里的说法要跟着换，
    // 不然一次「提取音轨」的日志全在讲编码。
    let label = if plan.audio_only { "Audio extraction" } else { "Encoding" };

    // 记录开始时间和输入文件信息
    log(tx, String::new());
    log(tx, "========================================".into());
    log(tx, format!("[START] {label} started at {}", time::local_timestamp()));
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

    // 源视频总时长。EncodePlan 带了覆盖值时以它为准：视频轨被 edit list 复制过、
    // 用户又选了编两遍的文件，探测器报的只是头部声明的那一遍。
    let source_duration = plan
        .duration_override
        .unwrap_or_else(|| get_media_duration(tx, ffmpeg, &input_path));
    // 进度用的是「这一遍实际要编多长」，不是源视频全长：`-ss`/`-to` 都排在 -i 前面，
    // ffmpeg 的 time= 从 0 走到 (终点 - 起点)，拿全长当分母的话进度条永远走不满。
    let total_duration =
        output_duration(source_duration, plan.trim_start_secs, plan.trim_end_secs);
    if source_duration > 0.0 {
        log(tx, format!(
            "[INFO] Duration: {} ({source_duration:.2}s)",
            time::format_hms_ms(source_duration)
        ));
    }
    let trimmed = plan.trim_start_secs.is_some() || plan.trim_end_secs.is_some();
    if trimmed && total_duration > 0.0 {
        log(tx, format!(
            "[INFO] Encode length: {} ({total_duration:.2}s)",
            time::format_hms_ms(total_duration)
        ));
    }

    // 总 pass 数：CRF 和纯音频都是一遍，2pass 两遍
    let pass_count: u32 = if plan.pass1.is_empty() { 1 } else { 2 };

    let mut current_pass: u32 = 1;

    // 清 2pass 的临时日志。取消那条路以前不清：app.rs 是在 stop() 返回后立刻删的，
    // 那会儿 ffmpeg 还开着这个文件，Windows 上删除会失败（共享冲突），错误被吞掉，
    // 文件就留下了。走到这里 ffmpeg 已经退出，才删得动。
    let cleanup_logs = |tx: &RunSender| {
        if let Some(log_base) = &plan.log_file_path {
            cleanup_temp_files(tx, log_base);
        }
    };

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
            cleanup_logs(tx);
            emit_cancelled(tx, label);
            return Ok(());
        }
        log(tx, "[PASS 1/2] First pass completed".into());
        current_pass += 1;
    }

    // ---------- PASS 2 / CRF 编码 / 音轨提取 ----------
    let is_crf = plan.pass1.is_empty();
    let pass_description = if plan.audio_only {
        "Audio extraction"
    } else if is_crf {
        "CRF encoding"
    } else {
        "Second pass"
    };
    let display_pass = if is_crf { 1 } else { 2 };
    let display_total = pass_count;

    log(tx, String::new());
    log(tx, format!("[PASS {display_pass}/{display_total}] Starting {pass_description}..."));
    log(tx, format!("[CMD] ffmpeg {}", plan.display_pass2));
    let cancelled = run_process(
        tx, stop, active, current_pid, ffmpeg, &plan.pass2, &plan.output,
        start_time, current_pass, pass_count, total_duration,
    )?;
    if cancelled || stop.load(Ordering::SeqCst) {
        cleanup_logs(tx);
        emit_cancelled(tx, label);
        return Ok(());
    }
    log(tx, format!("[PASS {display_pass}/{display_total}] {pass_description} completed"));

    // 清理临时文件（仅 2pass 模式需要）
    cleanup_logs(tx);

    // 记录完成信息
    let total_elapsed = start_time.elapsed().as_secs_f64();
    log(tx, String::new());
    log(tx, "========================================".into());
    log(tx, format!("[COMPLETE] {label} finished at {}", time::local_timestamp()));
    log(tx, format!("[COMPLETE] Total elapsed time: {}", time::format_hms(total_elapsed)));
    log(tx, "========================================".into());

    if plan.output.exists() {
        let size = std::fs::metadata(&plan.output).map(|m| m.len()).unwrap_or(0);
        log(tx, format!("[OUTPUT] File created: {}", plan.output.display()));
        log(tx, format!("[OUTPUT] Size: {:.2} MB", size as f64 / 1024.0 / 1024.0));
        log(tx, String::new());
        let _ = tx.send(Event::Completed(plan.output.to_string_lossy().into_owned()));
    } else {
        // ffmpeg 退出码是 0 但没有输出文件（被别的程序删了、muxer 写去了别处）。
        // 这里以前照样发 Completed，界面上显示「完成」，实际什么也没产出。
        let msg = format!("Output file not found: {}", plan.output.display());
        log(tx, format!("[ERROR] {msg}"));
        log(tx, String::new());
        return Err(AppError::EncodeFailed(msg));
    }
    Ok(())
}

/// 运行单个 ffmpeg 子进程，返回是否被取消。
fn run_process(
    tx: &RunSender,
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
    let reader = match std::thread::Builder::new()
        .name("stderr-reader".into())
        .spawn(move || {
            read_stderr_and_report(&mut stderr, &tx2, start_time, pass_index, pass_count, total_duration);
        })
    {
        Ok(h) => h,
        Err(e) => {
            // 起不来就没有日志和进度可报，直接收摊；以前这里是 expect，
            // 一 panic 就把 child 连同还在跑的 ffmpeg 一起丢了
            let _ = kill_process_tree(pid);
            child.wait().ok();
            active.store(false, Ordering::SeqCst);
            *current_pid.lock().unwrap() = None;
            return Err(AppError::EncodeFailed(format!(
                "Failed to start stderr reader: {e}"
            )));
        }
    };

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
                // 别把 pid 留在共享格里就走人：kill_now / 下一轮 start 都靠它找进程，
                // 留着一个已经结束的 pid，回头可能杀掉一个复用了这个号的无关进程。
                let _ = kill_process_tree(pid);
                child.wait().ok();
                active.store(false, Ordering::SeqCst);
                *current_pid.lock().unwrap() = None;
                reader.join().ok();
                return Err(AppError::EncodeFailed(format!("Failed to wait ffmpeg: {e}")));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let status = if cancelled {
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
            child.wait().ok()
        } else {
            let _ = kill_process_tree(pid);
            child.wait().ok()
        }
    } else {
        child.wait().ok()
    };

    active.store(false, Ordering::SeqCst);
    *current_pid.lock().unwrap() = None;
    reader.join().ok();

    // 取消不算失败（写 q 让 ffmpeg 退出，退出码本来就不是 0）。其它情况 ffmpeg
    // 非 0 退出说明这一遍没编出来 —— 之前这里不看退出码，失败照样打 [COMPLETE]、
    // 发 Completed，界面上显示「完成」，实际输出可能是个 0 字节文件。
    if !cancelled {
        if let Some(s) = status {
            if !s.success() {
                let code = s
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".into());
                return Err(AppError::EncodeFailed(format!("ffmpeg exited with code {code}")));
            }
        }
    }

    Ok(cancelled)
}

/// 读取 stderr：把 `\r` 和 `\n` 都当作行分隔符（ffmpeg 进度用 `\r` 原地刷新），
/// 逐行发日志并解析进度。
fn read_stderr_and_report(
    stderr: &mut impl std::io::Read,
    tx: &RunSender,
    start_time: Instant,
    pass_index: u32,
    pass_count: u32,
    total_duration: f64,
) {
    let progress_regex = Regex::new(r"time=(\d+):(\d+):(\d+\.\d+)").expect("regex ok");
    // 估剩余要用「这一遍」已经跑了多久，不是整轮跑了多久：2pass 跑到第二遍时
    // `start_time` 里还含着第一遍的时间，拿它算速度会把速度算小一半左右，
    // 剩余时间跟着翻倍。整轮耗时另有用处（显示「已用」），不受影响。
    let pass_start = Instant::now();
    let mut reader = BufReader::new(stderr);
    let mut chunk: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        chunk.clear();
        let mut eof = false;
        // 内层读一行：分隔符（\r 或 \n）被吃掉但不入 chunk
        loop {
            match reader.read(&mut byte) {
                Ok(0) => {
                    eof = true;
                    break;
                }
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
            // 空行（两个分隔符挨着，比如 \r\n）。这里不能顺手多读一个字节再
            // continue：continue 会走回外层的 chunk.clear()，把刚读到的那个
            // 字节连同下一行的首字符一起丢掉。
            if eof {
                break;
            }
            continue;
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
            let pass_elapsed = pass_start.elapsed().as_secs_f64();
            let speed = if pass_elapsed > 0.0 { current / pass_elapsed } else { 0.0 };
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

/// 媒体总时长（秒），失败返回 0。
///
/// 读的是 ffmpeg 横幅里容器声明的那个时长。它只当进度条的分母用，横幅的精度的
/// 百分秒，差 2 ms 无所谓。
fn get_media_duration(tx: &RunSender, ffmpeg: &Path, input_path: &str) -> f64 {
    if input_path.is_empty() {
        return 0.0;
    }

    let banner = probe::read_banner(ffmpeg, input_path);
    if !banner.opened {
        log(tx, "[WARN] Failed to get media duration".into());
    }
    banner.duration
}

/// 这一遍实际要编多长（秒）。ffmpeg 的进度 `time=` 是相对输出时间轴的，
/// 而 `-ss`/`-to` 排在 `-i` 前面，输出从 0 开始、到 (终点 - 起点) 结束，
/// 所以分母要用裁剪后的长度，不能用源视频全长。
fn output_duration(source: f64, start: Option<f64>, end: Option<f64>) -> f64 {
    match (start, end) {
        (Some(s), Some(e)) => (e - s).max(0.0),
        (Some(s), None) => (source - s).max(0.0),
        // 只给终点时从 0 编到终点，源比终点短就按源的长度
        (None, Some(e)) => {
            if source > 0.0 {
                e.min(source)
            } else {
                e
            }
        }
        (None, None) => source,
    }
}

/// ISOBMFF（mp4/mov 这一族）的头部特征：偏移 4 处是 `ftyp`。
/// 只有这一族的 demuxer 会按 edit list 展开视频轨，别的容器不用白跑这次探测。
fn is_isobmff(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 12];
    f.read_exact(&mut head).is_ok() && &head[4..8] == b"ftyp"
}

/// 检查视频轨是不是被 edit list 整段复制过（"战渣浪"那类为躲二压做出来的文件）。
///
/// 这类文件的 moov 只声明一遍时长（比如 174.6 秒），但视频轨的 edit list 把同一段媒体
/// 排了两遍，ffmpeg 默认照着编两遍（349.2 秒）。进度条用的总时长取自声明值，于是跑到
/// 一半就到 100%，之后的 ETA 全是 0，看着像是卡住了。
///
/// 数包那一趟走容器索引不解码，实测 100MB 的 mp4 要 0.29 秒，所以先用 `ftyp`
/// 把非 mp4/mov 挡掉。`probe` 是已经探好的结果，声明时长和帧率都从它取。
pub fn probe_repeated_timeline(
    input_path: &str,
    probe: &probe::Probe,
) -> Option<probe::RepeatedTimeline> {
    if input_path.is_empty() || !is_isobmff(input_path) {
        return None;
    }
    probe::repeated_timeline(&ffmpeg_config::ffmpeg_path(), input_path, probe)
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

/// 删除 2pass 生成的临时日志文件。
///
/// ffmpeg 会在 passlogfile 后面加 `-0`，实际文件名是 `<base>-0.log` 和
/// `<base>-0.log.mbtree`。base 是绝对路径，文件落在应用的当前工作目录，
/// 不在输出目录旁边 —— 照输出目录找永远删不掉（旧程序就是这样，会一直堆文件）。
fn cleanup_temp_files(tx: &RunSender, log_base: &Path) {
    for suffix in PASSLOG_SUFFIXES {
        let file = append_suffix(log_base, suffix);
        if !file.exists() {
            continue;
        }
        match std::fs::remove_file(&file) {
            Ok(()) => {
                let name = file
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| file.to_string_lossy().into_owned());
                log(tx, format!("[Cleanup] Deleted: {name}"));
            }
            Err(e) => {
                log(tx, format!("[CleanupFail] {e}"));
            }
        }
    }
}

/// ffmpeg 给 passlogfile 加的后缀（第二遍写的是 `-0.log.mbtree` 那张统计表）
const PASSLOG_SUFFIXES: [&str; 2] = ["-0.log", "-0.log.mbtree"];

/// 在路径末尾直接接后缀。不能用 `Path::with_extension`：它把 `.` 当成扩展名
/// 分隔符，`log_x` 会被替换成 `log.log` 而不是拼出 `log_x-0.log`。
fn append_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// 清理编码产生的临时文件（供 UI 停止 / 关窗时调用）。
pub fn cleanup_temp_files_public(log_base: &Path) {
    for suffix in PASSLOG_SUFFIXES {
        let _ = std::fs::remove_file(append_suffix(log_base, suffix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::args::{build, build_audio_extract, AudioExtractInput, EncodeInput};

    /// 定位仓库自带的 ffmpeg（仓库根下的 `tools/`）。
    fn tools_dir() -> Option<PathBuf> {
        let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools");
        if tools.join("ffmpeg.exe").exists() {
            Some(tools)
        } else {
            None
        }
    }

    /// 探一遍再查 edit list。生产路径上 `Probe` 来自缓存，测试里现探。
    fn repeated_of(ffmpeg: &Path, path: &Path) -> Option<probe::RepeatedTimeline> {
        let p = probe::probe(
            ffmpeg,
            &path.to_string_lossy(),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        probe_repeated_timeline(&path.to_string_lossy(), &p)
    }

    /// 测试用的发送端。live 恒为 true，事件不会被过滤掉。
    fn test_sender(tx: &Sender<Event>) -> RunSender {
        RunSender {
            tx: tx.clone(),
            live: Arc::new(Mutex::new(true)),
        }
    }

    #[test]
    fn blank_line_does_not_eat_the_next_lines_first_byte() {
        // ffmpeg 的 stderr 里 \r 和 \n 会挨在一起（进度行用 \r 原地刷新）。
        // 以前遇到这种空行会多读一个字节再 continue，而 continue 回到外层就是
        // chunk.clear()，下一行的首字符就没了。
        let (tx, rx) = std::sync::mpsc::channel();
        let sender = test_sender(&tx);
        let mut input: &[u8] = b"alpha\r\nlibx264: ok\r\n\r\n[A]done\n";
        read_stderr_and_report(&mut input, &sender, Instant::now(), 1, 1, 10.0);
        drop(sender);

        let lines: Vec<String> = rx
            .try_iter()
            .filter_map(|e| match e {
                Event::Log(l) => Some(l),
                _ => None,
            })
            .collect();
        assert_eq!(lines, vec!["alpha", "libx264: ok", "[A]done"]);
    }

    #[test]
    fn progress_duration_follows_trim_range() {
        // 不裁：就是源全长
        assert_eq!(output_duration(300.0, None, None), 300.0);
        // 两头都填：终点减起点。之前这里用源全长，
        // 裁 60..90 的进度条最多走到 10% 就不动了
        assert_eq!(output_duration(300.0, Some(60.0), Some(90.0)), 30.0);
        // 只填起点
        assert_eq!(output_duration(300.0, Some(60.0), None), 240.0);
        // 只填终点，源比终点长
        assert_eq!(output_duration(300.0, None, Some(120.0)), 120.0);
        // 只填终点，源比终点短
        assert_eq!(output_duration(90.0, None, Some(120.0)), 90.0);
        // 探不到时长时也别算出负数
        assert_eq!(output_duration(0.0, Some(5.0), None), 0.0);
    }

    /// ffmpeg 给 passlogfile 拼的是 `-0.log`，不是把 `.log` 当扩展名换掉。
    #[test]
    fn passlog_suffix_is_appended_not_replaced() {
        let base = Path::new("C:/app/log_abc123");
        assert_eq!(
            append_suffix(base, "-0.log"),
            PathBuf::from("C:/app/log_abc123-0.log")
        );
    }

    /// 被新一次运行顶掉的旧 worker，事件不能再发出去 —— 否则它收尾时那个
    /// Cancelled 会把新一轮刚设好的「编码中」界面改回「已停止」。
    #[test]
    fn stale_run_events_are_dropped() {
        let (tx, rx) = std::sync::mpsc::channel();
        let live = Arc::new(Mutex::new(true));
        let sender = RunSender {
            tx,
            live: live.clone(),
        };
        sender.send(Event::Log("before".into()));
        *live.lock().unwrap() = false;
        sender.send(Event::Cancelled);

        let got: Vec<Event> = rx.try_iter().collect();
        assert_eq!(got.len(), 1, "被顶掉之后的事件应当全部丢掉");
        assert!(matches!(got[0], Event::Log(_)));
    }

    /// 空方案：只用来验 start 的换代行为。worker 起来后拿不到有效参数会很快失败，
    /// 不碰磁盘。
    fn dummy_plan() -> EncodePlan {
        EncodePlan {
            pass1: Vec::new(),
            pass2: Vec::new(),
            display_pass1: String::new(),
            display_pass2: String::new(),
            output: crate::testutil::scratch_path("dummy_never_created.mp4"),
            audio_only: false,
            log_file_path: None,
            duration_override: None,
            trim_start_secs: None,
            trim_end_secs: None,
        }
    }

    /// 再点一次开始，上一轮要被标记作废并叫停。否则旧的 stop_flag 会被新一轮
    /// 复用，旧 worker 看到它变回 false 就永远停不下来。
    #[test]
    fn starting_again_retires_the_previous_run() {
        let (service, _rx) = FfmpegService::new();
        let prev = Run::new();
        let prev_live = prev.live.clone();
        let prev_stop = prev.stop_flag.clone();
        *service.run.lock().unwrap() = Some(prev);

        service.start(dummy_plan());

        assert!(!*prev_live.lock().unwrap(), "上一轮应当被标记作废");
        assert!(prev_stop.load(Ordering::SeqCst), "上一轮应当被叫停");
        let current = service.run.lock().unwrap();
        let cur = current.as_ref().expect("新一轮应当已经装上");
        assert!(*cur.live.lock().unwrap(), "新一轮应当是活的");
        assert!(
            !cur.stop_flag.load(Ordering::SeqCst),
            "新一轮的停止标志不该受上一轮影响"
        );
    }

    /// 从公共入口发起一个 CRF 方案，等它自己跑完。这条路走的是
    /// `FfmpegService` 的 worker + 事件通道，不是直接调 `run_encoding`。
    fn crf_input(input: &Path, tag: &str) -> EncodeInput {
        let _ = tag;
        EncodeInput {
            input_text: input.to_string_lossy().into_owned(),
            output_text: String::new(),
            format_index: 2, // mp4
            mode_index: 0,
            value_text: "26".into(),
            width_text: "0".into(),
            height_text: "0".into(),
            scale_up: false,
            audio_mode_index: 0,
            audio_codec_index: 0,
            trim_start_secs: None,
            trim_end_secs: None,
            ignore_editlist: false,
            duration_override: None,
        }
    }

    /// 等某个事件出现，超时返回 None；中途报 Failed 直接 panic。
    fn wait_for(
        rx: &Receiver<Event>,
        timeout: Duration,
        want: impl Fn(&Event) -> bool,
    ) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Event::Failed(m)) => panic!("编码失败: {m}"),
                Ok(e) => {
                    if want(&e) {
                        return Some(e);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    }

    #[test]
    fn service_delivers_completion_for_a_real_encode() {
        let Some(tools) = tools_dir() else { return };
        let ffmpeg = tools.join("ffmpeg.exe");
        let input = make_input(&ffmpeg, "svc");

        let (service, rx) = FfmpegService::new();
        let plan = build(crf_input(&input, "svc")).expect("plan should build");
        let output = plan.output.clone();
        service.start(plan);

        let done = wait_for(&rx, Duration::from_secs(120), |e| {
            matches!(e, Event::Completed(_))
        });
        assert!(done.is_some(), "没等到 Completed");
        assert!(output.exists(), "输出文件应当存在");

        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(&input);
    }

    /// 停止之后应当收到 Cancelled，而且不会再来一个 Completed。
    #[test]
    fn service_stop_cancels_a_running_encode() {
        let Some(tools) = tools_dir() else { return };
        let ffmpeg = tools.join("ffmpeg.exe");
        // 60 秒的源，veryslow 下不可能在几百毫秒内编完，来得及点停止
        let input = make_input_full(&ffmpeg, "svcstop", 60, false);

        let (service, rx) = FfmpegService::new();
        let plan = build(crf_input(&input, "svcstop")).expect("plan should build");
        let output = plan.output.clone();
        service.start(plan);

        // 等它真的开始编（拿到第一行日志）再停
        assert!(
            wait_for(&rx, Duration::from_secs(30), |e| matches!(e, Event::Log(_))).is_some(),
            "worker 迟迟没有动静"
        );
        service.stop();

        let cancelled = wait_for(&rx, Duration::from_secs(30), |e| {
            matches!(e, Event::Cancelled)
        });
        assert!(cancelled.is_some(), "停止之后没等到 Cancelled");

        // 停完之后不能再冒出 Completed
        let late = wait_for(&rx, Duration::from_secs(3), |e| {
            matches!(e, Event::Completed(_))
        });
        assert!(late.is_none(), "已经停了却还报完成");

        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(&input);
    }

    /// 提取一律 `-c:a copy`，扩展名跟着源编码走，真的跑一遍确认 ffmpeg 认这个组合
    /// ——容器挑错了它会报 "Could not find tag for codec ... in stream"，然后写出个
    /// 0 字节文件。
    #[test]
    fn audio_extract_copies_every_codec_into_a_matching_container() {
        let Some(tools) = tools_dir() else { return };
        let ffmpeg = tools.join("ffmpeg.exe");

        let make = |codec: &str, tag: &str| -> Option<PathBuf> {
            let input = crate::testutil::scratch_path(&format!(
                "itest_extract_{tag}_{}.mkv",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&input);
            let ok = Command::new(&ffmpeg)
                .args([
                    "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1", "-c:a", codec,
                    "-strict", "-2",
                ])
                .arg(&input)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                Some(input)
            } else {
                None
            }
        };

        // 跑一次真实的提取，返回 (ffmpeg 参数, 产物路径)
        let extract = |input: &Path| -> (Vec<String>, PathBuf) {
            // 生产路径上这个值来自探测缓存（`AudioExtractInput::source_audio_codec`），
            // 测试里现探一次
            let source_audio_codec =
                probe::read_banner(&ffmpeg, &input.to_string_lossy()).audio_codec;
            let plan = build_audio_extract(AudioExtractInput {
                input_text: input.to_string_lossy().into_owned(),
                output_text: String::new(),
                trim_start_secs: None,
                trim_end_secs: None,
                source_audio_codec,
            })
            .expect("plan should build");

            let (tx, _rx) = std::sync::mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let active = Arc::new(AtomicBool::new(false));
            let pid = Arc::new(Mutex::new(None));
            let res = run_process(
                &test_sender(&tx),
                &stop,
                &active,
                &pid,
                &ffmpeg,
                &plan.pass2,
                &plan.output,
                Instant::now(),
                1,
                1,
                0.0,
            );
            assert!(res.is_ok(), "音频提取失败: {:?}", res.as_ref().err());

            (plan.pass2.clone(), plan.output.clone())
        };

        // (编码器, 探测回来的编码名, 提取出来该有的扩展名)
        let cases = [
            ("aac", "aac", "m4a"),
            ("libopus", "opus", "ogg"),
            ("flac", "flac", "flac"),
            ("libmp3lame", "mp3", "mp3"),
            ("ac3", "ac3", "ac3"),
            ("eac3", "eac3", "eac3"),
            ("pcm_s16le", "pcm_s16le", "wav"),
        ];

        for (encoder, tag, ext) in cases {
            // 造不出源就直接挂：静默跳过的话，某一行映射写错了也没人知道
            let input = make(encoder, tag).unwrap_or_else(|| panic!("造不出 {encoder} 源"));

            let (args, output) = extract(&input);

            assert!(
                args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"),
                "{encoder} 源应当直接复制，实际: {args:?}"
            );
            assert_eq!(
                output.extension().unwrap(),
                ext,
                "{encoder} 源的扩展名应当是 {ext}"
            );

            let len = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
            assert!(len > 0, "{encoder} 提取出来的文件不应当是空的");

            // 产物里的编码名还得是源编码，没有偷偷转码
            let got = probe::read_banner(&ffmpeg, &output.to_string_lossy()).audio_codec;
            assert_eq!(got.as_deref(), Some(tag), "{encoder} 源提取后编码变了");

            let _ = std::fs::remove_file(&output);
            let _ = std::fs::remove_file(&input);
        }
    }

    /// 用真实的 ffmpeg 生成指定时长、64×64 的测试输入（libx264 + yuv420p，与编码参数兼容）。
    /// `with_audio` 为真时再混入一条正弦音轨。tag 保证并行测试各自使用独立的文件
    /// （同进程内 PID 相同）。
    fn make_input_full(ffmpeg: &Path, tag: &str, secs: u32, with_audio: bool) -> PathBuf {
        let input = crate::testutil::scratch_path(&format!(
            "itest_{}_{}.mkv",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&input);

        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-y", "-f", "lavfi", "-i"])
            .arg(format!("color=c=red:s=64x64:r=24:d={secs}"));
        if with_audio {
            cmd.args(["-f", "lavfi", "-i"])
                .arg(format!("sine=frequency=440:duration={secs}"));
        }
        cmd.args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p"]);
        if with_audio {
            cmd.args(["-c:a", "aac", "-shortest"]);
        }
        let status = cmd
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .expect("spawn ffmpeg to make test input");
        assert!(status.success(), "failed to generate test input");
        input
    }

    /// 1 秒、无音轨的测试输入（多数端到端测试用这个）
    fn make_input(ffmpeg: &Path, tag: &str) -> PathBuf {
        make_input_full(ffmpeg, tag, 1, false)
    }

    /// 读容器声明的时长（秒），读不到返回 0
    fn probe_duration(ffmpeg: &Path, path: &Path) -> f64 {
        probe::read_banner(ffmpeg, &path.to_string_lossy()).duration
    }

    /// 读输出里音频流的编码名；没有音轨返回 None
    fn probe_stream_audio_codec(ffmpeg: &Path, path: &Path) -> Option<String> {
        probe::read_banner(ffmpeg, &path.to_string_lossy()).audio_codec
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
            audio_mode_index: 0,
            audio_codec_index: 0,
            trim_start_secs: None,
            trim_end_secs: None,
            ignore_editlist: false,
            duration_override: None,
        })
        .expect("plan should build");
        assert_eq!(plan.output.extension().map(|e| e.to_string_lossy().into_owned()), Some("flv".into()));

        let output = plan.output.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));
        let current_pid = Arc::new(Mutex::new(None));

        let res = run_encoding(&test_sender(&tx), &stop, &active, &current_pid, &ffmpeg, &plan);
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
            audio_mode_index: 0,
            audio_codec_index: 0,
            trim_start_secs: None,
            trim_end_secs: None,
            ignore_editlist: false,
            duration_override: None,
        })
        .expect("plan should build");
        assert!(!plan.pass1.is_empty(), "2pass must have a pass1");

        let output = plan.output.clone();
        let log_base = plan
            .log_file_path
            .clone()
            .expect("2pass should carry a passlog path");
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));
        let current_pid = Arc::new(Mutex::new(None));

        let res = run_encoding(&test_sender(&tx), &stop, &active, &current_pid, &ffmpeg, &plan);
        assert!(res.is_ok(), "run_encoding failed: {:?}", res.as_ref().err());

        let events: Vec<Event> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::Completed(_))), "Expected Completed");
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[PASS 1/2]"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[PASS 2/2]"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.contains("-pass 1"))));
        assert!(events.iter().any(|e| matches!(e, Event::Log(m) if m.starts_with("[COMPLETE]"))));

        let meta = std::fs::metadata(&output).expect("output should exist");
        assert!(meta.len() > 0, "output should not be empty");

        // 2pass 跑完应当自己把 `<base>-0.log` 清掉。回归：之前清理去输出目录里找，
        // 而文件落在应用当前目录，永远删不掉。
        // 先确认它确实被创建过，否则下面的「不存在」是假阳性。
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Log(m) if m.starts_with("[Cleanup] Deleted:"))),
            "没看到 [Cleanup] Deleted，说明临时文件压根没生成，这条断言没意义"
        );
        for suffix in PASSLOG_SUFFIXES {
            let leftover = append_suffix(&log_base, suffix);
            assert!(!leftover.exists(), "passlog 没清掉: {}", leftover.display());
        }

        // 清理（输出 + 输入）
        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(&input);
    }

    /// 端到端验证裁剪：2 秒输入裁 0.5–1.5 秒，输出的时长应约为 1 秒。
    /// 这条是 `-ss`/`-to` 放在 `-i` 前面的语义的实测盯守。
    #[test]
    fn end_to_end_trim_with_real_ffmpeg() {
        let Some(tools) = tools_dir() else {
            eprintln!("tools/ffmpeg.exe not found — skipping end-to-end trim test");
            return;
        };
        let ffmpeg = tools.join("ffmpeg.exe");
        let input = make_input_full(&ffmpeg, "trim", 2, false);

        let plan = build(EncodeInput {
            input_text: input.to_string_lossy().into_owned(),
            output_text: String::new(),
            format_index: 2, // mp4
            mode_index: 0,
            value_text: "26".into(),
            width_text: "0".into(),
            height_text: "0".into(),
            scale_up: false,
            audio_mode_index: 0,
            audio_codec_index: 0,
            trim_start_secs: Some(0.5),
            trim_end_secs: Some(1.5),
            ignore_editlist: false,
            duration_override: None,
        })
        .expect("plan should build");

        let output = plan.output.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));
        let current_pid = Arc::new(Mutex::new(None));

        let res = run_encoding(&test_sender(&tx), &stop, &active, &current_pid, &ffmpeg, &plan);
        assert!(res.is_ok(), "run_encoding failed: {:?}", res.as_ref().err());

        let dur = probe_duration(&ffmpeg, &output);
        eprintln!("trimmed output duration = {dur}s (expected ~1.0)");
        assert!(
            (dur - 1.0).abs() <= 0.25,
            "trim produced {dur}s, expected about 1.0s"
        );

        // 进度分母要用裁剪后的 1 秒，不是源视频的 2 秒，否则最多只走到一半
        let mut max_percent = 0.0_f64;
        for e in rx.try_iter() {
            if let Event::Progress { percent, .. } = e {
                max_percent = max_percent.max(percent);
            }
        }
        assert!(
            max_percent >= 90.0,
            "裁剪时进度只走到 {max_percent:.0}%，分母没用裁剪后的长度"
        );

        let _ = std::fs::remove_file(&output);
        let _ = std::fs::remove_file(&input);
    }

    /// 端到端验证音频模式：复制 / 转码 AAC / 转码 FLAC，输出里都应带上音轨。
    #[test]
    fn end_to_end_audio_modes_with_real_ffmpeg() {
        let Some(tools) = tools_dir() else {
            eprintln!("tools/ffmpeg.exe not found — skipping end-to-end audio test");
            return;
        };
        let ffmpeg = tools.join("ffmpeg.exe");
        let input = make_input_full(&ffmpeg, "audiomode", 1, true);

        // (mode_index, codec_index, format_index, 期望的音频编码名)
        let cases = [
            (1, 0, 0, "aac"),  // 复制到 flv（源就是 aac）
            (2, 0, 2, "aac"),  // 转码 AAC 到 mp4
            (2, 1, 2, "flac"), // 转码 FLAC 到 mp4
        ];

        for (mode, codec, format, expected) in cases {
            let plan = build(EncodeInput {
                input_text: input.to_string_lossy().into_owned(),
                output_text: String::new(),
                format_index: format,
                mode_index: 0,
                value_text: "26".into(),
                width_text: "0".into(),
                height_text: "0".into(),
                scale_up: false,
                    audio_mode_index: mode,
                audio_codec_index: codec,
                trim_start_secs: None,
                trim_end_secs: None,
                ignore_editlist: false,
                duration_override: None,
                })
            .expect("plan should build");

            let output = plan.output.clone();
            let (tx, _rx) = std::sync::mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let active = Arc::new(AtomicBool::new(false));
            let current_pid = Arc::new(Mutex::new(None));

            let res = run_encoding(&test_sender(&tx), &stop, &active, &current_pid, &ffmpeg, &plan);
            assert!(res.is_ok(), "run_encoding failed: {:?}", res.as_ref().err());

            let got = probe_stream_audio_codec(&ffmpeg, &output);
            eprintln!("audio mode={mode} codec={codec} format={format} -> {got:?}");
            assert_eq!(
                got.as_deref(),
                Some(expected),
                "unexpected audio codec for mode={mode} codec={codec} format={format}"
            );

            let _ = std::fs::remove_file(&output);
        }

        let _ = std::fs::remove_file(&input);
    }

    // ---------- edit list 重复检测 ----------

    fn box_size(d: &[u8], off: usize) -> u64 {
        u32::from_be_bytes(d[off..off + 4].try_into().unwrap()) as u64
    }

    /// 在 [start, end) 这一层里按 box 顺序找一个子 box，返回它的起始偏移。
    /// 顺序扫描而不是搜字节，免得撞上 mdat 里碰巧出现的同名 4 字节。
    fn find_box_in(d: &[u8], start: usize, end: usize, tag: &[u8; 4]) -> Option<usize> {
        let mut i = start;
        while i + 8 <= end {
            let size = box_size(d, i) as usize;
            if size < 8 || i + size > end {
                return None;
            }
            if &d[i + 4..i + 8] == tag {
                return Some(i);
            }
            i += size;
        }
        None
    }

    /// 把 ffmpeg 生成的 mp4 改造成「视频轨被 edit list 复制过」的样子：
    /// 把已有的单项 elst 换成两项（都从 media_time 0 开始、各占整段媒体），
    /// 再把 edts / trak / moov 的 box 大小补上多出来的 12 字节。
    ///
    /// 只能改已有 elst 不能新插一个——ffmpeg 的 mov demuxer 对同一个 trak 只认最后
    /// 一个 edts，新插的那个会被后面原有的覆盖掉。
    fn duplicate_video_track_via_editlist(path: &Path) {
        let mut d = std::fs::read(path).expect("read generated mp4");

        let moov = find_box_in(&d, 0, d.len(), b"moov").expect("moov");
        let moov_end = moov + box_size(&d, moov) as usize;
        let trak = find_box_in(&d, moov + 8, moov_end, b"trak").expect("trak");
        let trak_end = trak + box_size(&d, trak) as usize;
        let edts = find_box_in(&d, trak + 8, trak_end, b"edts").expect("edts");
        let elst = edts + 8;
        let elst_size = box_size(&d, elst) as usize;
        let mdia = find_box_in(&d, trak + 8, trak_end, b"mdia").expect("mdia");
        let mdia_end = mdia + box_size(&d, mdia) as usize;
        let mdhd = find_box_in(&d, mdia + 8, mdia_end, b"mdhd").expect("mdhd");
        let version = d[mdhd + 8];
        let duration = if version == 0 {
            u32::from_be_bytes(d[mdhd + 20..mdhd + 24].try_into().unwrap()) as u64
        } else {
            u64::from_be_bytes(d[mdhd + 28..mdhd + 36].try_into().unwrap())
        };
        let (edts_size, trak_size, moov_size) = (
            box_size(&d, edts),
            box_size(&d, trak),
            box_size(&d, moov),
        );

        let mut new_elst = Vec::new();
        new_elst.extend_from_slice(&40u32.to_be_bytes());
        new_elst.extend_from_slice(b"elst");
        new_elst.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags
        new_elst.extend_from_slice(&2u32.to_be_bytes());
        for _ in 0..2 {
            new_elst.extend_from_slice(&(duration as u32).to_be_bytes()); // segment_duration
            new_elst.extend_from_slice(&0i32.to_be_bytes()); // media_time
            new_elst.extend_from_slice(&1i16.to_be_bytes()); // media_rate = 1.0
            new_elst.extend_from_slice(&0i16.to_be_bytes());
        }

        let grow = new_elst.len() - elst_size;
        d.splice(elst..elst + elst_size, new_elst);
        // 只改了 moov 里面的字节，stco 记的是 mdat 的绝对偏移，不受影响
        for (off, old) in [
            (edts, edts_size),
            (trak, trak_size),
            (moov, moov_size),
        ] {
            d[off..off + 4].copy_from_slice(&((old + grow as u64) as u32).to_be_bytes());
        }

        std::fs::write(path, &d).expect("write patched mp4");
    }

    /// 用真实 ffmpeg：给一段 2 秒 / 30fps 的 mp4 装上复制整段的 edit list，
    /// 探测应当报出「声明 2 秒、实际 4 秒」。
    #[test]
    fn detects_editlist_duplicated_video_track() {
        let Some(tools) = tools_dir() else {
            eprintln!("tools/ffmpeg.exe not found — skipping edit list detection test");
            return;
        };
        let ffmpeg = tools.join("ffmpeg.exe");
        let path = crate::testutil::scratch_path(&format!(
            "editlist_{}.mp4",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let status = Command::new(&ffmpeg)
            .args(["-y", "-f", "lavfi", "-i", "testsrc=size=64x64:rate=30", "-t", "2"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-an"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "failed to generate the mp4 fixture");

        // 改造之前就是普通文件，不该被误报
        assert!(
            repeated_of(&ffmpeg, &path).is_none(),
            "a plain mp4 must not be reported as repeated"
        );

        duplicate_video_track_via_editlist(&path);

        let got = repeated_of(&ffmpeg, &path)
            .expect("the duplicated edit list should be detected");
        assert!((got.declared - 2.0).abs() < 0.1, "declared = {}", got.declared);
        assert!((got.actual - 4.0).abs() < 0.1, "actual = {}", got.actual);

        let _ = std::fs::remove_file(&path);
    }

    /// 非 mp4 容器（mkv）直接返回 None，连数包那一趟都不用跑
    #[test]
    fn repeated_timeline_skips_non_isobmff() {
        let Some(tools) = tools_dir() else {
            eprintln!("tools/ffmpeg.exe not found — skipping non-isobmff test");
            return;
        };
        let ffmpeg = tools.join("ffmpeg.exe");
        let path = crate::testutil::scratch_path(&format!(
            "editlist_neg_{}.mkv",
            std::process::id()
        ));
        std::fs::write(&path, b"\x1a\x45\xdf\xa3not an mp4 at all").unwrap();
        assert!(repeated_of(&ffmpeg, &path).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
