//! 文件拖放（对应旧程序 `MainWindow.Input_Drop` + 窗口 `AllowDrop`）。
//!
//! 通过 winit 事件钩子捕获 `DroppedFile`，把第一个文件的路径交给回调。

use slint::winit_030::{winit, EventResult, WinitWindowAccessor};

/// 在给定 Slint 窗口上注册拖放监听。`on_file` 收到被丢弃文件的路径字符串。
pub fn handle_drop<F>(window: &slint::Window, mut on_file: F)
where
    F: FnMut(String) + 'static,
{
    window.on_winit_window_event(move |_, event| {
        if let winit::event::WindowEvent::DroppedFile(path) = event {
            on_file(path.to_string_lossy().into_owned());
        }
        EventResult::Propagate
    });
}
