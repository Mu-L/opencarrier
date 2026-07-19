#!/usr/bin/env python3
"""86巴士班次时刻表海报生成器（Pillow 精确排版，对齐官方参考图风格）。

用法:
  python3 gen_poster.py --out output/poster.png
  python3 gen_poster.py --config poster_config.json --out output/poster.png

config JSON 示例见同目录 flow.md。不要用 AI 文生图做时刻表正文——文字会糊。
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from typing import Any

from PIL import Image, ImageDraw, ImageFont

# --- Brand palette (from official 86 参考海报) ---
BG = (245, 248, 255)
WHITE = (255, 255, 255)
BLUE = (47, 107, 230)
BLUE_DARK = (28, 64, 150)
BLUE_MID = (90, 140, 240)
BLUE_SOFT = (190, 215, 255)
BLUE_PALE = (225, 236, 255)
GREEN = (72, 196, 120)
RED = (255, 95, 100)
ORANGE = (255, 145, 50)
GRAY = (90, 100, 130)
GRAY_LIGHT = (150, 160, 180)
WATERMARK = (232, 238, 250)

DEFAULT_CONFIG: dict[str, Any] = {
    "brand": "86巴士",
    "title": "新线试运行时刻表",
    "route": "百度广州通勤专线 · 市桥 往返 百度广州",
    "badge": "NEW",
    "footer": "票价 ¥10/人  ·  满40/38人发车  ·  86巴士小程序购票",
    "disclaimer": "*到站时间可能因路况或天气有变化，仅供参考",
    "qr_label": "长按识别二维码\n立即购票",
    "qr_path": "",
    "outbound": {
        "label": "去程",
        "time": "07:35",
        "note": "发车",
        "stops": [
            {"name": "市桥地铁口", "type": "board", "time": "07:35"},
            {"name": "沙头新村", "type": "board", "time": "07:40"},
            {"name": "百度广州公司", "type": "alight", "time": ""},
        ],
    },
    "inbound": {
        "label": "回程",
        "time": "18:30",
        "note": "发车",
        "stops": [
            {"name": "百度广州公司", "type": "board", "time": "18:30"},
            {"name": "沙头新村", "type": "alight", "time": ""},
            {"name": "市桥地铁口", "type": "alight", "time": ""},
        ],
    },
    "width": 1080,
    "height": 1680,
}


def load_font(size: int, bold: bool = False) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    candidates = (
        [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
        ]
        if bold
        else [
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ]
    )
    for path in candidates:
        if os.path.exists(path):
            try:
                return ImageFont.truetype(path, size, index=0)
            except OSError:
                continue
    return ImageFont.load_default()


def text_size(draw: ImageDraw.ImageDraw, text: str, font: ImageFont.ImageFont) -> tuple[int, int]:
    box = draw.textbbox((0, 0), text, font=font)
    return box[2] - box[0], box[3] - box[1]


def draw_header(draw: ImageDraw.ImageDraw, w: int, h: int) -> None:
    # Soft blue blobs like official poster
    draw.ellipse((-120, -280, int(w * 0.85), 380), fill=BLUE_SOFT)
    draw.ellipse((int(w * 0.35), -200, w + 220, 420), fill=(200, 222, 255))
    draw.ellipse((int(w * 0.55), 40, w + 180, 360), fill=(215, 230, 255))


def draw_watermarks(draw: ImageDraw.ImageDraw, w: int, h: int, brand: str) -> None:
    f = load_font(42, True)
    for y in range(420, h - 200, 160):
        for x in range(40, w - 80, 220):
            draw.text((x, y), brand, fill=WATERMARK, font=f)


def draw_column(
    draw: ImageDraw.ImageDraw,
    x: int,
    y: int,
    col_w: int,
    col: dict[str, Any],
    label_color: tuple[int, int, int],
) -> int:
    """Draw one direction column. Returns bottom y."""
    f_label = load_font(36, True)
    f_time = load_font(40, True)
    f_note = load_font(28)
    f_stop_time = load_font(34, True)
    f_stop = load_font(30)

    label = str(col.get("label", "去程"))
    time_s = str(col.get("time", "")).strip()
    note = str(col.get("note", "发车")).strip()
    stops = col.get("stops") or []

    # header pill
    draw.rounded_rectangle((x, y, x + col_w, y + 64), radius=22, fill=BLUE_PALE)
    draw.text((x + 22, y + 14), label, fill=label_color, font=f_label)
    if time_s:
        tw, _ = text_size(draw, label, f_label)
        draw.text((x + 22 + tw + 16, y + 12), time_s, fill=BLUE_DARK, font=f_time)
        tw2, _ = text_size(draw, time_s, f_time)
        draw.text((x + 22 + tw + 16 + tw2 + 8, y + 18), note, fill=GRAY, font=f_note)

    line_x = x + 30
    top = y + 100
    step = 88
    n = max(len(stops), 1)
    bottom = top + (n - 1) * step
    if n > 1:
        draw.line((line_x, top, line_x, bottom), fill=BLUE_SOFT, width=5)

    for i, stop in enumerate(stops):
        cy = top + i * step
        kind = str(stop.get("type", "board")).lower()
        color = GREEN if kind in ("board", "up", "上车", "green") else RED
        name = str(stop.get("name", ""))
        st = str(stop.get("time", "") or "").strip()

        draw.ellipse((line_x - 13, cy - 13, line_x + 13, cy + 13), fill=color)
        tx = line_x + 32
        if st:
            draw.text((tx, cy - 30), st, fill=BLUE_DARK, font=f_stop_time)
            draw.text((tx, cy + 8), name, fill=GRAY, font=f_stop)
        else:
            draw.text((tx, cy - 14), name, fill=BLUE_DARK, font=f_stop)

    return bottom + 40


def try_load_qr(path: str, size: int = 150) -> Image.Image | None:
    if not path or not os.path.isfile(path):
        return None
    try:
        qr = Image.open(path).convert("RGBA")
        qr = qr.resize((size, size), Image.Resampling.LANCZOS)
        return qr
    except OSError:
        return None


def make_qr_placeholder(size: int = 150) -> Image.Image:
    """Simple patterned QR-like placeholder if no real QR provided."""
    img = Image.new("RGB", (size, size), WHITE)
    d = ImageDraw.Draw(img)
    d.rectangle((0, 0, size - 1, size - 1), outline=BLUE_SOFT, width=3)
    cell = max(size // 15, 4)
    for i in range(2, size - 2, cell):
        for j in range(2, size - 2, cell):
            if (i // cell + j // cell) % 3 == 0:
                d.rectangle((i, j, i + cell - 2, j + cell - 2), fill=BLUE_DARK)
    # finder patterns
    for ox, oy in ((8, 8), (size - 48, 8), (8, size - 48)):
        d.rectangle((ox, oy, ox + 36, oy + 36), outline=BLUE_DARK, width=4)
        d.rectangle((ox + 10, oy + 10, ox + 26, oy + 26), fill=BLUE_DARK)
    return img


def render(cfg: dict[str, Any]) -> Image.Image:
    w = int(cfg.get("width") or 1080)
    h = int(cfg.get("height") or 1680)
    img = Image.new("RGB", (w, h), BG)
    draw = ImageDraw.Draw(img)

    draw_header(draw, w, h)
    draw_watermarks(draw, w, h, str(cfg.get("brand") or "86巴士"))

    f_logo = load_font(34, True)
    f_title = load_font(68, True)
    f_route = load_font(32, True)
    f_footer = load_font(26, True)
    f_small = load_font(24)
    f_tiny = load_font(20)

    brand = str(cfg.get("brand") or "86巴士")
    title = str(cfg.get("title") or "新线试运行时刻表")
    route = str(cfg.get("route") or "")
    badge = str(cfg.get("badge") or "").strip()

    draw.text((48, 44), brand, fill=BLUE_DARK, font=f_logo)
    draw.text((50, 112), title, fill=BLUE_MID, font=f_title)
    draw.text((48, 110), title, fill=BLUE, font=f_title)
    if route:
        draw.text((48, 200), route, fill=BLUE_DARK, font=f_route)

    if badge:
        bw, bh = text_size(draw, badge, load_font(28, True))
        bx, by = 48, 260
        draw.rounded_rectangle((bx, by, bx + bw + 36, by + 44), radius=12, fill=ORANGE)
        draw.text((bx + 18, by + 8), badge, fill=WHITE, font=load_font(28, True))

    # columns
    margin = 48
    gap = 36
    col_w = (w - margin * 2 - gap) // 2
    left_x = margin
    right_x = margin + col_w + gap
    y0 = 340 if badge else 300

    outbound = cfg.get("outbound") or {}
    inbound = cfg.get("inbound") or {}
    y1 = draw_column(draw, left_x, y0, col_w, outbound, GREEN)
    y2 = draw_column(draw, right_x, y0, col_w, inbound, RED)
    content_bottom = max(y1, y2)

    # footer bar
    footer = str(cfg.get("footer") or "")
    fy = min(max(content_bottom + 40, h - 300), h - 280)
    draw.rounded_rectangle((margin, fy, w - margin, fy + 64), radius=18, fill=BLUE)
    if footer:
        fw, _ = text_size(draw, footer, f_footer)
        draw.text(((w - fw) // 2, fy + 18), footer, fill=WHITE, font=f_footer)

    # legend
    ly = fy + 90
    draw.ellipse((margin + 8, ly, margin + 32, ly + 24), fill=GREEN)
    draw.text((margin + 42, ly), "上车点", fill=GRAY, font=f_small)
    draw.ellipse((margin + 160, ly, margin + 184, ly + 24), fill=RED)
    draw.text((margin + 194, ly), "下车点", fill=GRAY, font=f_small)

    disclaimer = str(cfg.get("disclaimer") or "")
    if disclaimer:
        draw.text((margin, ly + 40), disclaimer, fill=GRAY_LIGHT, font=f_tiny)

    # QR block bottom-right
    qr_size = 150
    qx = w - margin - qr_size - 20
    qy = h - margin - qr_size - 50
    # keep clear of footer
    if qy < fy + 80:
        qy = fy + 90

    draw.rounded_rectangle(
        (qx - 16, qy - 16, qx + qr_size + 16, qy + qr_size + 50),
        radius=20,
        fill=WHITE,
        outline=BLUE_SOFT,
        width=2,
    )
    qr_img = try_load_qr(str(cfg.get("qr_path") or ""), qr_size) or make_qr_placeholder(qr_size)
    if qr_img.mode == "RGBA":
        img.paste(qr_img, (qx, qy), qr_img)
    else:
        img.paste(qr_img, (qx, qy))

    qr_label = str(cfg.get("qr_label") or "扫码购票")
    # multi-line label under QR
    ly2 = qy + qr_size + 6
    for i, line in enumerate(qr_label.split("\n")[:2]):
        lw, _ = text_size(draw, line, f_tiny)
        draw.text((qx + (qr_size - lw) // 2, ly2 + i * 22), line, fill=BLUE_DARK, font=f_tiny)

    return img


def main() -> int:
    ap = argparse.ArgumentParser(description="86巴士时刻表海报生成")
    ap.add_argument("--config", help="JSON 配置路径")
    ap.add_argument("--out", required=True, help="输出 PNG 路径")
    args = ap.parse_args()

    cfg = dict(DEFAULT_CONFIG)
    if args.config:
        with open(args.config, encoding="utf-8") as f:
            user = json.load(f)
        # shallow + one-level merge for outbound/inbound
        for k, v in user.items():
            if k in ("outbound", "inbound") and isinstance(v, dict):
                base = dict(cfg.get(k) or {})
                base.update(v)
                cfg[k] = base
            else:
                cfg[k] = v

    out = args.out
    os.makedirs(os.path.dirname(os.path.abspath(out)) or ".", exist_ok=True)
    img = render(cfg)
    img.save(out, format="PNG", optimize=True)
    print(json.dumps({"ok": True, "saved_to": out, "size": os.path.getsize(out)}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
