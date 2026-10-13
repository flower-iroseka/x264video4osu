//! 应用控制器：连接 Slint UI 回调与编码服务。
//!
//! 对应旧程序 `MainWindow.xaml.cs` 的事件处理器 + 与 `FfmpegService` 的交互。
//! 保持行为等价：校验顺序、提示文案、[CONFIG] 日志、进度/结果文本、保存日志、
//! 停止与关窗清理等均照搬旧程序。

use crate::error::AppError;
use crate::i18n::{self, Lang, Strings};
use crate::platform::time;
use crate::services::args::{build, build_audio_extract, crf_from_text, AudioExtractInput, EncodeInput};
use crate::services::ffmpeg::{cleanup_temp_files_public, Event, FfmpegService};
use crate::services::format::OutputFormat;
use crate::services::frame_cache::FrameCache;
use crate::services::pathutil::{full_path, validate_path};
use crate::services::preview::{self, Preview};
use crate::services::probe::{self, Framing, Probe, RepeatedTimeline};
use crate::services::probe_service::{ProbeEvent, ProbeService};
use crate::services::settings::{self, AppSettings};
use crate::services::trim;
use slint::{ComponentHandle, SharedString, Timer, TimerMode, VecModel};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// ---------- 项目链接（对应旧程序 AppConfig；GitHubIssuesUrl 在旧程序中未被使用） ----------
const REPO_URL: &str = "https://github.com/flower-iroseka/x264video4osu";

/// 探测超过这个时长还没结束就弹进度框。
///
/// 实测探测有约 156 ms 的进程启动底价，之后每 MB 约 1.6 ms——典型的 osu! 背景
/// 视频（几 MB 到几十 MB）两三百毫秒就完事，走不到这一步。真要看框的是几百 MB
/// 到几 GB 的长视频。
const PROBE_DIALOG_DELAY: Duration = Duration::from_millis(400);

/// `probe::probe` 里拆包建帧表那一段从 36% 起。进度到这个值，说明一级筛子已经
/// 判定可疑 VFR，正在做唯一需要用户等的事——这时也弹框。
const PROBE_TABLE_STAGE_PERCENT: u8 = probe::VFR_TABLE_STAGE_PERCENT;

/// 探测的总超时。卡住的 ffmpeg 不能把主窗口永久锁着。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// 查探测结果的三种下场。
enum ProbeLookup {
    Ready(std::sync::Arc<Probe>),
    /// 已经请了后台探测，等结果
    Pending,
    /// 探不了（文件不存在、打不开），别等了
    Unavailable,
}

/// 用户在等探测结果时点下的那个操作。探测落地后按它接着往下走。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    /// 没有等着的操作
    None,
    StartEncode,
    ExtractAudio,
}

/// 一次后台探测的进行状态。
struct ProbeJob {
    path: String,
    started: Instant,
    /// 探测进度 0..=100
    percent: u8,
    /// 结果到了之后要接着往下走的那个操作（用户点它的时候探测还没好，被拦下了）
    pending: PendingAction,
    /// 已弹出的进度框。None = 还没弹（探测够快的话根本不会弹）
    dialog: Option<ProbeDialog>,
}

/// 弹出来的进度框。持有 `ModalState` 不放 —— 一放手主窗口就恢复可交互了。
struct ProbeDialog {
    dialog: crate::ProbingDialog,
    modal: Rc<crate::win_dialog::ModalState>,
}

impl ProbeJob {
    fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            started: Instant::now(),
            percent: 0,
            pending: PendingAction::None,
            dialog: None,
        }
    }

    /// 该不该弹框了。两种时候弹：等太久了，或者一级筛子判出可疑 VFR、
    /// 正在拆包建帧表。
    fn wants_dialog(&self) -> bool {
        self.started.elapsed() >= PROBE_DIALOG_DELAY
            || self.percent >= PROBE_TABLE_STAGE_PERCENT
    }
}

/// 用户对「视频轨被 edit list 复制」的处理选择。
/// 连输入路径一起存，换了文件就重新问一次。
struct RepeatDecision {
    input: String,
    ignore_editlist: bool,
}

/// 进度行三格文字要用到的数（百分比单独一格，不在这里）。
#[derive(Clone, Copy)]
struct ProgressSnapshot {
    pass_index: u32,
    pass_count: u32,
    remaining_secs: f64,
    elapsed_secs: f64,
}

pub struct AppController {
    ui: crate::MainWindow,
    service: FfmpegService,
    rx: Receiver<Event>,
    strings: Strings,
    log_buffer: String,
    /// 逐行日志模型（日志页 ListView 显示用；与 log_buffer 同内容，仅按换行拆分）
    log_lines_model: Rc<VecModel<SharedString>>,
    last_output_path: Option<std::path::PathBuf>,
    /// 上次编码产物的字节数。结果行那串文字是编码结束时算好写进去的，
    /// 之后切语言不会自己重算，所以把字节数留着，切语言时重新拼一遍。
    last_output_bytes: Option<u64>,
    /// 上一次进度事件的那几个数。进度行是常驻显示的，编码结束后不再来事件，
    /// 那串「第 x/y 遍 / 剩余 / 已用」就停在结束时的语言里，切语言时按这份值重拼。
    last_progress: Option<ProgressSnapshot>,
    /// 2pass 临时日志的绝对路径基名（ffmpeg 自己加的 `-0` 后缀不含在内）。
    /// 停止或关窗时按它清理 —— 文件落在应用当前目录，不是输出目录旁边。
    current_log_path: Option<std::path::PathBuf>,
    /// 0 = CRF，1 = 2pass（与旧程序 `TwoPassRadio.IsChecked` 对应）
    mode_index: i32,
    /// 裁剪单位：0 = 按帧，1 = 按时间（与 Slint 侧单选框对应）
    trim_unit_index: i32,
    /// 裁剪输入框里那两段文本目前是按哪个单位填的（0 = 帧，1 = 时间）。
    /// `None` = 没有欠账，文本和 `trim_unit_index` 一致。
    ///
    /// 光记一个 bool（"欠一次换算"）不够：`convert_trim_texts` 只能「从另一个单位
    /// 换到当前单位」，单位来回切偶数次之后文本已经回到当前单位了，欠着的那次换算
    /// 却还在，补算时方向就反了 —— 帧号会被当成时间戳，120 帧变 120 秒（3600 帧）。
    /// 记下文本自己的单位，方向就不会错。
    trim_text_unit: Option<i32>,
    /// 探测结果缓存：同一个文件在这一次运行里只探一次（预览、切裁剪单位、
    /// 点开始编码都要用）。只放内存，不落盘。
    probe_cache: FrameCache,
    /// 后台探测服务。探测要起子进程（约 270 ms 起），放在 UI 线程上会冻界面。
    probe_service: ProbeService,
    probe_rx: Receiver<ProbeEvent>,
    /// 正在跑的探测；None = 没有
    probe_job: Option<ProbeJob>,
    /// 探过但打不开的文件。记下来免得每次问都重探一遍（重探会失败得一样快）。
    probe_failed: Option<String>,
    /// 上一次看到的输入文件（绝对路径）。识别"用户换文件了"用。
    /// 换文件要把 `probe_failed` 清掉：探测超时只是一次性的，留着的会让这个文件
    /// 在本次运行里永远按不了帧裁（每次都报「帧率未知」）。
    last_input_seen: String,
    /// 当前输入文件的探测结果，是 `probe_cache` 里那一份的快捷引用。
    /// 还没探到、或者文件探测失败时为 None。
    last_probe: Option<std::sync::Arc<Probe>>,
    /// 上次解析出的裁剪区间（秒），仅用于 [CONFIG] 日志
    last_trim: (Option<f64>, Option<f64>),
    /// 视频轨被 edit list 复制过时用户的选择（见 `RepeatDecision`）
    repeat_decision: Option<RepeatDecision>,
    /// 上一次编码对 edit list 的处理说明，只用于 [CONFIG] 日志
    last_editlist_note: Option<String>,
    /// 自身的弱引用：对话框的回调里要重入 `handle_start_encode`
    self_weak: Weak<RefCell<AppController>>,
    /// 持久化设置（语言）；语言切换时同步回写
    settings: AppSettings,
    /// 轮询编码事件的中断器（必须持有，否则会停止触发）
    poll_timer: Timer,
    /// 自动滚动去抖标志
    scroll_pending: Rc<Cell<bool>>,
    /// 预览会话。None = 没有 libmpv / 还没起 / 起不来，此时预览入口整体禁用。
    preview: Option<Preview>,
    /// mpv 落脚用的子窗口句柄。0 表示还没建。
    preview_host: isize,
    /// 预览最近一次打开的输入文件，用来判断要不要重新 load
    preview_opened: Option<String>,
    /// 当前预览文件的帧号换算方式。恒定帧率线性算，可变帧率查表。
    /// `Framing::Unknown` = 探不出来，此时帧号相关的控件禁用。
    /// 和按帧裁剪用的是同一次探测结果，两边帧号必须一致。
    preview_framing: Framing,
    /// 正在把换算结果写回另一个输入框。程序写 text 理论上不会触发 edited，
    /// 但两个框互相写，一旦触发就是死循环，加个闸保险。
    preview_syncing: bool,
}

