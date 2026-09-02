//! x264video4osu — osu! 背景视频压制工具（Rust + Slint 移植版）。
//!
//! 对应旧 C#/WPF 项目，功能等价：通过 ffmpeg 使用 x264 编码器为 osu!
//! 制作兼容的背景视频（支持 CRF / 2pass、FLV/AVI/MP4、分辨率缩放、音轨分离）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use slint::ComponentHandle;

mod app;
mod error;
mod i18n;
mod io;
mod platform;
mod services;
mod win_flash;
#[cfg(test)]
mod log_layout_test;
#[cfg(test)]
mod method_row_layout_test;
#[cfg(test)]
mod res_row_layout_test;

include!(concat!(env!("OUT_DIR"), "/main.rs"));
include!(concat!(env!("OUT_DIR"), "/about.rs"));
include!(concat!(env!("OUT_DIR"), "/ffmpeg_not_found.rs"));
include!(concat!(env!("OUT_DIR"), "/message.rs"));

use crate::app::wire_title_bar;

fn main() {
    // 先校验 FFmpeg / ffprobe 工具，缺失时展示提示对话框；无论点击哪个按钮
    // 对话框关闭后都退出进程（与旧程序一致，退出码 1）。
    if services::ffmpeg_config::validate_tools().is_err() {
        show_tools_missing_dialog();
        std::process::exit(1);
    }

    let controller = app::AppController::create();
    let ui = controller.borrow().window_handle();
    ui.run().expect("failed to run UI event loop");
}

/// 展示「未找到 FFmpeg」对话框；两个按钮（下载/退出）都会关闭对话框，
/// 之后调用方以退出码 1 结束进程。
fn show_tools_missing_dialog() {
    // 用持久化的语言与主题构造对话框（与主窗口一致；首次运行回落默认中文 + 默认主题）
    let settings = services::settings::load();
    let lang = if settings.lang_index == 0 { i18n::Lang::Zh } else { i18n::Lang::En };
    let strings = i18n::for_lang(lang);
    let dialog = crate::FfmpegNotFoundDialog::new().expect("failed to create ffmpeg-not-found dialog");
    wire_title_bar!(dialog);
    dialog.invoke_apply_theme(settings.theme_index);
    dialog.set_dialog_title(strings.not_found_title.into());
    dialog.set_message(strings.not_found_message.into());
    dialog.set_download_text(strings.download_text.into());
    dialog.set_exit_text(strings.exit_text.into());
    dialog.set_titlebar_tooltip_minimize(strings.titlebar_minimize.into());
    dialog.set_titlebar_tooltip_maximize(strings.titlebar_maximize.into());
    dialog.set_titlebar_tooltip_close(strings.titlebar_close.into());

    let url = services::ffmpeg_config::download_url().to_string();
    let weak = dialog.as_weak();
    dialog.on_download(move || {
        // 与旧程序一致：先打开下载页，再关闭对话框
        let _ = io::external::open_url(&url);
        if let Some(d) = weak.upgrade() {
            let _ = d.window().hide();
        }
    });
    let weak = dialog.as_weak();
    dialog.on_exit_dialog(move || {
        if let Some(d) = weak.upgrade() {
            let _ = d.window().hide();
        }
    });

    // 模态运行：直到对话框被关闭（run() 返回）
    let _ = dialog.run();
}
