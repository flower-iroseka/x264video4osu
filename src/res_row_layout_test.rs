//! 无头几何测试：验证分辨率行排版 —— 「宽度」对齐上面输入框的起始列、caption 紧贴输入框、
//! 「高度」放在剩余空间约 45% 处。
//!
//! 复刻 ui/main.slint 分辨率行的布局结构（用默认风格，布局机制与风格无关）：
//! - 输入框 `min-width: 70px` + `horizontal-stretch: 0`，覆盖 fluent LineEdit
//!   默认的 `horizontal-stretch: 1` / `min-width: max(160px, …)`（否则输入框会把整行拉满）；
//! - 「宽度」标签紧跟 90px 标签列之后（与上面输入框的起始列对齐），输入框紧贴其后；
//! - 剩余横向空间由两个弹性空白按 45%:55% 分割，在约 45% 处放入「高度」+ 高度输入框。
//!
//! 断言：
//! 1. 输入框宽度 == 70px（可容纳 5 位数）；
//! 2. 「宽度」组的 x == 分辨率标签右缘 + 外层间距（即与上面输入框起始列对齐）；
//! 3. caption 与输入框间隙很小（==3px，"紧跟着"）；
//! 4. 高度组前弹性空白 / (前后弹性空白之和) ≈ 45%。

#![cfg(test)]

use slint::ComponentHandle;

slint::slint! {
    import { LineEdit, HorizontalBox } from "std-widgets.slint";

    export component ResRowTest inherits Window {
        width: 572px;
        height: 120px;

        in-out property <string> width-text;
        in-out property <string> height-text;

        out property <length> dbg-reslabel-x: reslabel.x;
        out property <length> dbg-reslabel-w: reslabel.width;
        out property <length> dbg-wgroup-x: wgroup.x;
        out property <length> dbg-wgroup-w: wgroup.width;
        out property <length> dbg-hgroup-x: hgroup.x;
        out property <length> dbg-wlabel-x: wlabel.x;
        out property <length> dbg-wlabel-w: wlabel.width;
        out property <length> dbg-wedit-x: wedit.x;
        out property <length> dbg-wedit-w: wedit.width;
        out property <length> dbg-hlabel-x: hlabel.x;
        out property <length> dbg-hlabel-w: hlabel.width;
        out property <length> dbg-hedit-x: hedit.x;
        out property <length> dbg-hedit-w: hedit.width;
        out property <length> dbg-spacer45-w: spacer45.width;
        out property <length> dbg-spacer55-w: spacer55.width;

        HorizontalBox {
            vertical-stretch: 0;
            // 与上面各行的 spacing:4px 一致（输入框起始列对齐的关键）
            spacing: 4px;
            reslabel := Text { text: "分辨率"; width: 90px; vertical-alignment: center; }
            wgroup := HorizontalLayout {
                spacing: 3px;
                wlabel := Text { text: "宽度"; vertical-alignment: center; }
                wedit := LineEdit { min-width: 70px; horizontal-stretch: 0; text <=> root.width-text; }
            }
            spacer45 := Rectangle { horizontal-stretch: 45; }
            hgroup := HorizontalLayout {
                spacing: 3px;
                hlabel := Text { text: "高度"; vertical-alignment: center; }
                hedit := LineEdit { min-width: 70px; horizontal-stretch: 0; text <=> root.height-text; }
            }
            spacer55 := Rectangle { horizontal-stretch: 55; }
        }
    }
}

#[test]
fn resolution_row_aligns_width_with_control_column_and_height_at_45pct() {
    i_slint_backend_testing::init_no_event_loop();

    let win = ResRowTest::new().expect("failed to create ResRowTest");
    win.set_width_text("1920".into());
    win.set_height_text("1080".into());
    win.show();

    // 1. 输入框宽度：min-width 70px 生效，宽度/高度框等宽
    let wedit_w = win.get_dbg_wedit_w();
    eprintln!("width edit box = {wedit_w}px");
    assert!(
        (wedit_w - 70.0).abs() <= 1.0,
        "min-width override failed: width edit rendered {wedit_w}px, expected 70px"
    );
    assert_eq!(wedit_w, win.get_dbg_hedit_w(), "width/height boxes must be equal width");

    // 2. 「宽度」组与上面输入框起始列对齐：reslabel 右缘 + 外层 4px 间距（与其他行一致）
    let reslabel_right = win.get_dbg_reslabel_x() + win.get_dbg_reslabel_w();
    let align_offset = win.get_dbg_wgroup_x() - reslabel_right;
    eprintln!("width group offset from label column = {align_offset}px");
    assert!(
        (align_offset - 4.0).abs() <= 1.0,
        "width group not aligned with control column (offset={align_offset}px, expected 4px spacing)"
    );

    // 3. caption 与输入框间隙：edit.x - (label.x + label.width)，应为组内 3px
    let gap_w = win.get_dbg_wedit_x() - (win.get_dbg_wlabel_x() + win.get_dbg_wlabel_w());
    let gap_h = win.get_dbg_hedit_x() - (win.get_dbg_hlabel_x() + win.get_dbg_hlabel_w());
    eprintln!("caption→box gap: width={gap_w}px height={gap_h}px");
    assert!((gap_w - 3.0).abs() <= 1.0, "width caption not hugging its box (gap={gap_w}px)");
    assert!((gap_h - 3.0).abs() <= 1.0, "height caption not hugging its box (gap={gap_h}px)");

    // 4. 高度组放在剩余空间约 45% 处：spacer45 / (spacer45 + spacer55) ≈ 0.45
    let s45 = win.get_dbg_spacer45_w();
    let s55 = win.get_dbg_spacer55_w();
    let ratio = s45 / (s45 + s55);
    eprintln!("height group position: spacer45={s45}px spacer55={s55}px ratio={ratio}");
    assert!(
        (ratio - 0.45).abs() <= 0.03,
        "height group not at ~45% of remaining space (ratio={ratio}, spacer45={s45}, spacer55={s55})"
    );

    win.hide();
}