impl AppController {
    /// 返回主窗口句柄（供 main 运行事件循环）。
    /// 生成的 Slint 组件未实现 `Clone`，通过 `ComponentHandle::clone_strong` 复制强句柄。
    pub fn window_handle(&self) -> crate::MainWindow {
        ComponentHandle::clone_strong(&self.ui)
    }

    /// 创建控制器并完成所有 UI 回调接线。
    pub fn create() -> Rc<RefCell<Self>> {
        let ui = crate::MainWindow::new().expect("failed to create main window");
        let (service, rx) = FfmpegService::new();
        let (probe_service, probe_rx) = ProbeService::new();

        // 读取持久化设置：首次运行（无设置文件）回落默认值（中文）
        let settings = settings::load();
        let lang = if settings.lang_index == 0 { Lang::Zh } else { Lang::En };

        let controller = Rc::new(RefCell::new(Self {
            ui,
            service,
            rx,
            strings: i18n::for_lang(lang),
            log_buffer: String::new(),
            log_lines_model: Rc::new(VecModel::default()),
            last_output_path: None,
            last_output_bytes: None,
            last_progress: None,
            current_log_path: None,
            mode_index: 0,
            trim_unit_index: 0,
            trim_text_unit: None,
            probe_cache: FrameCache::default(),
            probe_service,
            probe_rx,
            probe_job: None,
            probe_failed: None,
            last_input_seen: String::new(),
            last_probe: None,
            last_trim: (None, None),
            repeat_decision: None,
            last_editlist_note: None,
            self_weak: Weak::new(),
            settings,
            poll_timer: Timer::default(),
            scroll_pending: Rc::new(Cell::new(false)),
            preview: None,
            preview_host: 0,
            preview_opened: None,
            preview_framing: Framing::Unknown,
            preview_syncing: false,
        }));
        // 弱引用要等 Rc 建好才能取，所以放在构造之后补上
        controller.borrow_mut().self_weak = Rc::downgrade(&controller);

        AppController::initialize(&controller);
        controller
    }

