# -*- coding: utf-8 -*-
"""x264video4osu 主应用图标源（生成脚本）。

当前图标 = 「Frutiger-Aero 导演场记板 · osu 命中圈三角簇」：深色玻璃场记板，
顶部经典 45° 斜条纹色带 + 斜向玻璃光泽；板上嵌入白色命中圈 + aqua 玻璃内盘，
盘内为“全部尖朝上、方向一致、大小不一、部分层叠”的白色正三角形簇
（仿 osu 白色三角形皮肤动效）。

本脚本是图标设计的唯一事实来源：所有几何/配色/超采样参数都在这里，
`assets/app_icon.ico` 与 `icon_work/clapper_hitcircle_256.png` 都由它导出。
运行 `python icon_render_v2.py` 重新生成：
  assets/app_icon.ico               —— 构建嵌入用（app_icon.rc 引用，build.rs 打进 exe）
  icon_work/clapper_hitcircle_256.png —— 256 主成图（预览/文档用，与 ico 的 256 帧同源）

依赖：Python + Pillow。`.ico` 是提交在库里的构建快照；本脚本用于在需要
调整设计时（改色/改几何/补尺寸档）可复现地再次产出它。
"""
import math
import os
from PIL import Image, ImageDraw, ImageFilter

S = 6
CANVAS = 256
C = CANVAS / 2
SIZE = CANVAS * S
os.makedirs("assets", exist_ok=True)
os.makedirs("icon_work", exist_ok=True)

WHITE = (255, 255, 255, 255)
# ---- 场记板（方案：导演场记板） ----
CHAR_TOP = (64, 72, 85, 255)
CHAR_BOT = (10, 14, 20, 255)
BAR_LIGHT = (244, 248, 252, 255)
BAR_DARK = (16, 21, 29, 255)
# ---- 命中圈内盘 ----
DISC_TOP = (158, 220, 250, 255)
DISC_BOT = (28, 132, 206, 255)
# ---- 三角形 ----
TRI_TOP = (255, 255, 255, 255)
TRI_BOT = (196, 212, 226, 255)
SHADOW = (0, 40, 76, 80)


def P(x, y):
    return int(round(x * S)), int(round(y * S))


def layer():
    return Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))


def mix(c1, c2, t):
    return tuple(int(c1[i] + (c2[i] - c1[i]) * t) for i in range(4))


def mask_ring(r_out, r_in, cx=C, cy=C):
    m = Image.new("L", (SIZE, SIZE), 0)
    d = ImageDraw.Draw(m)
    d.ellipse([P(cx - r_out, cy - r_out), P(cx + r_out, cy + r_out)], fill=255)
    d.ellipse([P(cx - r_in, cy - r_in), P(cx + r_in, cy + r_in)], fill=0)
    return m


def mask_disk(r, cx=C, cy=C):
    m = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(m).ellipse([P(cx - r, cy - r), P(cx + r, cy + r)], fill=255)
    return m


def mask_poly(xs, ys):
    m = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(m).polygon([P(xs[i], ys[i]) for i in range(len(xs))], fill=255)
    return m


def mask_round_rect(x0, y0, x1, y1, r):
    m = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(m).rounded_rectangle(
        [P(x0, y0), P(x1, y1)], radius=int(r * S), fill=255)
    return m


def fill_grad(dst, m, c1, c2, direction="v"):
    bbox = m.getbbox()
    if not bbox:
        return
    x0, y0, x1, y1 = bbox
    w, h = x1 - x0, y1 - y0
    g = Image.new("RGBA", (w, h))
    d = ImageDraw.Draw(g)
    if direction == "v":
        for yy in range(h):
            d.line([(0, yy), (w, yy)], fill=mix(c1, c2, yy / max(1, h - 1)))
    else:
        for xx in range(w):
            d.line([(xx, 0), (xx, h)], fill=mix(c1, c2, xx / max(1, w - 1)))
    dst.paste(g, (x0, y0), m.crop(bbox))


def paint(dst, m, color):
    dst.paste(Image.new("RGBA", (SIZE, SIZE), color), (0, 0), m)


def shadow_layer(m, dx, dy, radius):
    out = layer()
    out.paste(Image.new("RGBA", (SIZE, SIZE), SHADOW), (0, 0), m)
    out = out.filter(ImageFilter.GaussianBlur(radius * S))
    res = layer()
    res.paste(out, (int(dx * S), int(dy * S)), out)
    return res


def alpha_mask_layer(shape_mask, color, blur_px=0):
    """把 'L' 形状 mask 变成指定颜色 + 羽化的 RGBA 图层。"""
    im = Image.new("RGBA", (SIZE, SIZE), color)
    res = layer()
    res.paste(im, (0, 0), shape_mask)
    if blur_px:
        res = res.filter(ImageFilter.GaussianBlur(blur_px * S))
    return res


