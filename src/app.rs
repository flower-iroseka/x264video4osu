//! 应用控制器：连接 Slint UI 回调与编码服务。
//!
//! 对应旧程序 `MainWindow.xaml.cs` 的事件处理器 + 与 `FfmpegService` 的交互。
//! 保持行为等价：校验顺序、提示文案、[CONFIG] 日志、进度/结果文本、保存日志、
//! 停止与关窗清理等均照搬旧程序。

use crate::error::AppError;
use crate::i18n::{self, Lang, Strings};
use crate::platform::time;
use crate::services::args::{build, EncodeInput};
use crate::services::ffmpeg::{
    cleanup_temp_files_public, Event, FfmpegService, RepeatedTimeline,
};
use crate::services::format::OutputFormat;
use crate::services::pathutil::{full_path, validate_path};
use crate::services::preview::{self, Preview};
use crate::services::settings::{self, AppSettings};
use crate::services::trim;
use slint::{ComponentHandle, SharedString, Timer, TimerMode, VecModel};
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::mpsc::Receiver;
use std::time::Duration;

// ---------- 项目链接（对应旧程序 AppConfig；GitHubIssuesUrl 在旧程序中未被使用） ----------
const REPO_URL: &str = "https://github.com/flower-iroseka/x264video4osu";

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
    /// 抽帧探测结果缓存（按帧裁剪时要拿帧率换算，日志里也要用）
    last_probe: Option<crate::services::ffmpeg::MediaInfo>,
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
    /// 当前预览文件的帧率。0 = 探不出来，此时帧号相关的控件禁用。
    /// 和按帧裁剪用的是同一次 ffprobe 结果，两边帧号必须一致。
    preview_fps: f64,
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
            preview_fps: 0.0,
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
                c.borrow_mut().handle_start_encode();
            }
        });
        let w = weak.clone();
        ui.on_stop_encode(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_stop_encode();
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
                    p.set_paused(!p.state().paused);
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

    fn handle_start_encode(&mut self) {
        let input_text = self.ui.get_input_path().to_string();
        let output_text = self.ui.get_output_path().to_string();

        // ---- 校验（顺序与文案照搬旧程序 Start_Click） ----
        let input_path = input_text.trim().to_string();
        if input_path.is_empty() {
            self.show_warning(
                self.strings.title_validation.clone(),
                self.strings.msg_empty_input.clone(),
            );
            return;
        }
        if !validate_path(&input_path) {
            self.show_warning(
                self.strings.title_validation.clone(),
                self.strings.msg_invalid_input.clone(),
            );
            return;
        }
        let full = full_path(&input_path).unwrap_or_default();
        if !Path::new(&full).exists() {
            self.show_error(
                self.strings.title_error.clone(),
                self.strings.msg_input_not_found.clone(),
            );
            return;
        }

        // ---- 视频轨被 edit list 复制过？（"战渣浪"那类为躲二压做出来的文件） ----
        // 这类文件的容器只声明一遍时长，ffmpeg 却按 edit list 编两遍。进度条用的总时长
        // 取自声明值，跑到一半就到 100%，看起来像是一直不结束。这里先问用户要编几遍，
        // 同一个文件问过一次就不再问。
        let full_str = full.to_string_lossy().into_owned();
        let repeated = crate::services::ffmpeg::probe_repeated_timeline(&full_str);
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
        let (trim_start_secs, trim_end_secs) = match self.resolve_trim(&full, timeline_secs) {
            Ok(v) => v,
            Err(msg) => {
                let title = self.strings.title_validation.clone();
                self.show_warning(title, msg);
                return;
            }
        };

        // 覆盖 ffprobe 读到的总时长：视频轨被 edit list 复制时，容器声明的只是
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
            extract_audio: self.ui.get_extract_audio(),
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
                // 参数不合法算用户填错了，其余当启动失败。冒号后面是底层原文，不翻译。
                match &e {
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
        }
    }

    /// 把界面上的裁剪起止解析成秒。未启用、或起止都空 → (None, None) 表示不裁剪。
    /// 按帧输入时用源视频帧率换算（第 0 帧 = 源第一帧）；超出总时长会 clamp。
    ///
    /// `duration_override` 是「编两遍」时 edit list 展开后的长度，用它当 clamp 的上限；
    /// 传 None 就按 ffprobe 报的容器声明时长算。
    fn resolve_trim(
        &mut self,
        input_full: &Path,
        duration_override: Option<f64>,
    ) -> Result<(Option<f64>, Option<f64>), String> {
        self.last_probe = None;
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
        let probe = crate::services::ffmpeg::probe_media(&input_full.to_string_lossy());
        if let Some(p) = &probe {
            self.last_probe = Some(p.clone());
        }
        let fps = probe.as_ref().map(|p| p.fps).unwrap_or(0.0);
        let nb_frames = probe.as_ref().map(|p| p.nb_frames).unwrap_or(0);
        let duration = duration_override
            .or_else(|| probe.as_ref().map(|p| p.duration))
            .unwrap_or(0.0);

        if by_frame && fps <= 0.0 {
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
                crate::services::trim::frame_to_secs(frame, fps)
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

    fn handle_open_folder(&mut self) {
        if let Some(out) = &self.last_output_path {
            let dir = out
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !dir.is_empty() {
                let _ = crate::io::external::open_folder(&dir);
            }
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
        self.trim_unit_index = new_index;

        let start = self.ui.get_trim_start_text().to_string();
        let end = self.ui.get_trim_end_text().to_string();
        // 两个框都空着就没什么可换的，也别为它白跑一次 ffprobe
        if start.trim().is_empty() && end.trim().is_empty() {
            return;
        }
        // 两边换算都要帧率（帧 → 秒），拿不到就原样留着，
        // 和按帧编码时会报 msg_no_fps 的口径一致
        let fps = match self.probe_fps_for_trim() {
            Some(fps) => fps,
            None => return,
        };
        let to_frames = self.trim_unit_index == 0;
        self.ui
            .set_trim_start_text(trim::convert_unit_text(&start, to_frames, fps).into());
        self.ui
            .set_trim_end_text(trim::convert_unit_text(&end, to_frames, fps).into());
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
        // 帧号要和按帧裁剪用同一套算法，都用 trim 里那两个换算函数
        if st.loaded && self.preview_fps > 0.0 {
            self.ui
                .set_preview_frame_cur(trim::secs_to_frame(st.time_pos, self.preview_fps).to_string().into());
            self.ui.set_preview_frame_total(
                trim::secs_to_frame(st.duration, self.preview_fps).to_string().into(),
            );
        } else {
            self.ui.set_preview_frame_cur("--".into());
            self.ui.set_preview_frame_total("--".into());
        }
        // 用户正在拖进度条时别覆盖他手上的值
        if !self.ui.get_preview_scrubbing() {
            self.ui.set_preview_seek_1000((st.progress() * 1000.0) as f32);
        }
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
        // 帧号换算要帧率。和按帧裁剪走同一条 ffprobe，别另起一套，
        // 否则「当前帧为开头」填进去的帧号和编码时算的对不上。
        self.preview_fps = self.probe_fps_for_trim().unwrap_or(0.0);
        self.ui.set_preview_fps_known(self.preview_fps > 0.0);
        // 预览面板按真实宽高比摆画面。上一次探测的结果里带着宽高，不用再跑一遍 ffprobe；
        // 探不到（比如 ffprobe 只给了部分字段）就按 16:9 摆。
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
            if self.preview_fps > 0.0 {
                trim::secs_to_frame(secs, self.preview_fps).to_string()
            } else {
                "0".to_string()
            }
            .into(),
        );
        self.preview_syncing = false;
    }

    /// 用户改了帧号框：把对应的时间戳算出来填进另一个框。
    fn preview_frame_edited(&mut self, text: &str) {
        if self.preview_syncing || self.preview_fps <= 0.0 {
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
        let secs = trim::frame_to_secs(frame, self.preview_fps);
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
        if self.preview_fps <= 0.0 {
            return;
        }
        let frame = trim::secs_to_frame(secs, self.preview_fps);
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
        let secs = trim::parse_timecode(&time_text).or_else(|| {
            frame_text
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|_| self.preview_fps > 0.0)
                .map(|f| trim::frame_to_secs(f, self.preview_fps))
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
            (p.state().duration - 1.0 / self.preview_fps.max(1.0)).max(0.0)
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
            if self.preview_fps <= 0.0 {
                return;
            }
            trim::secs_to_frame(secs, self.preview_fps).to_string()
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

    /// 取源视频帧率（按帧裁剪的换算要用）。探测结果沿用 `last_probe` 的缓存策略。
    fn probe_fps_for_trim(&mut self) -> Option<f64> {
        if let Some(info) = &self.last_probe {
            if info.fps > 0.0 {
                return Some(info.fps);
            }
        }
        let path = self.ui.get_input_path().to_string();
        let info = crate::services::ffmpeg::probe_media(&path)?;
        let fps = info.fps;
        self.last_probe = Some(info);
        (fps > 0.0).then_some(fps)
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
        let crf = if !is_two_pass && (0..=51).contains(&value_box) {
            value_box
        } else {
            23
        };

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
        self.append_log(&format!(
            "[CONFIG] Audio extraction: {}",
            if self.ui.get_extract_audio() { "enabled" } else { "disabled" }
        ));

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

        // 裁剪范围：显示解析后的秒数；按帧输入时附带源帧率
        match self.last_trim {
            (None, None) => self.append_log("[CONFIG] Trim: none"),
            (start, end) => {
                let fmt = |v: Option<f64>| match v {
                    Some(s) => trim::format_timecode(s),
                    None => "start".to_string(),
                };
                let fps_note = match &self.last_probe {
                    Some(p) if p.fps > 0.0 => format!(" @ {:.3} fps", p.fps),
                    _ => String::new(),
                };
                self.append_log(&format!(
                    "[CONFIG] Trim: {} → {}{fps_note}",
                    fmt(start),
                    fmt(end)
                ));
            }
        }

        // 视频轨被 edit list 复制过时，记下这次是按哪一种编的
        if let Some(note) = self.last_editlist_note.clone() {
            self.append_log(&note);
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
