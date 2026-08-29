"""
生成 ClipSync 应用图标源图 (1024x1024) —— 与托盘图标同视觉:
  蓝紫对角渐变 + 圆角方块 + 白色同步环 (refresh) 双箭头
输出: client/app-icon.png (供 `npm run tauri -- icon` 生成全套 .ico/.icns/png)
"""
import math
from PIL import Image, ImageDraw

W = 1024
BLUE = (0x5B, 0x6C, 0xFF)    # 左上
PURPLE = (0xA6, 0x6B, 0xFF)  # 右下
WHITE = (255, 255, 255)
RADIUS = 200

# 1) 对角渐变底 (蓝→紫, 沿 x+y)
grad = Image.new("RGB", (W, W))
px = grad.load()
denom = 2 * (W - 1)
for y in range(W):
    for x in range(W):
        t = (x + y) / denom
        r = int(BLUE[0] + (PURPLE[0] - BLUE[0]) * t)
        g = int(BLUE[1] + (PURPLE[1] - BLUE[1]) * t)
        b = int(BLUE[2] + (PURPLE[2] - BLUE[2]) * t)
        px[x, y] = (r, g, b)

# 2) 圆角矩形 mask
mask = Image.new("L", (W, W), 0)
ImageDraw.Draw(mask).rounded_rectangle((0, 0, W - 1, W - 1), radius=RADIUS, fill=255)

# 3) 合成背景
bg = Image.new("RGBA", (W, W), (0, 0, 0, 0))
bg.paste(grad, (0, 0), mask)

# 4) 白色同步环 overlay —— 用"外圈+内圈"多边形 fill, 边缘完全平滑无锯齿
overlay = Image.new("RGBA", (W, W), (0, 0, 0, 0))
d2 = ImageDraw.Draw(overlay)
cx, cy = W // 2, W // 2
R = 320          # 环中心半径
THICK = 64       # 环填充厚度
STEP = 0.35      # 角度步进 (度), 越小越平滑


def thick_arc_poly(r, thick, a_start, a_end):
    """外圈顺时针 + 内圈逆时针 围成"环段"多边形.
    屏幕坐标: 0°=3 点钟, 顺时针增加 (cos=x, sin=y, y 向下为正)."""
    R_o, R_i = r + thick / 2.0, r - thick / 2.0
    outer, inner = [], []
    a = a_start
    while a <= a_end + 1e-6:
        rad = math.radians(a)
        outer.append((cx + R_o * math.cos(rad), cy + R_o * math.sin(rad)))
        a += STEP
    a = a_end
    while a >= a_start - 1e-6:
        rad = math.radians(a)
        inner.append((cx + R_i * math.cos(rad), cy + R_i * math.sin(rad)))
        a -= STEP
    return outer + inner


# 上半弧: 200°(左下) → 270°(屏幕上方) → 340°(右下)
d2.polygon(thick_arc_poly(R, THICK, 200, 340), fill=WHITE + (255,))
# 下半弧: 20°(右上) → 90°(屏幕下方) → 160°(左下)
d2.polygon(thick_arc_poly(R, THICK, 20, 160), fill=WHITE + (255,))


def arrow(end_deg, size=140):
    """在 end_deg 角度处画一个朝切向 (顺时针 +90°) 的三角箭头."""
    tip_rad = math.radians(end_deg)
    tip = (cx + R * math.cos(tip_rad), cy + R * math.sin(tip_rad))
    tang_deg = end_deg + 90
    tang = math.radians(tang_deg)
    extend = size * 0.55
    tip_ext = (tip[0] + extend * math.cos(tang), tip[1] + extend * math.sin(tang))
    back = -size * 0.15
    base = (tip[0] + back * math.cos(tang), tip[1] + back * math.sin(tang))
    perp1 = math.radians(tang_deg - 90)
    perp2 = math.radians(tang_deg + 90)
    w = size * 0.7
    p1 = (base[0] + w * math.cos(perp1), base[1] + w * math.sin(perp1))
    p2 = (base[0] + w * math.cos(perp2), base[1] + w * math.sin(perp2))
    return [tip_ext, p1, p2]


# 上弧末端 340° 切向 70°(屏幕右下) ; 下弧末端 160° 切向 250°(屏幕左上)
d2.polygon(arrow(340), fill=WHITE + (255,))
d2.polygon(arrow(160), fill=WHITE + (255,))

# 5) 合成并保存
final = Image.alpha_composite(bg, overlay)
final.save("app-icon.png")
print("saved app-icon.png", final.size, final.mode)