    /// 初始化：设置默认语言文本，接线回调，启动轮询定时器，注册拖放与关窗。
    fn initialize(self_rc: &Rc<RefCell<Self>>) {
        let weak = Rc::downgrade(self_rc);

        let rc = self_rc.borrow();
        let ui = &rc.ui;

        // 应用保存的语言（首次运行回落默认：中文）。语言框索引通过 <=> 双向绑定，
        // 这里只同步界面文本。
        ui.set_language_index(rc.settings.lang_index);
        ui.set_strings(rc.strings.ui.clone());
        // 预览按钮先藏起来，等确认 libmpv 可用、子窗口建出来了再放出来，
        // 免得闪一下又消失。
        ui.set_preview_available(false);

        // ---- 按钮回调 ----
        let w = weak.clone();
        ui.on_browse_input(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_browse_input();
            }
        });
        let w = weak.clone();
        ui.on_browse_output(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_browse_output();
            }
        });
        let w = weak.clone();
        ui.on_start_encode(move || {
            if let Some(c) = w.upgrade() {
                let mut c = c.borrow_mut();
                c.forget_probe_failure();
                c.handle_start_encode();
            }
        });
        let w = weak.clone();
        ui.on_stop_encode(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_stop_encode();
            }
        });
        let w = weak.clone();
        ui.on_extract_audio(move || {
            if let Some(c) = w.upgrade() {
                let mut c = c.borrow_mut();
                c.forget_probe_failure();
                c.handle_extract_audio();
            }
        });
        let w = weak.clone();
        ui.on_open_folder(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_open_folder();
            }
        });
        let w = weak.clone();
        ui.on_save_log(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_save_log();
            }
        });
        let w = weak.clone();
        ui.on_show_about(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_show_about();
            }
        });

        // ---- 语言切换 ----
        let w = weak.clone();
        ui.on_language_changed(move |index: i32| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_language_changed(index);
            }
        });

        // ---- 输出格式切换：FLAC 只在 mp4 下可选，切走后把编码索引夹回 AAC ----
        let w = weak.clone();
        ui.on_format_changed(move |index: i32| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_format_changed(index);
            }
        });

        // ---- 编码方式切换（RadioGroup 的 selected 回调） ----
        let w = weak.clone();
        ui.on_mode_changed(move |text: slint::SharedString| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_mode_changed(text.to_string());
            }
        });

        // ---- 裁剪单位切换（传的是本地化显示文本，与当前语言比对） ----
        let w = weak.clone();
        ui.on_trim_unit_changed(move |text: slint::SharedString| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_trim_unit_changed(text.to_string());
            }
        });

        // ---- 文件拖放 ----
        let w = weak.clone();
        let window = ui.window();
        crate::platform::dragdrop::handle_drop(&window, move |path: String| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_dropped_file(path);
            }
        });

        // ---- 预览面板 ----
        let w = weak.clone();
        ui.on_preview_toggle_expanded(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().toggle_preview_expanded();
            }
        });
        let w = weak.clone();
        ui.on_preview_toggle_play(move || {
            if let Some(c) = w.upgrade() {
                let c = c.borrow();
                if let Some(p) = &c.preview {
                    p.toggle_paused();
                }
            }
        });
        let w = weak.clone();
        ui.on_preview_step(move |n: i32| {
            if let Some(c) = w.upgrade() {
                if let Some(p) = &c.borrow().preview {
                    p.step_frames(n as i64);
                }
            }
        });
        let w = weak.clone();
        ui.on_preview_seek(move |ratio: f32| {
            if let Some(c) = w.upgrade() {
                let c = c.borrow();
                if let Some(p) = &c.preview {
                    let dur = p.state().duration;
                    if dur > 0.0 {
                        p.seek((ratio as f64 * dur).clamp(0.0, dur));
                    }
                }
            }
        });
        let w = weak.clone();
        ui.on_preview_seek_by(move |secs: f32| {
            if let Some(c) = w.upgrade() {
                if let Some(p) = &c.borrow().preview {
                    p.seek_by(secs as f64);
                }
            }
        });
        let w = weak.clone();
        ui.on_preview_set_volume(move |volume: f32| {
            if let Some(c) = w.upgrade() {
                if let Some(p) = &c.borrow().preview {
                    p.set_volume(volume as f64);
                }
            }
        });
        let w = weak.clone();
        ui.on_preview_volume_step(move |delta: f32| {
            if let Some(c) = w.upgrade() {
                if let Some(p) = &c.borrow().preview {
                    p.step_volume(delta as f64);
                }
            }
        });
        let w = weak.clone();
        ui.on_preview_toggle_mute(move || {
            if let Some(c) = w.upgrade() {
                let c = c.borrow();
                if let Some(p) = &c.preview {
                    p.toggle_mute();
                }
            }
        });
        let w = weak.clone();
        ui.on_preview_goto_start(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().preview_goto(true);
            }
        });
        let w = weak.clone();
        ui.on_preview_goto_end(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().preview_goto(false);
            }
        });
        let w = weak.clone();
        ui.on_preview_frame_edited(move |text: SharedString| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().preview_frame_edited(&text);
            }
        });
        let w = weak.clone();
        ui.on_preview_time_edited(move |text: SharedString| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().preview_time_edited(&text);
            }
        });
        let w = weak.clone();
        ui.on_preview_jump_to_input(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().preview_jump_to_input();
            }
        });
        let w = weak.clone();
        ui.on_preview_set_trim_start(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().preview_set_trim(true);
            }
        });
        let w = weak.clone();
        ui.on_preview_set_trim_end(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().preview_set_trim(false);
            }
        });

        // ---- 关窗清理 ----
        let w = weak.clone();
        let window = ui.window();
        window.on_close_requested(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().cleanup_for_close();
            }
            slint::CloseRequestResponse::HideWindow
        });

        drop(rc);

        // ---- 轮询编码事件 ----
        // 窗口固定尺寸（用户不可缩放、无最大化），无需再同步最大化状态。
        // 每轮同时校正一次窗口物理尺寸：系统缩放变化后 winit 后端只更新渲染比例、
        // 不重设窗口物理尺寸（event_loop.rs 的 ScaleFactorChanged 忽略了
        // inner_size_writer），窗口会比渲染画布小，底部按钮栏被裁掉。尺寸不匹配时
        // 重设为 逻辑尺寸 × 缩放因子；只在真的不一致时调用 set_size，开销可以忽略。
        let w = weak.clone();
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            if let Some(c) = w.upgrade() {
                let mut c = c.borrow_mut();
                c.drain_events();
                c.drain_probe_events();
                apply_window_size(&c.ui);
                c.sync_preview();
            }
        });
        self_rc.borrow_mut().poll_timer = timer;

        // ---- 启动后一次性设置 ----
        // 原生标题栏不会自动采用 exe 资源图标：启动后把嵌入的应用图标设为窗口图标。
        let w = Rc::downgrade(self_rc);
        slint::Timer::single_shot(Duration::from_millis(100), move || {
            if let Some(c) = w.upgrade() {
                let c = c.borrow();
                let hwnd = crate::win_icon::main_window_hwnd(c.ui.window());
                crate::win_icon::apply_window_icon(hwnd);
            }
        });

        // ---- 预览 ----
        // 比图标再晚一点：那会儿窗口一定建出来了，子窗口才有地方挂。
        Self::setup_preview(self_rc);
    }

    // =============================================================
    // 事件处理
    // =============================================================

    fn handle_browse_input(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            self.ui.set_input_path(path.to_string_lossy().into_owned().into());
        }
    }

    fn handle_browse_output(&mut self) {
        let format = OutputFormat::from_index(self.ui.get_format_index());
        let ext = format.extension().trim_start_matches('.');
        let dialog = rfd::FileDialog::new()
            .add_filter(&self.strings.dialog_filter_video, &["mp4", "flv", "avi"]);
        if let Some(mut path) = dialog.save_file() {
            // 对应旧程序 SaveFileDialog.DefaultExt：用户未输入扩展名时补全
            if path.extension().is_none() {
                path.set_extension(ext);
            }
            self.ui.set_output_path(path.to_string_lossy().into_owned().into());
        }
    }

    /// 输入的公共校验（顺序与文案照搬旧程序 `Start_Click`）。
    /// 过了返回规范化后的绝对路径，没过就弹提示并返回 None。
    fn validated_input_path(&mut self) -> Option<String> {
        let input_text = self.ui.get_input_path().to_string();
        let input_path = input_text.trim().to_string();
        if input_path.is_empty() {
            self.show_warning(
                self.strings.title_validation.clone(),
                self.strings.msg_empty_input.clone(),
            );
            return None;
        }
        if !validate_path(&input_path) {
            self.show_warning(
                self.strings.title_validation.clone(),
                self.strings.msg_invalid_input.clone(),
            );
            return None;
        }
        let full = full_path(&input_path).unwrap_or_default();
        if !Path::new(&full).exists() {
            self.show_error(
                self.strings.title_error.clone(),
                self.strings.msg_input_not_found.clone(),
            );
            return None;
        }
        Some(full.to_string_lossy().into_owned())
    }

    /// 探测还没好：记下用户点的是哪个操作，结果到了由 `resume_pending_action` 接着走。
    fn defer_until_probed(&mut self, path: &str, action: PendingAction) {
        if let Some(j) = &mut self.probe_job {
            if j.path == path {
                j.pending = action;
            }
        }
    }

    fn handle_start_encode(&mut self) {
        let output_text = self.ui.get_output_path().to_string();

        // ---- 校验（顺序与文案照搬旧程序 Start_Click） ----
        let Some(full_str) = self.validated_input_path() else {
            return;
        };
        let input_text = full_str.clone();

        // 探测要起子进程（约 270 ms 起），不能在 UI 线程上干等。没有结果就先请一次，
        // 结果到了 `drain_probe_events` 会把这个方法原样重跑一遍 ——
        // 和 `show_repeat_dialog` 那套重入是一个路子。
        // edit list 那种「编一遍还是两遍」要等探测出来才知道，所以放在探测之后。
        if let ProbeLookup::Pending = self.lookup_probe(&full_str) {
            self.defer_until_probed(&full_str, PendingAction::StartEncode);
            return;
        }

        // ---- 视频轨被 edit list 复制过？（"战渣浪"那类为躲二压做出来的文件） ----
        // 这类文件的容器只声明一遍时长，ffmpeg 却按 edit list 编两遍。进度条用的总时长
        // 取自声明值，跑到一半就到 100%，看着像是不结束。这里先问用户要编几遍，
        // 同一个文件问过一次就不再问。
        // edit list 只可能出现在 mp4/mov 上，数包那一趟由 `probe_repeated_timeline`
        // 内部用 `ftyp` 挡掉别的容器。
        let probe = self.last_probe.clone();
        let repeated = probe
            .as_ref()
            .and_then(|p| crate::services::ffmpeg::probe_repeated_timeline(&full_str, p));
        let decided = self
            .repeat_decision
            .as_ref()
            .filter(|d| d.input == full_str)
            .map(|d| d.ignore_editlist);
        let ignore_editlist = match (&repeated, decided) {
            (Some(info), None) => {
                self.show_repeat_dialog(&full_str, info);
                return;
            }
            (Some(_), Some(choice)) => choice,
            (None, _) => false,
        };
        // 选择编两遍时，时间轴是 edit list 展开后的长度：裁剪的上限和进度总数都按它算。
        let timeline_secs = match (&repeated, ignore_editlist) {
            (Some(info), false) => Some(info.actual),
            _ => None,
        };
        self.last_editlist_note = repeated.map(|info| {
            if ignore_editlist {
                format!("[CONFIG] Edit list: ignored (encoding once, {:.1}s)", info.declared)
            } else {
                format!("[CONFIG] Edit list: kept (encoding twice, {:.1}s)", info.actual)
            }
        });

        // ---- 裁剪区间（勾选且填了起止才生效；按帧要先探源帧率） ----
        let (trim_start_secs, trim_end_secs) = match self.resolve_trim(timeline_secs) {
            Ok(v) => v,
            Err(msg) => {
                let title = self.strings.title_validation.clone();
                self.show_warning(title, msg);
                return;
            }
        };

        // 覆盖探测到的总时长：视频轨被 edit list 复制时，容器声明的只是
        // 其中一遍，展开后的时间轴才是真实长度。裁剪掉的那一段不在这里减 ——
        // run_encoding 会按 plan 里的 trim 区间统一折算，减两次会把分母算错。
        let duration_override = timeline_secs;

        let input = EncodeInput {
            input_text,
            output_text,
            format_index: self.ui.get_format_index(),
            mode_index: self.mode_index,
            value_text: self.ui.get_value_text().to_string(),
            width_text: self.ui.get_width_text().to_string(),
            height_text: self.ui.get_height_text().to_string(),
            scale_up: self.ui.get_scale_up(),
            audio_mode_index: self.ui.get_audio_mode_index(),
            audio_codec_index: self.ui.get_audio_codec_index(),
            trim_start_secs,
            trim_end_secs,
            ignore_editlist,
            duration_override,
        };

        self.set_encoding(true);

        match build(input) {
            Ok(plan) => {
                self.last_output_path = Some(plan.output.clone());
                self.current_log_path = plan.log_file_path.clone();

                self.ui.set_progress_visible(true);
                self.ui.set_result_visible(false);
                self.ui.set_progress_value(0.0);

                // 新编码开始即清空日志：log_buffer / 行模型只增不清，若不清空，
                // 同进程内多次编码会无限累积（内存随会话一直涨，且"保存日志"
                // 存到的会是历次编码拼在一起的全量文本）。
                self.reset_log();
                self.log_config_lines();
                self.service.start(plan);
            }
            Err(e) => {
                self.set_encoding(false);
                self.show_build_error(&e);
            }
        }
    }

    /// 「提取音轨」按钮：把音频流原样复制出来，不重编码、不编视频。跑完不弹文件夹，
    /// 要用结果行里的「打开输出文件夹」自己点过去（`last_output_path` 已经指到产物上）。
    fn handle_extract_audio(&mut self) {
        let output_text = self.ui.get_output_path().to_string();

        let Some(full_str) = self.validated_input_path() else {
            return;
        };

        // 提取要用的两个信息都在探测结果里：源音频编码决定导出文件的扩展名，按帧裁剪
        // 要用帧率。所以和「开始」一样，探测没好就先等它。
        if let ProbeLookup::Pending = self.lookup_probe(&full_str) {
            self.defer_until_probed(&full_str, PendingAction::ExtractAudio);
            return;
        }

        // 探测成功但这个文件根本没音轨：直说，别等 ffmpeg 报 "does not contain any
        // stream" 那种看不懂的话。探测没成功（`last_probe` 为 None）时不拦，
        // 交给 ffmpeg 去报。
        if self.last_probe.as_ref().is_some_and(|p| p.audio_codec.is_none()) {
            let title = self.strings.title_validation.clone();
            let msg = self.strings.msg_no_audio_track.clone();
            self.show_warning(title, msg);
            return;
        }

        // 勾了「裁剪视频」就跟着裁，导出的音频和裁过的视频是同一段。
        // 传 None：edit list 展开那套只影响视频轨，音轨没有 edit list。
        let (trim_start_secs, trim_end_secs) = match self.resolve_trim(None) {
            Ok(v) => v,
            Err(msg) => {
                let title = self.strings.title_validation.clone();
                self.show_warning(title, msg);
                return;
            }
        };

        let input = AudioExtractInput {
            input_text: full_str,
            output_text,
            trim_start_secs,
            trim_end_secs,
            source_audio_codec: self.last_probe.as_ref().and_then(|p| p.audio_codec.clone()),
        };

        self.set_encoding(true);

        match build_audio_extract(input) {
            Ok(plan) => {
                self.last_output_path = Some(plan.output.clone());
                self.current_log_path = plan.log_file_path.clone();

                self.ui.set_progress_visible(true);
                self.ui.set_result_visible(false);
                self.ui.set_progress_value(0.0);

                self.reset_log();
                self.log_extract_config_lines();
                self.service.start(plan);
            }
            Err(e) => {
                self.set_encoding(false);
                self.show_build_error(&e);
            }
        }
    }

    /// 方案构建失败时的提示。参数不合法算用户填错了，其余当启动失败。
    /// 冒号后面是底层原文，不翻译。
    fn show_build_error(&mut self, e: &AppError) {
        match e {
            AppError::InvalidArgument(detail) => {
                let msg = self.strings.msg_invalid_parameter.replace("{detail}", detail);
                let title = self.strings.title_validation.clone();
                self.show_warning(title, msg);
            }
            _ => {
                let msg = self.strings.msg_encode_failed.replace("{detail}", &e.to_string());
                let title = self.strings.title_error.clone();
                self.show_error(title, msg);
            }
        }
    }

    /// 把界面上的裁剪起止解析成秒。未启用、或起止都空 → (None, None) 表示不裁剪。
    /// 按帧输入时用源视频帧率换算（第 0 帧 = 源第一帧）；超出总时长会 clamp。
    ///
    /// `duration_override` 是「编两遍」时 edit list 展开后的长度，用它当 clamp 的上限；
    /// 传 None 就按探测器报的容器声明时长算。
    /// 探测结果由调用方（`handle_start_encode`）保证就绪——它在本方法之前
    /// 已经用 `lookup_probe` 确认过，没就绪就先返回了。
    fn resolve_trim(&mut self, duration_override: Option<f64>) -> Result<(Option<f64>, Option<f64>), String> {
        self.last_trim = (None, None);
        if !self.ui.get_trim_enabled() {
            return Ok((None, None));
        }
        let start_text = self.ui.get_trim_start_text().to_string();
        let end_text = self.ui.get_trim_end_text().to_string();
        let start_text = start_text.trim().to_string();
        let end_text = end_text.trim().to_string();
        if start_text.is_empty() && end_text.is_empty() {
            return Ok((None, None));
        }

        let by_frame = self.trim_unit_index == 0;
        // 探测结果由 `handle_start_encode` 在调这里之前保证就绪（没就绪它会先返回）。
        // `last_probe` 是当前文件的那一份。
        let framing = self
            .last_probe
            .as_ref()
            .map(|p| p.framing.clone())
            .unwrap_or(Framing::Unknown);
        let nb_frames = self.last_probe.as_ref().map(|p| p.nb_frames).unwrap_or(0);
        let duration = duration_override
            .or_else(|| self.last_probe.as_ref().map(|p| p.duration))
            .unwrap_or(0.0);

        if by_frame && !framing.is_valid() {
            return Err(self.strings.msg_no_fps.clone());
        }

        let mut secs = [None, None];
        for (i, text) in [start_text.as_str(), end_text.as_str()].iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            let v = if by_frame {
                let bad_frame = || self.strings.msg_invalid_frame.replace("{text}", text);
                let frame: i64 = text.parse().map_err(|_| bad_frame())?;
                if frame < 0 {
                    return Err(bad_frame());
                }
                let frame = if nb_frames > 0 { frame.min(nb_frames) } else { frame };
                // 帧号超过 VFR 帧表长度时 `frame_to_secs` 查不到、返回 0，
                // 裁剪区间会静默变成「从 0 开始」。夹到最后一帧：填一个越过末尾的
                // 号，本意就是到视频末尾。
                framing.frame_to_secs(framing.clamp_frame(frame))
            } else {
                crate::services::trim::parse_timecode(text).ok_or_else(|| {
                    self.strings.msg_invalid_time.replace("{text}", text)
                })?
            };
            secs[i] = Some(if duration > 0.0 { v.min(duration) } else { v });
        }

        if let (Some(s), Some(e)) = (secs[0], secs[1]) {
            if e <= s {
                return Err(self.strings.msg_trim_order.clone());
            }
        }
        self.last_trim = (secs[0], secs[1]);
        Ok((secs[0], secs[1]))
    }

    fn handle_stop_encode(&mut self) {
        // 旧程序先恢复按钮状态，再停止
        self.set_encoding(false);
        self.service.stop();
        if let Some(log) = &self.current_log_path {
            cleanup_temp_files_public(log);
        }
    }

    /// 「打开输出文件夹」：打开并选中产物本身，不是只把文件夹打开。
    fn handle_open_folder(&mut self) {
        if let Some(out) = &self.last_output_path {
            let _ = crate::io::external::open_folder_selecting(out);
        }
    }

    fn handle_save_log(&mut self) {
        if let Some(out) = &self.last_output_path {
            let dir = match out.parent() {
                Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
                _ => return,
            };
            if !dir.exists() {
                return;
            }
            let stamp = time::local_compact();
            let rand = random_in_range(1000, 9999);
            let name = format!("log_{stamp}_{rand}.txt");
            let _ = std::fs::write(dir.join(&name), self.log_buffer.clone());
        }
        // 旧程序在无输出路径时会因 NullReferenceException 崩溃 —— 此处保护为空操作
    }

    fn handle_show_about(&mut self) {
        let dialog = crate::AboutDialog::new().expect("failed to create about dialog");
        dialog.set_dialog_title(self.strings.about_title.clone().into());
        dialog.set_version_line(self.strings.about_version_line.clone().into());
        dialog.set_description(self.strings.about_description.clone().into());
        dialog.set_repository_text(self.strings.repository_text.clone().into());
        dialog.set_close_text(self.strings.close_text.clone().into());

        let url_repo = REPO_URL.to_string();
        dialog.on_open_repository(move || {
            let _ = crate::io::external::open_url(&url_repo);
        });

        // 显示、挂到主窗口下、居中、打开期间禁用主窗口都在 open 里；系统关闭路径
        // （ALT+F4 / 任务栏关闭）也归它管，这里只需要在「关闭」按钮上 dismiss
        let modal = crate::win_dialog::open(
            &dialog,
            self.main_hwnd(),
            (dialog.get_dialog_w(), dialog.get_dialog_h()),
        );

        // 「关闭」按钮
        let weak = dialog.as_weak();
        dialog.on_close_dialog(move || {
            modal.dismiss();
            if let Some(d) = weak.upgrade() {
                let _ = d.window().hide();
            }
        });
    }

    /// 主窗口的 HWND。取不到（窗口还没建）时返回空指针，`win_dialog::open` 会退化
    /// 成只在屏幕居中、不建立 owner 关系。
    fn main_hwnd(&self) -> windows_sys::Win32::Foundation::HWND {
        crate::win_icon::main_window_hwnd(self.ui.window())
    }

    fn handle_language_changed(&mut self, index: i32) {
        let lang = if index == 0 { Lang::Zh } else { Lang::En };
        self.strings = i18n::for_lang(lang);
        self.ui.set_strings(self.strings.ui.clone());
        // 结果行是编码结束时算好写进去的，语言换了得按新语言重拼一遍
        if let Some(bytes) = self.last_output_bytes {
            self.ui.set_output_size_text(self.result_size_text(bytes));
        }
        // 进度行常驻显示，编完之后不再来事件，也停在旧语言里，同样要重拼
        if let Some(p) = self.last_progress {
            self.apply_progress_texts(p);
        }
        // 中英文的窗口宽度和标签列宽度不同，切语言后按新尺寸重设窗口
        apply_window_size(&self.ui);
        // 持久化语言选择（语言框索引已通过 <=> 同步到 UI，这里只需保存）
        self.settings.lang_index = index;
        settings::save(&self.settings);
    }

    fn handle_format_changed(&mut self, index: i32) {
        // FLAC 只在 mp4 下可选（Slint 侧的 codec-model 按 format-index 计算）。
        // 从 mp4 切到别的容器时，编码索引可能还停在 FLAC，夹回 AAC。
        if index != 2 && self.ui.get_audio_codec_index() != 0 {
            self.ui.set_audio_codec_index(0);
        }
    }

    fn handle_trim_unit_changed(&mut self, text: String) {
        // Slint 侧 RadioButton 的 text 既是显示文本又是回传值，没有独立的值字段，
        // 所以这里拿当前语言的「时间」文案比对；不等于它就按「帧」处理。
        let new_index = if text == self.strings.ui.trim_time.as_str() { 1 } else { 0 };
        // 点已经选中的那一项时 selected 也会来一次，不挡住会把值换算两遍变回去
        if new_index == self.trim_unit_index {
            return;
        }
        // 文本没被换算过，单位也就没变：已经欠着一次换算的话，文本的单位就是欠的那次
        // 记下的单位；不欠的话，文本的单位就是切换前的当前单位。
        // 不能直接取切换前的当前单位 —— 来回切两次之后文本其实回到了原单位，而那次
        // 欠着的换算还没补，取错方向会把帧号当成时间戳（120 帧变 120 秒）。
        self.trim_text_unit = Some(self.trim_text_unit.unwrap_or(self.trim_unit_index));
        self.trim_unit_index = new_index;
        self.convert_trim_texts();
    }

    /// 按当前的裁剪单位把两个输入框的文本换算一遍。
    ///
    /// 换不了就**原样留着**（空串、格式不对、帧率还没探到），和按帧编码时会报
    /// `msg_no_fps` 的口径一致。只在 `trim_text_unit` 记着"文本是另一个单位"时才
    /// 真的换 —— 帧率没探到的时候那次换算没做成，标记留着，由 `apply_probe` 补。
    fn convert_trim_texts(&mut self) {
        let Some(from) = self.trim_text_unit else {
            return;
        };
        // 单位又切回来了，文本本来就是当前单位，不用换
        if from == self.trim_unit_index {
            self.trim_text_unit = None;
            return;
        }
        let start = self.ui.get_trim_start_text().to_string();
        let end = self.ui.get_trim_end_text().to_string();
        // 两个框都空着就没什么可换的，也别为它白跑一次探测
        if start.trim().is_empty() && end.trim().is_empty() {
            self.trim_text_unit = None;
            return;
        }
        let framing = self.current_framing();
        if !framing.is_valid() {
            // 帧率还没探到，留着等 apply_probe 再补
            return;
        }
        let to_frames = self.trim_unit_index == 0;
        self.ui
            .set_trim_start_text(trim::convert_unit_text(&start, to_frames, &framing).into());
        self.ui
            .set_trim_end_text(trim::convert_unit_text(&end, to_frames, &framing).into());
        self.trim_text_unit = None;
    }

    // =============================================================
    // 视频预览
    // =============================================================

    /// 等窗口真的建出来，再建子窗口、起 mpv。失败就把预览入口整体藏掉。
    fn setup_preview(self_rc: &Rc<RefCell<Self>>) {
        let w = Rc::downgrade(self_rc);
        slint::Timer::single_shot(Duration::from_millis(250), move || {
            let Some(c) = w.upgrade() else { return };
            let mut c = c.borrow_mut();

            if !preview::is_available() {
                // 没有 libmpv-2.dll。预览是可选功能，不影响其它部分。
                c.ui.set_preview_available(false);
                return;
            }

            let main = crate::win_icon::main_window_hwnd(c.ui.window());
            let host = if main.is_null() {
                std::ptr::null_mut()
            } else {
                crate::platform::preview_window::create(main)
            };
            if host.is_null() {
                c.ui.set_preview_available(false);
                return;
            }

            match Preview::new(host) {
                Ok(p) => {
                    c.preview_host = host as isize;
                    c.preview = Some(p);
                    // 预览一律从收起开始，不记上次的状态：窗口默认保持原来的窄宽度，
                    // 要用预览再点侧边的展开按钮。
                    c.ui.set_preview_expanded(false);
                    c.ui.set_preview_available(true);
                    // 让预览快捷键一开始就是活的：Slint 只往有焦点的元素送按键，
                    // 没元素有焦点时按键直接丢。
                    c.ui.invoke_focus_preview_keys();
                    apply_window_size(&c.ui);
                    c.reload_preview_source();
                }
                Err(e) => {
                    // 起不来就彻底关掉入口，别让用户点开一个空面板
                    eprintln!("preview unavailable: {e}");
                    crate::platform::preview_window::destroy(host);
                    c.ui.set_preview_available(false);
                }
            }
        });
    }

    /// 展开/收起预览。窗口宽度跟着变，立刻应用一次免得等 100ms 的轮询。
    /// 展开状态不写进设置：下次启动还是收起的。
    fn toggle_preview_expanded(&mut self) {
        let expanded = !self.ui.get_preview_expanded();
        self.ui.set_preview_expanded(expanded);
        if expanded {
            // 展开时把键盘焦点收到预览这一层：刚才可能在输入框里，不收的话
            // 空格会被打成空格，快捷键一个都进不来。
            self.ui.invoke_focus_preview_keys();
        }
        apply_window_size(&self.ui);
        self.sync_preview();
    }

    /// 把预览的当前状态搬到界面上，并顺手摆好 mpv 的子窗口。
    /// 由 100ms 的轮询定时器调用。
    fn sync_preview(&mut self) {
        if self.preview.is_none() {
            return;
        }
        // 输入框里换了文件就跟着换（内部会先比对，没变就直接返回）
        self.reload_preview_source();

        let Some(p) = &self.preview else { return };
        let st = p.state();

        // 画面区的位置随窗口尺寸/DPI/展开状态变，每轮都摆一次。SetWindowPos 很便宜。
        let host = self.preview_host as windows_sys::Win32::Foundation::HWND;
        if !host.is_null() {
            let expanded = self.ui.get_preview_expanded();
            if !expanded {
                crate::platform::preview_window::set_visible(host, false);
            } else {
                let sf = self.ui.window().scale_factor() as f32;
                let vx = self.ui.get_video_x() as f32;
                let vy = self.ui.get_video_y() as f32;
                let vw = self.ui.get_video_w() as f32;
                let vh = self.ui.get_video_h() as f32;
                // Slint 画那块矩形时，边界铺满它跨到的整格设备像素（左边 floor、
                // 右边 ceil）。子窗口得用同一套算法，不然 125% 缩放下两边差出半格，
                // 黑底占位矩形会在画面左边露出一条黑边（右边不会，方向反着）。
                let x = (vx * sf).floor() as i32;
                let y = (vy * sf).floor() as i32;
                let w = ((vx + vw) * sf).ceil() as i32 - x;
                let h = ((vy + vh) * sf).ceil() as i32 - y;
                crate::platform::preview_window::place(host, x, y, w, h);
                crate::platform::preview_window::set_visible(host, true);
            }
        }

        self.ui.set_preview_loaded(st.loaded);
        self.ui.set_preview_paused(st.paused);
        self.ui
            .set_preview_time_text(trim::format_timecode(st.time_pos).into());
        self.ui
            .set_preview_duration_text(trim::format_timecode(st.duration).into());
        // 帧号要和按帧裁剪用同一套算法，都走 Framing 上的换算
        if st.loaded && self.preview_framing.is_valid() {
            self.ui.set_preview_frame_cur(
                self.preview_framing
                    .secs_to_frame(st.time_pos)
                    .to_string()
                    .into(),
            );
            self.ui.set_preview_frame_total(
                self.preview_framing
                    .secs_to_frame(st.duration)
                    .to_string()
                    .into(),
            );
        } else {
            self.ui.set_preview_frame_cur("--".into());
            self.ui.set_preview_frame_total("--".into());
        }
        // 用户正在拖进度条时别覆盖他手上的值
        if !self.ui.get_preview_scrubbing() {
            self.ui.set_preview_seek_1000((st.progress() * 1000.0) as f32);
        }
        // 音量同理：拖动期间不回写，否则拖到一半会被上一轮的旧值拽回去
        if !self.ui.get_preview_vol_dragging() {
            self.ui.set_preview_volume(st.volume as f32);
        }
        self.ui.set_preview_muted(st.mute);
    }

    /// 输入文件变了就把新文件塞给 mpv。没变则什么都不做。
    fn reload_preview_source(&mut self) {
        let Some(p) = &self.preview else { return };
        let path = self.ui.get_input_path().to_string();
        if path.trim().is_empty() || self.preview_opened.as_deref() == Some(path.as_str()) {
            return;
        }
        if !Path::new(&path).exists() {
            return;
        }
        p.set_paused(true);
        p.open(Path::new(&path));
        self.preview_opened = Some(path);
        // 帧号换算要帧率。和按帧裁剪走同一次探测，别另起一套，
        // 否则「当前帧为开头」填进去的帧号和编码时算的对不上。
        self.preview_framing = self.current_framing();
        self.ui.set_preview_fps_known(self.preview_framing.is_valid());
        // 预览面板按真实宽高比摆画面。探测结果里带着宽高，不用再探一遍；
        // 探不到（比如没有视频轨）就按 16:9 摆。
        let aspect = match &self.last_probe {
            Some(info) if info.width > 0 && info.height > 0 => {
                info.width as f32 / info.height as f32
            }
            _ => 16.0 / 9.0,
        };
        self.ui.set_video_aspect(aspect);
        // 换文件了，输入框回到起点
        self.set_preview_inputs(0.0);
    }

    /// 把帧号框和时间戳框一起设成指定的秒数，中间不会触发互相换算。
    fn set_preview_inputs(&mut self, secs: f64) {
        let secs = secs.max(0.0);
        self.preview_syncing = true;
        self.ui
            .set_preview_time_input(trim::format_timecode(secs).into());
        self.ui.set_preview_frame_input(
            if self.preview_framing.is_valid() {
                self.preview_framing.secs_to_frame(secs).to_string()
            } else {
                "0".to_string()
            }
            .into(),
        );
        self.preview_syncing = false;
    }

    /// 用户改了帧号框：把对应的时间戳算出来填进另一个框。
    fn preview_frame_edited(&mut self, text: &str) {
        if self.preview_syncing || !self.preview_framing.is_valid() {
            return;
        }
        // 打字过程中会经过 "1"、"12" 这种半截状态，解析不了就什么都不做，
        // 别弹框也别把另一个框清空
        let Ok(frame) = text.trim().parse::<i64>() else {
            return;
        };
        if frame < 0 {
            return;
        }
        // 越界的帧号夹到最后一帧再换算，否则 VFR 上会查不到、时间框变成 0
        let secs = self.preview_framing.frame_to_secs(self.preview_framing.clamp_frame(frame));
        self.preview_syncing = true;
        self.ui
            .set_preview_time_input(trim::format_timecode(secs).into());
        self.preview_syncing = false;
    }

    /// 用户改了时间戳框：把对应的帧号算出来填进另一个框。
    fn preview_time_edited(&mut self, text: &str) {
        if self.preview_syncing {
            return;
        }
        let Some(secs) = trim::parse_timecode(text) else {
            return;
        };
        if !self.preview_framing.is_valid() {
            return;
        }
        let frame = self.preview_framing.secs_to_frame(secs);
        self.preview_syncing = true;
        self.ui
            .set_preview_frame_input(frame.to_string().into());
        self.preview_syncing = false;
    }

    /// 跳到输入框指定的位置。时间戳优先，读不出来再用帧号。
    fn preview_jump_to_input(&mut self) {
        let Some(p) = &self.preview else { return };
        let time_text = self.ui.get_preview_time_input().to_string();
        let frame_text = self.ui.get_preview_frame_input().to_string();
        let framing = self.preview_framing.clone();
        let secs = trim::parse_timecode(&time_text).or_else(|| {
            frame_text
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|_| framing.is_valid())
                .map(|f| framing.frame_to_secs(framing.clamp_frame(f)))
        });
        let Some(secs) = secs else { return };
        let dur = p.state().duration;
        let target = if dur > 0.0 { secs.clamp(0.0, dur) } else { secs.max(0.0) };
        p.set_paused(true);
        p.seek(target);
    }

    /// 跳到视频开头 / 结尾。
    fn preview_goto(&mut self, to_start: bool) {
        let Some(p) = &self.preview else { return };
        let target = if to_start {
            0.0
        } else {
            (p.state().duration - 1.0 / self.preview_framing.fps_f64().max(1.0)).max(0.0)
        };
        p.set_paused(true);
        p.seek(target);
    }

    /// 把预览的当前位置写进裁剪的起点或终点输入框。
    fn preview_set_trim(&mut self, is_start: bool) {
        let Some(p) = &self.preview else { return };
        let secs = p.state().time_pos;
        let text = if self.trim_unit_index == 0 {
            // 按帧：要帧率，拿不到就不动，免得填一个错的帧号进去
            if !self.preview_framing.is_valid() {
                return;
            }
            self.preview_framing.secs_to_frame(secs).to_string()
        } else {
            trim::format_timecode(secs)
        };
        if is_start {
            self.ui.set_trim_start_text(text.into());
        } else {
            self.ui.set_trim_end_text(text.into());
        }
        // 填了值就把裁剪打开，否则填了也不生效
        self.ui.set_trim_enabled(true);
        self.ui.set_preview_scrubbing(false);
    }

    /// 取某个输入文件的探测结果。命中缓存就直接返回，否则现场探一次再存起来。
    ///
    /// 同一个文件在一次运行里会被问好几次（开预览、切裁剪单位、点开始编码），
    /// 缓存就是挡这些重复的子进程的。文件打不开时返回 None，`last_probe` 一并清掉。
    /// 当前输入框里的文件（绝对路径）。探测缓存的键统一用它，
    /// 免得同一个文件因为写法不同（相对/绝对）出现两个条目、白探一次。
    fn current_input_path(&self) -> String {
        let raw = self.ui.get_input_path().to_string();
        if raw.trim().is_empty() {
            return String::new();
        }
        full_path(raw.trim())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// 用户又点了一次「开始」或「提取音轨」，把上一轮「这个文件探不动」的记号清掉。
    ///
    /// 超时那种失败（探测慢、杀毒扫描、冷启动）是一过性的：不清的话，只要输入框里
    /// 一直是这个文件，`drain_probe_events` 里那条「换文件才清」的逻辑就永远不触发，
    /// 这个文件在本次运行里再也不会被探测，按帧裁剪一直报「读不出帧率」。
    ///
    /// 只挂在按钮的回调上、不放进 `handle_start_encode` 里面：探测失败后
    /// `resume_pending_action` 会把那个方法重跑一遍，清在这里会变成
    /// 「重探 → 失败 → 重探」的死循环。
    fn forget_probe_failure(&mut self) {
        self.probe_failed = None;
    }

    /// 查某个文件的探测结果。没有就顺手请一次后台探测——**不在 UI 线程上等**，
    /// 结果到了由 `drain_probe_events` 接手。
    fn lookup_probe(&mut self, path: &str) -> ProbeLookup {
        if path.is_empty() {
            self.last_probe = None;
            return ProbeLookup::Unavailable;
        }
        // 探过就知道打不开，别重探一遍（还会失败，只是白等）
        if self.probe_failed.as_deref() == Some(path) {
            self.last_probe = None;
            return ProbeLookup::Unavailable;
        }
        if let Some(hit) = self.probe_cache.get(Path::new(path)) {
            self.last_probe = Some(hit.clone());
            return ProbeLookup::Ready(hit);
        }

        self.probe_service.request(path);
        if !self.probe_job.as_ref().is_some_and(|j| j.path == path) {
            self.probe_job = Some(ProbeJob::new(path));
        }
        ProbeLookup::Pending
    }

    /// 当前输入文件的帧号换算方式。还没探好就是 `Framing::Unknown`——
    /// 探测完 `apply_probe` 会把结果补上。
    fn current_framing(&mut self) -> Framing {
        let path = self.current_input_path();
        match self.lookup_probe(&path) {
            ProbeLookup::Ready(p) => p.framing.clone(),
            _ => Framing::Unknown,
        }
    }

    /// 收后台探测的事件：更新进度、到点弹框、收结果。
    fn drain_probe_events(&mut self) {
        // 用户换文件了就把上一份「探测失败」清掉。超时那种失败是一过性的（探测慢、
        // 冷启动、杀毒扫描），不清的话这个文件在本次运行里再也不会被探测，按帧裁剪
        // 会一直报「帧率未知」。同一个文件一直选着则不清，免得对着坏文件反复重探。
        let path = self.current_input_path();
        if path != self.last_input_seen {
            self.last_input_seen = path;
            self.probe_failed = None;
        }

        while let Ok(event) = self.probe_rx.try_recv() {
            match event {
                ProbeEvent::Progress { path, percent } => {
                    if let Some(j) = &mut self.probe_job {
                        if j.path == path {
                            j.percent = percent;
                        }
                    }
                }
                ProbeEvent::Done { path, probe } => {
                    self.probe_service.finish(&path);
                    self.probe_cache.insert(Path::new(&path), probe.clone());
                    let is_current = self.current_input_path() == path;
                    if is_current {
                        self.last_probe = Some(probe.clone());
                        self.apply_probe(&probe);
                    }
                    self.resume_pending_action(&path, is_current);
                }
                ProbeEvent::Failed { path } => {
                    self.probe_service.finish(&path);
                    self.probe_failed = Some(path.clone());
                    if self.current_input_path() == path {
                        self.last_probe = None;
                    }
                    // 探不了也要把等着的那个操作放行，让它走老路去报错
                    self.resume_pending_action(&path, self.current_input_path() == path);
                }
            }
        }

        // 超时兜底：卡住的 ffmpeg 不能把主窗口永久锁着
        let timed_out = self
            .probe_job
            .as_ref()
            .filter(|j| j.started.elapsed() >= PROBE_TIMEOUT)
            .map(|j| j.path.clone());
        if let Some(path) = timed_out {
            self.probe_service.cancel();
            self.probe_failed = Some(path.clone());
            self.last_probe = None;
            // 和 Done/Failed 一样要把等着的操作放行，否则用户点了按钮、等 20 秒、
            // 进度框自己消失，然后什么也没发生
            self.resume_pending_action(&path, self.current_input_path() == path);
        }

        self.update_probe_dialog();
    }

    /// 探测跑完了（成没成都算），把用户之前点下的那个操作接着走一遍。
    fn resume_pending_action(&mut self, path: &str, is_current: bool) {
        let pending = self.take_probe_job(path);
        if !is_current {
            return;
        }
        match pending {
            PendingAction::StartEncode => self.handle_start_encode(),
            PendingAction::ExtractAudio => self.handle_extract_audio(),
            PendingAction::None => {}
        }
    }

    /// 探测跑完了，收走这一格。返回等着要继续的那个操作。
    fn take_probe_job(&mut self, path: &str) -> PendingAction {
        let Some(job) = self.probe_job.as_ref() else {
            return PendingAction::None;
        };
        if job.path != path {
            return PendingAction::None;
        }
        let pending = job.pending;
        // 放手会让 ModalState::drop 恢复主窗口；窗口也顺手藏掉
        if let Some(d) = self.probe_job.take().and_then(|j| j.dialog) {
            let _ = d.dialog.window().hide();
            d.modal.dismiss();
        }
        pending
    }

    /// 按规则决定要不要弹进度框，以及框里的进度走到哪了。
    fn update_probe_dialog(&mut self) {
        // 编码途中不弹：这个框是 Blocking 模态，`open_blocking` 会把主窗口
        // `EnableWindow(0)`，连「停止」一起点不动，用户只能干等探测结束。
        // 编码中改输入框（打字、浏览、拖放）都会走到这儿，代价太大，宁可不显示进度。
        if self.ui.get_encoding() {
            return;
        }
        // 先算完再动，别让 `probe_job` 的可变借用跨过 `self.main_hwnd()`
        let Some(needs_dialog) = self
            .probe_job
            .as_ref()
            .map(|j| j.dialog.is_none() && j.wants_dialog())
        else {
            return;
        };

        if needs_dialog {
            let dialog = crate::ProbingDialog::new().expect("failed to create probing dialog");
            dialog.set_dialog_title(self.strings.probing_title.clone().into());
            dialog.set_message(self.strings.probing_message.clone().into());
            dialog.set_progress(0.0);
            // 用户关不掉：探测亚秒级就结束，给「取消」要引入「编码流程被打断」
            // 的状态机，不值当
            let hwnd = self.main_hwnd();
            let modal = crate::win_dialog::open_blocking(
                &dialog,
                hwnd,
                (dialog.get_dialog_w(), dialog.get_dialog_h()),
            );
            if let Some(job) = &mut self.probe_job {
                job.dialog = Some(ProbeDialog { dialog, modal });
            }
        }

        if let Some(job) = &self.probe_job {
            if let Some(d) = &job.dialog {
                d.dialog
                    .set_progress((job.percent as f32 / 100.0).clamp(0.0, 1.0));
            }
        }
    }

    /// 探测结果到了之后，把它用到等它的地方：预览的帧号换算与画面比例、
    /// 裁剪输入框的单位换算。
    fn apply_probe(&mut self, probe: &Probe) {
        self.preview_framing = probe.framing.clone();
        self.ui.set_preview_fps_known(probe.framing.is_valid());

        if self.preview_opened.is_some() {
            let aspect = if probe.width > 0 && probe.height > 0 {
                probe.width as f32 / probe.height as f32
            } else {
                16.0 / 9.0
            };
            self.ui.set_video_aspect(aspect);
        }

        // 探测期间切过裁剪单位的话，那次换算被打回去了，这里补一次。
        // 只在真的欠着一次换算的时候调：`convert_trim_texts` 是「从另一个单位换到
        // 当前单位」，无条件调会把用户手填的帧号当成时间戳再换一遍。
        if self.trim_text_unit.is_some() && self.ui.get_trim_enabled() {
            self.convert_trim_texts();
        }
    }

    fn handle_mode_changed(&mut self, text: String) {
        // 与旧程序 EncodeMode_Changed 一致：切换编码方式时设置对应默认值
        if text == "2pass" {
            self.mode_index = 1;
            self.ui.set_value_text("800".into());
        } else {
            self.mode_index = 0;
            self.ui.set_value_text("26".into());
        }
    }

    fn handle_dropped_file(&mut self, path: String) {
        // 对应旧程序 Input_Drop：只接受第一个文件，验证合法性
        if !path.is_empty() && validate_path(&path) && Path::new(&path).exists() {
            self.ui.set_input_path(path.into());
        } else {
            self.show_error(
                self.strings.title_error.clone(),
                self.strings.msg_invalid_drop.clone(),
            );
        }
    }

    /// 窗口关闭：停止编码并清理临时文件（对应 MainWindow_Closing -> CleanupAll）
    fn cleanup_for_close(&mut self) {
        self.service.stop();
        self.service.kill_now();
        if let Some(log) = &self.current_log_path {
            cleanup_temp_files_public(log);
        }
        // 先丢预览（Drop 里销毁 mpv，mpv 会放开它引用的那个窗口），
        // 再销毁子窗口。顺序反了的话 mpv 会去操作一个已经不存在的句柄。
        self.preview = None;
        let host = self.preview_host as windows_sys::Win32::Foundation::HWND;
        crate::platform::preview_window::destroy(host);
        self.preview_host = 0;
    }

    // =============================================================
    // 编码事件分发（由轮询定时器调用）
    // =============================================================

    fn drain_events(&mut self) {
        let mut any_log = false;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Log(msg) => {
                    self.append_log(&msg);
                    any_log = true;
                }
                Event::Progress {
                    pass_index,
                    pass_count,
                    percent,
                    elapsed_secs,
                    remaining_secs,
                } => {
                    let snapshot = ProgressSnapshot {
                        pass_index,
                        pass_count,
                        remaining_secs,
                        elapsed_secs,
                    };
                    self.last_progress = Some(snapshot);
                    self.apply_progress_texts(snapshot);
                    self.ui.set_percent_text(format!("{percent:.0}%").into());
                    self.ui.set_progress_value((percent / 100.0) as f32);
                }
                Event::Completed(output_path) => {
                    self.handle_completed(&output_path);
                }
                Event::Cancelled => {
                    self.set_encoding(false);
                }
                Event::Failed(msg) => {
                    self.set_encoding(false);
                    // msg 是 ffmpeg 自己的输出，只能原样带出去
                    let text = self.strings.msg_encode_failed.replace("{detail}", &msg);
                    let title = self.strings.title_error.clone();
                    self.show_error(title, text);
                }
            }
        }
        if any_log && self.ui.get_auto_scroll() {
            self.schedule_scroll();
        }
    }

    fn handle_completed(&mut self, output_path: &str) {
        self.set_encoding(false);
        self.ui.set_progress_visible(false);
        self.ui.set_result_visible(true);
        if Path::new(output_path).exists() {
            let bytes = std::fs::metadata(output_path)
                .map(|m| m.len())
                .unwrap_or(0);
            self.last_output_bytes = Some(bytes);
            self.ui.set_output_size_text(self.result_size_text(bytes));
        }
        // 跑完不自己弹文件夹。结果行里的「打开输出文件夹」已经能定位到这个文件
        // （last_output_path 上面刚更新过），要不要跳过去让用户自己点。
    }

    /// 结果行里的「输出大小」文案，按当前语言拼。
    fn result_size_text(&self, bytes: u64) -> SharedString {
        self.strings
            .result_output_size
            .replace("{size}", &format!("{:.2}", bytes as f64 / 1024.0 / 1024.0))
            .into()
    }

    /// 进度行三格的文字（第 x/y 遍、剩余、已用），按当前语言拼。
    fn progress_texts(
        &self,
        p: ProgressSnapshot,
    ) -> (SharedString, SharedString, SharedString) {
        let pass = self
            .strings
            .progress_pass
            .replace("{pass}", &p.pass_index.to_string())
            .replace("{total}", &p.pass_count.to_string());
        let eta = self
            .strings
            .progress_eta
            .replace("{time}", &time::format_mmss(p.remaining_secs));
        let elapsed = self
            .strings
            .progress_elapsed
            .replace("{time}", &time::format_mmss(p.elapsed_secs));
        (pass.into(), eta.into(), elapsed.into())
    }

    fn apply_progress_texts(&self, p: ProgressSnapshot) {
        let (pass, eta, elapsed) = self.progress_texts(p);
        self.ui.set_pass_text(pass);
        self.ui.set_eta_text(eta);
        self.ui.set_elapsed_text(elapsed);
    }

    // =============================================================
    // 日志与配置记录
    // =============================================================

    /// 清空日志缓冲与逐行模型（仅在新编码开始前调用；校验失败不清，保留上一次日志）。
    /// VecModel::clear 会广播 row_removed，日志页 ListView 同步清空。
    fn reset_log(&mut self) {
        self.log_buffer.clear();
        self.log_lines_model.clear();
    }

    fn append_log(&mut self, msg: &str) {
        self.log_buffer.push_str(msg);
        self.log_buffer.push('\n');
        // 逐行拆入 ListView 模型：空行用单个空格占位，保证空行仍有行高（与 log_buffer 换行一致）
        for line in msg.split('\n') {
            self.log_lines_model
                .push(if line.is_empty() { " ".into() } else { line.into() });
        }
        self.ui.set_log_lines(self.log_lines_model.clone().into());
        if self.ui.get_auto_scroll() {
            self.schedule_scroll();
        }
    }

    /// [CONFIG] 参数记录（对应旧程序 LogEncodingParameters，逻辑独立复算）
    fn log_config_lines(&mut self) {
        // 从 UI 当前值重新解析（与旧程序 BuildFfmpegArgs / LogEncodingParameters 各自解析一致）
        let value_box = self.ui.get_value_text().trim().parse::<i32>().unwrap_or(0);
        let width = self.ui.get_width_text().trim().parse::<i32>().unwrap_or(0);
        let height = self.ui.get_height_text().trim().parse::<i32>().unwrap_or(0);
        let is_two_pass = self.mode_index == 1;
        let bitrate = if is_two_pass && value_box > 0 { value_box } else { 800 };
        // 和 build() 走同一条解析规则，免得日志里记的 CRF 跟实际传给 ffmpeg 的不一样
        let crf = crf_from_text(&self.ui.get_value_text());

        let fmt_name = OutputFormat::from_index(self.ui.get_format_index())
            .extension()
            .trim_start_matches('.')
            .to_string();

        self.append_log(&format!(
            "[CONFIG] Encoding mode: {}",
            if is_two_pass { "2-Pass (VBR)" } else { "CRF (Constant Rate Factor)" }
        ));
        self.append_log(&format!("[CONFIG] Output format: {fmt_name}"));
        if is_two_pass {
            self.append_log(&format!("[CONFIG] Target bitrate: {bitrate} kbps"));
        } else {
            self.append_log(&format!("[CONFIG] CRF value: {crf}"));
        }
        self.append_log(&format!(
            "[CONFIG] Resolution: {} x {}",
            if width > 0 { width.to_string() } else { "original".into() },
            if height > 0 { height.to_string() } else { "original".into() },
        ));
        self.append_log("[CONFIG] FPS: 24");
        self.append_log("[CONFIG] Preset: veryslow");
        self.append_log("[CONFIG] Profile: high, Level: 5.2");

        // 音频模式：0 = 无音频，1 = 复制音频流，2 = 转码（编码器按容器兜底，与 args.rs 一致）
        let audio_mode = self.ui.get_audio_mode_index();
        let audio_desc = match audio_mode {
            1 => "copy".to_string(),
            2 => {
                let codec = crate::services::audio::resolve_codec(
                    self.ui.get_audio_codec_index(),
                    OutputFormat::from_index(self.ui.get_format_index()),
                );
                format!("transcode ({})", codec.log_name())
            }
            _ => "none".to_string(),
        };
        self.append_log(&format!("[CONFIG] Audio mode: {audio_desc}"));

        self.log_trim_line();

        // 视频轨被 edit list 复制过时，记下这次是按哪一种编的
        if let Some(note) = self.last_editlist_note.clone() {
            self.append_log(&note);
        }
    }

    /// 提取音轨那次运行的 [CONFIG] 记录。视频那些参数（CRF、分辨率、格式…）
    /// 跟这一步没关系，不记。
    fn log_extract_config_lines(&mut self) {
        self.append_log("[CONFIG] Audio extraction only");
        self.log_trim_line();
    }

    /// 裁剪范围那一行。编码和提取音轨两边都调它，记法一样。
    fn log_trim_line(&mut self) {
        // 显示解析后的秒数；按帧输入时附带源帧率
        match self.last_trim {
            (None, None) => self.append_log("[CONFIG] Trim: none"),
            (start, end) => {
                let fmt = |v: Option<f64>| match v {
                    Some(s) => trim::format_timecode(s),
                    None => "start".to_string(),
                };
                let fps_note = match &self.last_probe {
                    Some(p) if p.framing.is_valid() => {
                        format!(" @ {:.3} fps", p.framing.fps_f64())
                    }
                    _ => String::new(),
                };
                self.append_log(&format!(
                    "[CONFIG] Trim: {} → {}{fps_note}",
                    fmt(start),
                    fmt(end)
                ));
            }
        }
    }

    fn set_encoding(&mut self, encoding: bool) {
        self.ui.set_encoding(encoding);
        // 编码期间把预览停下：两边都吃 CPU，而且预览还占着音频设备。
        // 界面上那几个按钮此时也已经禁用，这里只是保证音频真的停。
        if encoding {
            if let Some(p) = &self.preview {
                p.set_paused(true);
            }
        }
    }

    /// 自动滚动到日志底部（去抖：同一帧内多次追加只调度一次）
    fn schedule_scroll(&mut self) {
        if self.scroll_pending.get() {
            return;
        }
        self.scroll_pending.set(true);
        let ui_weak = self.ui.as_weak();
        let pending = self.scroll_pending.clone();
        Timer::single_shot(Duration::from_millis(16), move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_scroll_log_to_end();
            }
            pending.set(false);
        });
    }

    // =============================================================
    // 消息对话框
    // =============================================================

    /// 校验没过：黄色三角
    fn show_warning(&mut self, title: String, msg: String) {
        self.show_message(title, msg, "warning");
    }

    /// 真的出错：红色叉
    fn show_error(&mut self, title: String, msg: String) {
        self.show_message(title, msg, "error");
    }

    /// 标题和正文都由调用方按当前语言取好；`icon_kind` 直接决定左侧图标 ——
    /// 标题现在是本地化的，没法再靠字符串比较反推该用哪个图标。
    fn show_message(&mut self, title: String, msg: String, icon_kind: &str) {
        let dialog = crate::MessageDialog::new().expect("failed to create message dialog");
        dialog.set_dialog_title(title.into());
        dialog.set_message(msg.into());
        dialog.set_ok_text(self.strings.ok_text.clone().into());
        dialog.set_icon_kind(icon_kind.into());
        let modal = crate::win_dialog::open(
            &dialog,
            self.main_hwnd(),
            (dialog.get_dialog_w(), dialog.get_dialog_h()),
        );

        let weak = dialog.as_weak();
        dialog.on_ok(move || {
            modal.dismiss();
            if let Some(d) = weak.upgrade() {
                let _ = d.window().hide();
            }
        });
    }

    /// 这个文件的视频轨被 edit list 复制过，问用户转一遍还是转两遍。选完接着往下走
    /// `handle_start_encode`，选择存进 `repeat_decision`，同一个文件不再问第二次。
    fn show_repeat_dialog(&mut self, input_full: &str, info: &RepeatedTimeline) {
        let length = self.strings.duration_text;

        let dialog = crate::MessageDialog::new().expect("failed to create message dialog");
        dialog.set_dialog_title(self.strings.repeat_title.clone().into());
        dialog.set_message(
            self.strings
                .repeat_message
                .replace("{once}", &length(info.declared))
                .replace("{twice}", &length(info.actual))
                .into(),
        );
        dialog.set_ok_text(self.strings.repeat_once_text.clone().into());
        dialog.set_alt_text(self.strings.repeat_twice_text.clone().into());
        dialog.set_icon_kind("info".into());

        let modal = crate::win_dialog::open(
            &dialog,
            self.main_hwnd(),
            (dialog.get_dialog_w(), dialog.get_dialog_h()),
        );

        let weak_ui = dialog.as_weak();
        let weak_self = self.self_weak.clone();
        let path = input_full.to_string();
        let modal_ok = modal.clone();
        dialog.on_ok(move || {
            modal_ok.dismiss();
            if let Some(d) = weak_ui.upgrade() {
                let _ = d.window().hide();
            }
            if let Some(c) = weak_self.upgrade() {
                c.borrow_mut().repeat_decision = Some(RepeatDecision {
                    input: path.clone(),
                    ignore_editlist: true,
                });
                c.borrow_mut().handle_start_encode();
            }
        });

        let weak_ui = dialog.as_weak();
        let weak_self = self.self_weak.clone();
        let path_twice = input_full.to_string();
        dialog.on_alt(move || {
            modal.dismiss();
            if let Some(d) = weak_ui.upgrade() {
                let _ = d.window().hide();
            }
            if let Some(c) = weak_self.upgrade() {
                c.borrow_mut().repeat_decision = Some(RepeatDecision {
                    input: path_twice.clone(),
                    ignore_editlist: false,
                });
                c.borrow_mut().handle_start_encode();
            }
        });
    }
}

