//! 应用控制器：连接 Slint UI 回调与编码服务。
//!
//! 对应旧程序 `MainWindow.xaml.cs` 的事件处理器 + 与 `FfmpegService` 的交互。
//! 保持行为等价：校验顺序、提示文案、[CONFIG] 日志、进度/结果文本、保存日志、
//! 停止与关窗清理等均照搬旧程序。

use crate::error::AppError;
use crate::i18n::{self, Lang, Strings};
use crate::platform::time;
use crate::services::args::{build, EncodeInput};
use crate::services::ffmpeg::{cleanup_temp_files_public, Event, FfmpegService};
use crate::services::format::OutputFormat;
use crate::services::pathutil::{full_path, validate_path};
use crate::services::settings::{self, AppSettings};
use slint::{ComponentHandle, SharedString, Timer, TimerMode, VecModel};
use slint::winit_030::WinitWindowAccessor;
use slint::winit_030::winit::platform::windows::WindowExtWindows;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

// ---------- 硬编码英文提示（旧程序直接写死在 MessageBox.Show，不随语言切换） ----------
const MSG_EMPTY_INPUT: &str = "Input file path cannot be empty.";
const MSG_INVALID_INPUT: &str = "Invalid input file path.";
const MSG_INPUT_NOT_FOUND: &str = "Input file not found.";
const MSG_INVALID_DROP: &str = "Invalid file path dropped.";
const TITLE_VALIDATION: &str = "Validation Error";
const TITLE_ERROR: &str = "Error";

// ---------- 项目链接（对应旧程序 AppConfig；GitHubIssuesUrl 在旧程序中未被使用） ----------
const REPO_URL: &str = "https://github.com/flower-iroseka/x264video4osu";

/// 给 no-frame 对话框接好标题栏回调（拖拽 / 最小化 / 最大化还原 / 关闭=隐藏）。
/// 主窗口的关闭还需清理编码进程，故在 initialize 中单独接线，不用本宏。
/// 依赖 slint 的 `unstable-winit-030` 特性：经 WinitWindowAccessor 拿到底层
/// winit 窗口，drag_window 走原生 HTCAPTION 拖拽（保留 Aero snap）。
macro_rules! wire_title_bar {
    ($comp:expr) => {{
        // ComponentHandle::window() 返回借用（&Window），不能跨 'static 闭包持有；
        // 捕获组件 Weak，回调里 upgrade 后取 window() 的局部借用。
        use slint::winit_030::WinitWindowAccessor;
        let weak = $comp.as_weak();
        $comp.on_title_bar_drag(move || {
            if let Some(c) = weak.upgrade() {
                let _ = c.window().with_winit_window(|w| w.drag_window());
            }
        });
        let weak = $comp.as_weak();
        $comp.on_title_bar_minimize(move || {
            if let Some(c) = weak.upgrade() {
                let _ = c.window().with_winit_window(|w| w.set_minimized(true));
            }
        });
        let weak = $comp.as_weak();
        $comp.on_title_bar_close(move || {
            if let Some(c) = weak.upgrade() {
                let _ = c.window().hide();
            }
        });
    }};
}
pub(crate) use wire_title_bar;

pub struct AppController {
    ui: crate::MainWindow,
    service: FfmpegService,
    rx: Receiver<Event>,
    strings: Strings,
    log_buffer: String,
    /// 逐行日志模型（日志页 ListView 显示用；与 log_buffer 同内容，仅按换行拆分）
    log_lines_model: Rc<VecModel<SharedString>>,
    last_output_path: Option<std::path::PathBuf>,
    current_log_file_name: Option<String>,
    /// 0 = CRF，1 = 2pass（与旧程序 `TwoPassRadio.IsChecked` 对应）
    mode_index: i32,
    /// 持久化设置（主题/语言）；主题切换与语言切换时同步回写
    settings: AppSettings,
    /// 轮询编码事件的中断器（必须持有，否则会停止触发）
    poll_timer: Timer,
    /// 自动滚动去抖标志
    scroll_pending: Rc<Cell<bool>>,
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

