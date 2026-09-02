//! 无头几何测试：编码方法单选行的排版 —— 包装组件（MethodRadioRow 结构）
//! 必须与「直接放进 HorizontalBox」一样：紧贴 90px 标签列之后、保持紧凑宽度
//! 靠左，而不是被拉伸/居中到窗口中间。
//!
//! 背景：MethodRadioRow 把 RadioGroup 包在隐式根组件里以容纳 Tooltip（Tooltip
//! 不能直接放进布局）。实测发现：
//! - 裸 Rectangle 隐式根默认 stretch=1，会被布局撑满整行 → RadioGroup 内部
//!   GridLayout 把两个单选按钮居中到窗口中间；
//! - 只设 `horizontal-stretch: 0` 不够（preferred 仍是 Rectangle 默认）；
//! - 用 HorizontalLayout 包裹 RadioGroup + 根 `horizontal-stretch: 0` 才紧凑
//!   （HorizontalLayout 提供内容尺寸，stretch=0 阻止分配富余空间）。

#![cfg(test)]

use slint::ComponentHandle;

slint::slint! {
    import { RadioGroup, HorizontalBox } from "std-widgets.slint";

    component MethodRow {
        in property <string> tip;
        callback selected(string);

        // 显式把根尺寸绑到 RadioGroup 的内容尺寸：隐式根是裸 Rectangle，
        // preferred=0 / stretch=1，会被布局撑满整行导致单选按钮居中。
        width: rg.min-width;
        height: rg.min-height;

        rg := RadioGroup {
            orientation: horizontal;
            selected(text) => { root.selected(text); }
            RadioButton { text: "CRF"; checked: true; }
            RadioButton { text: "2pass"; }
        }

        if (root.tip != ""): Tooltip {
            Rectangle {
                background: #FFFFFF;
                border-width: 1px;
                border-color: #C0C0C0;
                border-radius: 4px;
                HorizontalLayout {
                    padding-left: 9px; padding-right: 9px; padding-top: 6px; padding-bottom: 6px;
                    Text { text: root.tip; font-size: 12px; color: #333333; }
                }
            }
        }
    }

    export component MethodRowTest inherits Window {
        width: 572px;
        height: 120px;

        out property <length> dbg-label-x: label.x;
        out property <length> dbg-label-w: label.width;
        out property <length> dbg-row-x: row.x;
        out property <length> dbg-row-w: row.width;
        out property <length> dbg-row-minw: row.min-width;
        out property <length> dbg-row-h: row.height;

        HorizontalBox {
            spacing: 4px;
            label := Text { text: "编码方法"; width: 90px; vertical-alignment: center; }
            row := MethodRow { tip: "test"; }
        }
    }
}

#[test]
fn method_row_stays_left_aligned_and_compact() {
    i_slint_backend_testing::init_no_event_loop();

    let win = MethodRowTest::new().expect("failed to create MethodRowTest");
    win.show();

    let label_right = win.get_dbg_label_x() + win.get_dbg_label_w();
    let row_x = win.get_dbg_row_x();
    let row_w = win.get_dbg_row_w();
    eprintln!("label right={label_right} | row x={row_x} w={row_w} h={} minw={}", win.get_dbg_row_h(), win.get_dbg_row_minw());

    // 1. 行的 x 紧贴标签列（90px + 4px 间距），与其它横行的输入列对齐
    let offset = row_x - label_right;
    eprintln!("row offset from label column = {offset}px");
    assert!(
        (offset - 4.0).abs() <= 1.0,
        "row not aligned right after label column (offset={offset}px, expected 4px)"
    );

    // 2. 行宽保持紧凑（内容宽 ~146px），明显小于剩余整行宽度 ~470px
    assert!(
        row_w < 300.0,
        "row was stretched across the window ({row_w}px); radios get centered by the grid"
    );

    win.hide();
}