/// 按当前语言对应的逻辑尺寸重设窗口物理尺寸。宽高都取自 Slint 侧的 `window-w` /
/// `window-h`（中文窄、英文宽），这样尺寸只在 ui/main.slint 里写一份。
/// DPI 修正与语言切换共用；仅在物理尺寸与目标不一致时才重设。
fn apply_window_size(ui: &crate::MainWindow) {
    let win = ui.window();
    let sf = win.scale_factor() as f64;
    // 复选框和单选钮按这个比例把自己吸附到整数设备像素上（见 FluentMetrics）。
    let metrics = ui.global::<crate::FluentMetrics>();
    if metrics.get_device_pixel_ratio() as f64 != sf {
        metrics.set_device_pixel_ratio(sf as f32);
    }
    let desired = slint::PhysicalSize::new(
        (ui.get_window_w() as f64 * sf) as u32,
        (ui.get_window_h() as f64 * sf) as u32,
    );
    // 不相等才重设，避免在缩放因子尚未稳定时把已正确的窗口改小。
    if win.size() != desired {
        win.set_size(desired);
    }
}

/// 生成 [min, max] 区间内的随机整数（对应 `new Random().Next(min, max)`，
/// 用 uuid v4 的字节取模，避免引入 rand 依赖）
fn random_in_range(min: u32, max: u32) -> u32 {
    let b = uuid::Uuid::new_v4();
    let n = u32::from_le_bytes([b.as_bytes()[0], b.as_bytes()[1], b.as_bytes()[2], b.as_bytes()[3]]);
    min + (n % (max - min + 1))
}