        // 读取持久化设置：首次运行（无设置文件）回落默认值（中文 + 默认主题）
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
            current_log_file_name: None,
            mode_index: 0,
            settings,
            poll_timer: Timer::default(),
            scroll_pending: Rc::new(Cell::new(false)),
        }));

        AppController::initialize(&controller);
        controller
    }

    /// 初始化：设置默认语言文本，接线回调，启动轮询定时器，注册拖放与关窗。
    fn initialize(self_rc: &Rc<RefCell<Self>>) {
        let weak = Rc::downgrade(self_rc);

        let rc = self_rc.borrow();
        let ui = &rc.ui;

        // 应用保存的语言与主题（首次运行回落默认：中文 + 默认主题）。语言框索引
        // 通过 <=> 双向绑定，这里只同步界面文本；主题通过 apply-theme 同步全局换肤。
        ui.set_language_index(rc.settings.lang_index);
        ui.invoke_apply_theme(rc.settings.theme_index);
        ui.set_strings(rc.strings.ui.clone());

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

        // ---- 无边框标题栏（no-frame）----
        // window() 返回借用（&Window），不能跨 'static 闭包持有；用组件 Weak 在
        // 回调里 upgrade 后取局部借用。
        let weak_ui = ui.as_weak();
        ui.on_title_bar_drag(move || {
            if let Some(c) = weak_ui.upgrade() {
                let _ = c.window().with_winit_window(|w| w.drag_window());
            }
        });
        let weak_ui = ui.as_weak();
        ui.on_title_bar_minimize(move || {
            if let Some(c) = weak_ui.upgrade() {
                let _ = c.window().with_winit_window(|w| w.set_minimized(true));
            }
        });
        // 关闭按钮与原生 X 等价：先清理编码进程，再隐藏窗口（隐藏触发 keepalive 释放，事件循环退出）
        let w = weak.clone();
        let weak_ui = ui.as_weak();
        ui.on_title_bar_close(move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().cleanup_for_close();
            }
            if let Some(c) = weak_ui.upgrade() {
                let _ = c.window().hide();
            }
        });
        // ---- 语言切换 ----
        let w = weak.clone();
        ui.on_language_changed(move |index: i32| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_language_changed(index);
            }
        });

        // ---- 主题切换 ----
        let w = weak.clone();
        ui.on_theme_changed(move |index: i32| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_theme_changed(index);
            }
        });

        // ---- 编码方式切换（RadioGroup 的 selected 回调） ----
        let w = weak.clone();
        ui.on_mode_changed(move |text: slint::SharedString| {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().handle_mode_changed(text.to_string());
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
        let w = weak.clone();
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            if let Some(c) = w.upgrade() {
                c.borrow_mut().drain_events();
            }
        });
        self_rc.borrow_mut().poll_timer = timer;

        // ---- DPI 缩放适配 ----
        // Slint 的 winit 后端在系统缩放变化后只更新渲染比例、不重设窗口物理
        // 尺寸（event_loop.rs 的 ScaleFactorChanged 忽略了 inner_size_writer），
        // 某些环境下窗口会比渲染画布小，底部按钮栏被裁掉。事件循环启动后按
        // 实际缩放因子把窗口重设为 逻辑尺寸 × 缩放因子。
        let w = Rc::downgrade(self_rc);
        slint::Timer::single_shot(Duration::from_millis(100), move || {
            if let Some(c) = w.upgrade() {
                const WINDOW_W: f32 = 572.0; // 与 ui/main.slint 的 Window width 保持一致
                let c = c.borrow();
                // 原生标题栏不会自动采用 exe 资源图标：启动后把嵌入的应用图标设为窗口图标。
                let hwnd = crate::win_icon::main_window_hwnd(c.ui.window());
                crate::win_icon::apply_window_icon(hwnd);
                // 与 ui/main.slint 的 Window height 保持一致：Aero 577（内容 545 + 标题栏 32），默认主题 545
                let window_h = if c.settings.theme().is_aero() { 577.0 } else { 545.0 };
                let win = c.ui.window();
                let sf = win.scale_factor() as f64;
                let desired = slint::PhysicalSize::new(
                    (WINDOW_W as f64 * sf) as u32,
                    (window_h as f64 * sf) as u32,
                );
                // 仅当物理尺寸与目标不一致时才重设，避免在缩放因子尚未
                // 稳定时把已正确的窗口改小。
                if win.size() != desired {
                    win.set_size(desired);
                }
            }
        });
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
        let dialog = rfd::FileDialog::new().add_filter("Video Files", &["mp4", "flv", "avi"]);
        if let Some(mut path) = dialog.save_file() {
            // 对应旧程序 SaveFileDialog.DefaultExt：用户未输入扩展名时补全
            if path.extension().is_none() {
                path.set_extension(ext);
            }
            self.ui.set_output_path(path.to_string_lossy().into_owned().into());
        }
    }

    fn handle_start_encode(&mut self) {
        let input = EncodeInput {
            input_text: self.ui.get_input_path().to_string(),
            output_text: self.ui.get_output_path().to_string(),
            format_index: self.ui.get_format_index(),
            mode_index: self.mode_index,
            value_text: self.ui.get_value_text().to_string(),
            width_text: self.ui.get_width_text().to_string(),
            height_text: self.ui.get_height_text().to_string(),
            scale_up: self.ui.get_scale_up(),
            extract_audio: self.ui.get_extract_audio(),
        };

        // ---- 校验（顺序与文案照搬旧程序 Start_Click） ----
        let input_path = input.input_text.trim();
        if input_path.is_empty() {
            self.show_message(TITLE_VALIDATION, MSG_EMPTY_INPUT);
            return;
        }
        if !validate_path(input_path) {
            self.show_message(TITLE_VALIDATION, MSG_INVALID_INPUT);
            return;
        }
        let full = full_path(input_path).unwrap_or_default();
        if !Path::new(&full).exists() {
            self.show_message(TITLE_ERROR, MSG_INPUT_NOT_FOUND);
            return;
        }

        self.set_encoding(true);

        match build(input) {
            Ok(plan) => {
                self.last_output_path = Some(plan.output.clone());
                self.current_log_file_name = if plan.log_file_name.is_empty() {
                    None
                } else {
                    Some(plan.log_file_name.clone())
                };

                self.ui.set_progress_visible(true);
                self.ui.set_result_visible(false);
                self.ui.set_progress_value(0.0);

                // 新编码开始即清空日志：log_buffer / 行模型只增不清，若不清空，
                // 同进程内多次编码会无限累积（内存随会话一直涨，且"保存日志"
                // 存到的会是历次编码拼在一起的全量文本）。
                self.reset_log();
                self.log_config_lines();
                self.service.reset();
                self.service.start(plan);
            }
            Err(e) => {
                self.set_encoding(false);
                let (title, msg) = match &e {
                    AppError::InvalidArgument(m) => {
                        (TITLE_VALIDATION, format!("Invalid parameter: {m}"))
                    }
                    _ => (TITLE_ERROR, format!("Failed to start encoding: {e}")),
                };
                self.show_message(title, &msg);
            }
        }
    }

    fn handle_stop_encode(&mut self) {
        // 旧程序先恢复按钮状态，再停止
        self.set_encoding(false);
        self.service.stop();
        if let (Some(out), Some(log)) = (&self.last_output_path, &self.current_log_file_name) {
            cleanup_temp_files_public(&out.to_string_lossy(), log);
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
        wire_title_bar!(dialog);
        // 对话框单独持有自己的 FluentPalette 实例，需各自应用当前主题
        dialog.invoke_apply_theme(self.settings.theme_index);
        dialog.set_dialog_title(self.strings.about_title.clone().into());
        dialog.set_version_line(self.strings.about_version_line.clone().into());
        dialog.set_description(self.strings.about_description.clone().into());
        dialog.set_repository_text(self.strings.repository_text.clone().into());
        dialog.set_close_text(self.strings.close_text.clone().into());
        dialog.set_titlebar_tooltip_minimize(self.strings.titlebar_minimize.clone().into());
        dialog.set_titlebar_tooltip_maximize(self.strings.titlebar_maximize.clone().into());
        dialog.set_titlebar_tooltip_close(self.strings.titlebar_close.clone().into());

        let url_repo = REPO_URL.to_string();
        dialog.on_open_repository(move || {
            let _ = crate::io::external::open_url(&url_repo);
        });

        // ---- 模态化：禁用主窗口（无法对焦/操作），对话框任意关闭路径恢复 ----
        // 若不禁用，关于窗口打开时关掉主窗口只会隐藏主窗口，事件循环因对话框
        // 仍可见而继续运行，整个程序退不出去。winit 的 set_enable 封装了
        // EnableWindow：被禁用的窗口无法激活、无法接收输入，正是「主窗口无法
        // 对焦或者操作」。注意 show_message 不模态（编码中可能弹错，不能挡住
        // 「停止」按钮），故这里只针对关于窗口。
        {
            let main = self.ui.window();
            let _ = main.with_winit_window(|w| w.set_enable(false));
        }
        let main_weak = self.ui.as_weak();
        let reenable_main = move || {
            if let Some(m) = main_weak.upgrade() {
                let _ = m.window().with_winit_window(|w| w.set_enable(true));
            }
        };

        // ---- 闪烁反馈：检测「点击被禁用的主窗口」----
        // 主窗口禁用后应用层收不到它的鼠标消息（EnableWindow 直接拦截），OS 自带的
        // 「闪烁 owned 对话框标题栏」反馈又只作用于原生 caption（no-frame 无），因此
        // 装 WH_MOUSE_LL 低级鼠标钩子在输入层捕获「按下点命中主窗口」，命中即触发
        // 关于窗口标题栏/边框闪烁（见 win_flash 模块）。三条关闭路径都要 about_closed。
        {
            let main_hwnd = self
                .ui
                .window()
                .with_winit_window(|w| {
                    use slint::winit_030::winit::raw_window_handle::HasWindowHandle;
                    match w.window_handle() {
                        Ok(h) => match h.as_raw() {
                            slint::winit_030::winit::raw_window_handle::RawWindowHandle::Win32(
                                win32,
                            ) => win32.hwnd.get() as *mut core::ffi::c_void,
                            _ => std::ptr::null_mut(),
                        },
                        Err(_) => std::ptr::null_mut(),
                    }
                })
                .unwrap_or(std::ptr::null_mut());
            crate::win_flash::about_opened(main_hwnd, &dialog);
        }

        // 「关闭」按钮
        let weak = dialog.as_weak();
        let r = reenable_main.clone();
        dialog.on_close_dialog(move || {
            crate::win_flash::about_closed();
            r();
            if let Some(d) = weak.upgrade() {
                let _ = d.window().hide();
            }
        });
        // 标题栏 X（覆盖 wire_title_bar 默认 close：先恢复主窗口再隐藏）
        let weak = dialog.as_weak();
        let r = reenable_main.clone();
        dialog.on_title_bar_close(move || {
            crate::win_flash::about_closed();
            r();
            if let Some(d) = weak.upgrade() {
                let _ = d.window().hide();
            }
        });
        // 系统关闭（ALT+F4 / 任务栏关闭）
        let r = reenable_main.clone();
        dialog.window().on_close_requested(move || {
            crate::win_flash::about_closed();
            r();
            slint::CloseRequestResponse::HideWindow
        });

        let _ = dialog.window().show();
    }

    fn handle_language_changed(&mut self, index: i32) {
        let lang = if index == 0 { Lang::Zh } else { Lang::En };
        self.strings = i18n::for_lang(lang);
        self.ui.set_strings(self.strings.ui.clone());
        // 持久化语言选择（语言框索引已通过 <=> 同步到 UI，这里只需保存）
        self.settings.lang_index = index;
        settings::save(&self.settings);
    }

    fn handle_theme_changed(&mut self, index: i32) {
        // 同步全局换肤（is-aero），并持久化主题选择
        self.ui.invoke_apply_theme(index);
        self.settings.theme_index = index;
        settings::save(&self.settings);
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
            self.show_message(TITLE_ERROR, MSG_INVALID_DROP);
        }
    }

    /// 窗口关闭：停止编码并清理临时文件（对应 MainWindow_Closing -> CleanupAll）
    fn cleanup_for_close(&mut self) {
        self.service.stop();
        self.service.kill_now();
        if let (Some(out), Some(log)) = (&self.last_output_path, &self.current_log_file_name) {
            cleanup_temp_files_public(&out.to_string_lossy(), log);
        }
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
                    self.ui.set_pass_text(format!("Pass {pass_index}/{pass_count}").into());
                    self.ui.set_percent_text(format!("{percent:.0}%").into());
                    self.ui.set_eta_text(format!("ETA: {}", time::format_mmss(remaining_secs)).into());
                    self.ui
                        .set_elapsed_text(format!("Elapsed: {}", time::format_mmss(elapsed_secs)).into());
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
                    self.show_message(TITLE_ERROR, &format!("Failed to start encoding: {msg}"));
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
            let size = std::fs::metadata(output_path)
                .map(|m| m.len())
                .unwrap_or(0);
            self.ui
                .set_output_size_text(format!("Output size: {:.2} MB", size as f64 / 1024.0 / 1024.0).into());
        }
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
    }

    fn set_encoding(&mut self, encoding: bool) {
        self.ui.set_encoding(encoding);
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

    fn show_message(&mut self, title: &str, msg: &str) {
        let dialog = crate::MessageDialog::new().expect("failed to create message dialog");
        wire_title_bar!(dialog);
        // 对话框单独持有自己的 FluentPalette 实例，需各自应用当前主题
        dialog.invoke_apply_theme(self.settings.theme_index);
        dialog.set_dialog_title(title.into());
        dialog.set_message(msg.into());
        dialog.set_ok_text(self.strings.ok_text.clone().into());
        dialog.set_titlebar_tooltip_minimize(self.strings.titlebar_minimize.clone().into());
        dialog.set_titlebar_tooltip_maximize(self.strings.titlebar_maximize.clone().into());
        dialog.set_titlebar_tooltip_close(self.strings.titlebar_close.clone().into());
        let weak = dialog.as_weak();
        dialog.on_ok(move || {
            if let Some(d) = weak.upgrade() {
                let _ = d.window().hide();
            }
        });
        let _ = dialog.window().show();
    }
}

/// 生成 [min, max] 区间内的随机整数（对应 `new Random().Next(min, max)`，
/// 用 uuid v4 的字节取模，避免引入 rand 依赖）
fn random_in_range(min: u32, max: u32) -> u32 {
    let b = uuid::Uuid::new_v4();
    let n = u32::from_le_bytes([b.as_bytes()[0], b.as_bytes()[1], b.as_bytes()[2], b.as_bytes()[3]]);
    min + (n % (max - min + 1))
}
