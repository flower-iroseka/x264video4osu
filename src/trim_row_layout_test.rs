//! 无头几何测试：验证裁剪范围行的排版 —— 起点 caption 对齐 90px 标签列、
//! 两个 LineEdit 保持固定宽度不被拉伸、caption 紧贴输入框。
//!
//! 复刻 ui/main.slint 裁剪范围行的结构（用默认风格，布局机制与风格无关）：
//! LineEdit 要覆盖 fluent 默认的 `horizontal-stretch: 1` / `min-width: max(160px, …)`，
//! 否则输入框会把整行拉满、把 caption 挤走（resolution 行就是为这个坑写的测试）。

#![cfg(test)]

use slint::ComponentHandle;

slint::slint! {
    import { LineEdit, HorizontalBox } from "std-widgets.slint";

    export component TrimRowTest inherits Window {
        width: 440px;
        height: 120px;

        in-out property <bool> trim-enabled: true;
        in-out property <string> trim-start-text;
        in-out property <string> trim-end-text;

        out property <length> dbg-range-x: rangelabel.x;
        out property <length> dbg-range-w: rangelabel.width;
        out property <length> dbg-startlabel-x: startlabel.x;
        out property <length> dbg-startlabel-w: startlabel.width;
        out property <length> dbg-startedit-x: startedit.x;
        out property <length> dbg-startedit-w: startedit.width;
        out property <length> dbg-endlabel-x: endlabel.x;
        out property <length> dbg-endlabel-w: endlabel.width;
        out property <length> dbg-endedit-x: endedit.x;
        out property <length> dbg-endedit-w: endedit.width;
        out property <bool> dbg-startedit-enabled: startedit.enabled;

        HorizontalBox {
            vertical-stretch: 0;
            spacing: 4px;
            rangelabel := Text { text: "裁剪范围"; width: 52px; vertical-alignment: center; }
            startlabel := Text { text: "起点"; vertical-alignment: center; }
            startedit := LineEdit { min-width: 110px; horizontal-stretch: 0; enabled: root.trim-enabled; text <=> root.trim-start-text; }
            Rectangle { horizontal-stretch: 1; }
            endlabel := Text { text: "终点"; vertical-alignment: center; }
            endedit := LineEdit { min-width: 110px; horizontal-stretch: 0; text <=> root.trim-end-text; }
        }
    }
}

#[test]
fn trim_row_aligns_to_control_column_and_keeps_fixed_box_width() {
    i_slint_backend_testing::init_no_event_loop();

    let win = TrimRowTest::new().expect("failed to create TrimRowTest");
    win.set_trim_start_text("0".into());
    win.set_trim_end_text("00:00:05.000".into());
    let _ = win.show();

    // 1. 输入框宽度：min-width 110px 生效，起点/终点等宽
    let sw = win.get_dbg_startedit_w();
    eprintln!("trim start edit box = {sw}px");
    assert!(
        (sw - 110.0).abs() <= 1.0,
        "min-width override failed: start edit rendered {sw}px, expected 110px"
    );
    assert_eq!(sw, win.get_dbg_endedit_w(), "start/end boxes must be equal width");

    // 2. 起点 caption 与上面各行的控件起始列对齐：90px 标签右缘 + 外层 4px 间距
    let range_right = win.get_dbg_range_x() + win.get_dbg_range_w();
    let align_offset = win.get_dbg_startlabel_x() - range_right;
    eprintln!("start caption offset from label column = {align_offset}px");
    assert!(
        (align_offset - 4.0).abs() <= 1.0,
        "start caption not aligned with control column (offset={align_offset}px, expected 4px)"
    );

    // 3. caption 紧贴输入框（行内 4px 间距）
    let gap_start = win.get_dbg_startedit_x() - (win.get_dbg_startlabel_x() + win.get_dbg_startlabel_w());
    let gap_end = win.get_dbg_endedit_x() - (win.get_dbg_endlabel_x() + win.get_dbg_endlabel_w());
    eprintln!("caption→box gap: start={gap_start}px end={gap_end}px");
    assert!((gap_start - 4.0).abs() <= 1.0, "start caption not hugging its box (gap={gap_start}px)");
    assert!((gap_end - 4.0).abs() <= 1.0, "end caption not hugging its box (gap={gap_end}px)");

    // 4. 未勾选裁剪时输入框为禁用态
    win.set_trim_enabled(false);
    assert!(!win.get_dbg_startedit_enabled(), "start edit should be disabled when trim is off");

    let _ = win.hide();
}