def diagonal_stripes(bx0, by0, bx1, by1, period, col_a, col_b, phase=0):
    """在 (bx0,by0,bx1,by1) 区域内生成 45° 斜向两色条纹（最终像素单位）。
    返回 (stripes_layer_fullsize, band_mask)。band_mask 由调用方决定裁剪。"""
    bw, bh = int((bx1 - bx0) * S), int((by1 - by0) * S)
    ppx = max(1, int(round(period * S)))
    im = Image.new("RGB", (bw, bh), col_a[:3])
    px = im.load()
    for yy in range(bh):
        for xx in range(bw):
            if ((xx + yy + int(phase * S)) // ppx) % 2:
                px[xx, yy] = col_b[:3]
    layer_rgba = im.convert("RGBA")
    return layer_rgba, (int(bx0 * S), int(by0 * S))


def draw_triangle(base, apex, side, c_top=TRI_TOP, c_bot=TRI_BOT, shadow=True):
    """画一个尖朝上的正三角形。apex=(ax,ay) 为顶角，side 为边长（最终像素）。"""
    ax, ay = apex
    h = side * math.sqrt(3) / 2.0
    x0, x1, yb = ax - side / 2.0, ax + side / 2.0, ay + h
    tri = mask_poly([x0, x1, ax], [yb, yb, ay])
    if shadow:
        base.alpha_composite(shadow_layer(tri, 0, 2.5, 1.6))
    fill_grad(base, tri, c_top, c_bot, "v")
    return tri


def gloss_band(layer_out, poly_pts, alpha=22, blur=26):
    """整画布斜向大面积玻璃光泽（alpha_mask_layer 版本）。"""
    m = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(m).polygon([P(x, y) for x, y in poly_pts], fill=255)
    sh = alpha_mask_layer(m, (255, 255, 255, alpha), blur)
    layer_out.alpha_composite(sh)


# ===============================================================
# 主设计：Frutiger-Aero 导演场记板 · 命中圈三角簇
# ===============================================================
def design_clapper():
    base = layer()

    # ---- 场记板（深色玻璃板） ----
    X0, Y0, X1, Y1 = 24, 66, 232, 248      # 板外框
    R = 18
    board = mask_round_rect(X0, Y0, X1, Y1, R)
    fill_grad(base, board, CHAR_TOP, CHAR_BOT, "v")

    # 板体边缘微光
    rim = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(rim).rounded_rectangle([P(X0, Y0), P(X1, Y1)], radius=int(R * S),
                                          outline=255, width=int(1.4 * S))
    base.alpha_composite(alpha_mask_layer(rim, (255, 255, 255, 52), 0))

    # ---- 顶部斜条纹色带（经典场记板色条） ----
    bar_h = 40
    bx0, by0, bx1 = X0, Y0, X1
    by1 = Y0 + bar_h
    stripes, off = diagonal_stripes(bx0, by0, bx1, by1, 26, BAR_LIGHT, BAR_DARK)
    band_local = board.crop((int(bx0 * S), int(by0 * S), int(bx1 * S), int(by1 * S)))
    board_patch = layer()
    board_patch.paste(stripes, off, band_local)
    base.alpha_composite(board_patch)

    # 色带下缘一条细分隔高光
    sep = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(sep).line([P(X0 + 2, by1), P(X1 - 2, by1)], fill=255, width=int(1 * S))
    base.alpha_composite(alpha_mask_layer(sep, (255, 255, 255, 70), 0))

    # ---- 板体斜向大面积玻璃光泽（Frutiger-Aero） ----
    gloss_band(base, [(X0 - 46, Y0 - 30), (X0 + 70, Y0 - 30), (X0 - 8, Y1 + 34),
                      (X0 - 90, Y1 + 30)], alpha=20, blur=30)

    # ---- 命中圈（白色圆环 + aqua 内盘），中心约 (128, 170) ----
    cx, cy, r_out, r_in = 128, 170, 62, 49
    fill_grad(base, mask_disk(r_in, cx, cy), DISC_TOP, DISC_BOT, "v")
    paint(base, mask_ring(r_out, r_in, cx, cy), WHITE)

    # ---- 内盘三角形簇（尖朝上、大小不同、部分层叠） ----
    tris = [
        ((128, 138), 40),   # 大（中上，后）
        ((150, 134), 17),   # 小（右上，叠在大上）
        ((106, 172), 26),   # 中（左下）
        ((152, 182), 20),   # 小（右下）
    ]
    for apex, side in tris:
        draw_triangle(base, apex, side)

    # ---- 内盘顶部极淡玻璃高光 ----
    sheen = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(sheen).arc(
        [P(cx - 46, cy - 46), P(cx + 46, cy + 46)], 200, 340,
        fill=70, width=int(3.2 * S))
    sh = Image.new("RGBA", (SIZE, SIZE), (255, 255, 255, 255))
    gloss = layer()
    gloss.paste(sh, (0, 0), sheen)
    gloss = gloss.filter(ImageFilter.GaussianBlur(int(1.6 * S)))
    base.alpha_composite(gloss)
    return base


def down(im, size):
    return im.resize((size, size), Image.LANCZOS)


def export_ico(im, path):
    # 重要：直接用主图 + sizes 列表，Pillow 会自动为每个尺寸下采样。
    # 不要用 append_images —— 本 Pillow 版本下 append 只会产出 16x16。
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    im.save(path, format="ICO", sizes=[(s, s) for s in sizes])


def main():
    art = design_clapper()
    export_ico(art, "assets/app_icon.ico")
    down(art, 256).save("icon_work/clapper_hitcircle_256.png")
    chk = Image.open("assets/app_icon.ico")
    print("assets/app_icon.ico sizes:", sorted(chk.info.get("sizes", [])))


if __name__ == "__main__":
    main()
