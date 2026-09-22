#!/usr/bin/env python3
"""生成 assets/voicetype.ico（Windows exe / 安装包图标）与 icon-256.png。

设计源：assets/icon.svg（麦克风 + 声波，靛紫渐变圆角底）。
本脚本按同一几何参数用 PIL 直接绘制（4x 超采样后 LANCZOS 缩小，平滑抗锯齿），
避免依赖系统 SVG 光栅化器；改动图标时同步改 icon.svg 与本文件的 GEOMETRY。
"""
from PIL import Image, ImageDraw

# ---- 几何（256 设计稿坐标，运行时统一 ×S）----
BG_RADIUS = 56
CAPSULE = (104, 56, 152, 144, 24)          # 拾音头
U_CENTER_R = ((128, 120), 44, 12)          # U 型支架：圆心/半径/线宽
U_STUB_Y = (108, 121)                      # 支架竖边
STEM = (122, 160, 134, 186, 6)             # 杆
BASE = (98, 188, 158, 200, 6)              # 底座
WAVES = [                                  # (端点x, 圆心x, 半径, 半张角deg, 不透明度)
    (200, 162.3, 52, 43.7, 0.85),
    (222, 164.9, 76, 41.2, 0.45),
    (56, 93.7, 52, 43.7, 0.85),
    (34, 91.1, 76, 41.2, 0.45),
]
GRAD_TOP = (0x63, 0x66, 0xF1)
GRAD_BOTTOM = (0xA8, 0x55, 0xF7)
WHITE = (255, 255, 255, 255)
SIZES = [16, 24, 32, 48, 64, 128, 256]
S = 4  # 超采样倍率


def build() -> Image.Image:
    size = 256 * S
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    # 渐变圆角底
    grad = Image.new("RGB", (size, size))
    px = grad.load()
    for y in range(size):
        t = y / (size - 1)
        color = tuple(round(a + (b - a) * t) for a, b in zip(GRAD_TOP, GRAD_BOTTOM))
        for x in range(size):
            px[x, y] = color
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [8 * S, 8 * S, 248 * S, 248 * S], radius=BG_RADIUS * S, fill=255
    )
    canvas.paste(grad, (0, 0), mask)

    # 白色主体（先画在透明层，再叠加，支持声波半透明）
    art = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(art)

    d.rounded_rectangle(
        [CAPSULE[0] * S, CAPSULE[1] * S, CAPSULE[2] * S, CAPSULE[3] * S],
        radius=CAPSULE[4] * S, fill=WHITE,
    )
    cx, cy = U_CENTER_R[0][0] * S, U_CENTER_R[0][1] * S
    r = U_CENTER_R[1] * S
    d.arc([cx - r, cy - r, cx + r, cy + r], start=0, end=180,
          fill=WHITE, width=U_CENTER_R[2] * S)
    w = U_CENTER_R[2] * S // 2
    d.rectangle([cx - r - w, U_STUB_Y[0] * S, cx - r + w, U_STUB_Y[1] * S], fill=WHITE)
    d.rectangle([cx + r - w, U_STUB_Y[0] * S, cx + r + w, U_STUB_Y[1] * S], fill=WHITE)
    d.rounded_rectangle([STEM[0] * S, STEM[1] * S, STEM[2] * S, STEM[3] * S],
                        radius=STEM[4] * S, fill=WHITE)
    d.rounded_rectangle([BASE[0] * S, BASE[1] * S, BASE[2] * S, BASE[3] * S],
                        radius=BASE[4] * S, fill=WHITE)
    for ex, ccx, radius, half, alpha in WAVES:
        c_x, c_y = ccx * S, 128 * S
        rr = radius * S
        d.arc([c_x - rr, c_y - rr, c_x + rr, c_y + rr],
              start=-half if ex > 128 else 180 - half,
              end=half if ex > 128 else 180 + half,
              fill=(255, 255, 255, round(alpha * 255)), width=10 * S)

    canvas.alpha_composite(art)
    return canvas


def main() -> None:
    master = build()
    base = master.resize((256, 256), Image.LANCZOS)
    base.save("assets/icon-256.png")
    base.save(
        "assets/voicetype.ico",
        sizes=[(s, s) for s in SIZES],
    )
    print("生成 assets/voicetype.ico 与 assets/icon-256.png")


if __name__ == "__main__":
    main()
