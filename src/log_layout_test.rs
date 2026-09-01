//! 无头布局测试：验证日志页（ListView 逐行渲染）在真实 Tab 布局下的滚动行为。
//!
//! 用 Slint 测试后端（不弹窗）复刻 ui/main.slint 日志页结构，断言：
//! 1. 多行日志时 viewport-height > visible-height（内容溢出 → 有滚动条、可滚动）；
//! 2. scroll-to-end 把 viewport-y 设到最底部（Slint 里 viewport-y 顶部为 0、
//!    向下为负，所以正确公式是负值）；
//! 3. 空日志时无溢出。
//!
//! 注意：无事件循环的测试后端不会在 show 之后因模型变化触发重排，所以每个用例
//! 都在 show 之前把数据装好；运行时由事件循环驱动，追加日志后会自动重排。

#![cfg(test)]

use slint::{ComponentHandle, VecModel};

slint::slint! {
    import { TabWidget, ListView, VerticalBox, HorizontalBox } from "std-widgets.slint";

    // 复刻 ui/main.slint 的 LogView（VerticalBox 根 + ListView 逐行渲染）
    component LogViewReplica inherits VerticalBox {
        in property <[string]> log-lines;
        in property <bool> auto-scroll: true;

        in-out property <length> viewport-y <=> i-scroll-view.viewport-y;
        out property <length> viewport-height <=> i-scroll-view.viewport-height;
        out property <length> visible-height <=> i-scroll-view.visible-height;

        callback scroll-to-end();

        i-scroll-view := ListView {
            horizontal-stretch: 1;
            vertical-stretch: 1;
            for line in root.log-lines: Text {
                text: line;
                wrap: word-wrap;
                font-family: "Consolas";
                font-size: 11px;
            }
        }

        scroll-to-end => {
            i-scroll-view.viewport-y = -(i-scroll-view.viewport-height - i-scroll-view.visible-height);
        }
    }

    // 复刻日志页：TabWidget > Tab > Rectangle > VerticalBox > LogView + 底部按钮行
    export component TestWindow inherits Window {
        width: 572px;
        height: 545px;
        in property <[string]> log-lines;
        in property <length> vh: log-view.viewport-height;
        in property <length> vv: log-view.visible-height;
        in property <length> vy: log-view.viewport-y;

        callback scroll-to-end();
        scroll-to-end => { log-view.scroll-to-end(); }

        TabWidget {
            horizontal-stretch: 1;
            vertical-stretch: 1;
            Tab { title: "log";
                Rectangle {
                    background: #F8F9FA;
                    border-width: 1px;
                    border-color: #E0E0E0;
                    border-radius: 4px;
                    VerticalBox {
                        padding: 8px;
                        spacing: 6px;
                        log-view := LogViewReplica {
                            horizontal-stretch: 1;
                            vertical-stretch: 1;
                            log-lines: root.log-lines;
                            auto-scroll: true;
                        }
                        HorizontalBox {
                            alignment: end;
                            spacing: 10px;
                            Rectangle { width: 80px; height: 20px; }
                        }
                    }
                }
            }
        }
    }
}

fn make_lines(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("[line {i}] the quick brown fox jumps over the lazy dog")).collect()
}

fn set_lines(win: &TestWindow, lines: Vec<String>) {
    let model = VecModel::from(lines.into_iter().map(slint::SharedString::from).collect::<Vec<_>>());
    win.set_log_lines(std::rc::Rc::new(model).into());
}

/// 1：多行日志必须溢出（viewport > visible → 滚动条与滚动有效的前提）。
#[test]
fn log_view_overflows_with_many_lines() {
    i_slint_backend_testing::init_no_event_loop();

    let win = TestWindow::new().expect("failed to create TestWindow");
    set_lines(&win, make_lines(200));
    win.show();

    let (vh, vv) = (win.get_vh(), win.get_vv());
    eprintln!("200 lines: viewport={vh:?} visible={vv:?}");
    assert!(vh > vv, "200-line log must overflow (viewport {vh:?} > visible {vv:?})");
    win.hide();
}

/// 3：空日志不溢出。
#[test]
fn log_view_no_overflow_when_empty() {
    i_slint_backend_testing::init_no_event_loop();

    let win = TestWindow::new().expect("failed to create TestWindow");
    set_lines(&win, Vec::new());
    win.show();

    let (vh, vv) = (win.get_vh(), win.get_vv());
    eprintln!("empty: viewport={vh:?} visible={vv:?}");
    assert!(vh <= vv, "empty log should not overflow");
    win.hide();
}

/// 2：scroll-to-end 必须把 viewport-y 设到最底部（负值 = 向下滚动）。
#[test]
fn scroll_to_end_goes_to_bottom() {
    i_slint_backend_testing::init_no_event_loop();

    let win = TestWindow::new().expect("failed to create TestWindow");
    set_lines(&win, make_lines(200));
    win.show();

    let (vh, vv) = (win.get_vh(), win.get_vv());
    win.invoke_scroll_to_end();

    let bottom = win.get_vy();
    let zero = vv - vv; // 同类型零值
    let expected = -(vh - vv); // 正确底部 = 负的 (viewport - visible)
    eprintln!("viewport-height={vh:?} visible-height={vv:?} after-scroll viewport-y={bottom:?}");
    assert!(
        bottom < zero,
        "scroll-to-end did not scroll down (viewport-y={bottom:?}): formula likely writes a positive value that clamps to top"
    );
    assert!(
        bottom >= expected,
        "scrolled past the bottom: viewport-y={bottom:?} expected≈{expected:?}"
    );
    win.hide();
}
