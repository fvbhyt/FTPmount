"""生成应用图标（纯标准库，无需 Pillow）。

!!! 现已改用用户提供的正式图标（见 desktop/src-tauri/icons/ 下的各尺寸 PNG）。
!!! 本脚本只作为「没有设计稿时」的占位方案保留，正常不需要再跑。
!!! 本脚本只作为「没有设计稿时」的占位方案保留，正常不需要再跑。

设计：圆角矩形 + 对角渐变（#3b82f6 → #8b5cf6）+ 中央白色云朵。
做法：先在 1024×1024 上按 4× 超采样画，再盒式降采样到各目标尺寸，
边缘才有抗锯齿，不会像直接按 64×64 硬画那样有锯齿。

输出（Tauri 打包要求）：
  icons/32x32.png
  icons/128x128.png
  icons/128x128@2x.png   (256)
  icons/icon.png         (512)
  icons/icon.ico         (16/32/48/64/128/256 多尺寸，内嵌 PNG)
"""

import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "desktop", "src-tauri", "icons")

# 设计稿尺寸（逻辑坐标），实际按 SS 倍超采样
BASE = 256
SS = 4
W = BASE * SS
R = 58 * SS  # 圆角半径

# 渐变端点
C0 = (59, 130, 246)    # #3b82f6
C1 = (139, 92, 246)    # #8b5cf6

# 云朵（逻辑坐标 0..256）：三个圆 + 底部圆角矩形
CLOUD = [(100, 158, 38), (160, 158, 38), (130, 128, 46)]
BOTTOM = (64, 156, 196, 196)  # x0, y0, x1, y1


def s(v):
    return v * SS


def inside_round_rect(x, y):
    cx = min(max(x, R), W - 1 - R)
    cy = min(max(y, R), W - 1 - R)
    dx, dy = x - cx, y - cy
    return dx * dx + dy * dy <= R * R


def inside_cloud(x, y):
    for cx, cy, r in CLOUD:
        cx, cy, r = s(cx), s(cy), s(r)
        if (x - cx) ** 2 + (y - cy) ** 2 <= r * r:
            return True
    x0, y0, x1, y1 = (s(v) for v in BOTTOM)
    # 底部矩形两端也做成圆角
    rr = (y1 - y0) / 2
    if x0 + rr <= x <= x1 - rr and y0 <= y <= y1:
        return True
    for ccx in (x0 + rr, x1 - rr):
        if (x - ccx) ** 2 + (y - (y0 + y1) / 2) ** 2 <= rr * rr:
            return True
    return False


def render():
    """返回 (r,g,b,a) 的二维列表，尺寸 W×W。"""
    px = [[(0, 0, 0, 0)] * W for _ in range(W)]
    for y in range(W):
        row = px[y]
        for x in range(W):
            if not inside_round_rect(x, y):
                continue
            t = (x + y) / (2 * (W - 1))
            r = int(C0[0] + (C1[0] - C0[0]) * t)
            g = int(C0[1] + (C1[1] - C0[1]) * t)
            b = int(C0[2] + (C1[2] - C0[2]) * t)
            if inside_cloud(x, y):
                r = g = b = 255
            row[x] = (r, g, b, 255)
    return px


def downsample(px, size):
    """盒式降采样：W×W → size×size（源/目标比例不必整除）。"""
    scale = W / size
    out = []
    for y in range(size):
        y0, y1 = int(y * scale), max(int(y * scale) + 1, int((y + 1) * scale))
        row = []
        for x in range(size):
            x0, x1 = int(x * scale), max(int(x * scale) + 1, int((x + 1) * scale))
            ar = ag = ab = aa = 0
            n = 0
            for sy in range(y0, min(y1, W)):
                src = px[sy]
                for sx in range(x0, min(x1, W)):
                    r, g, b, a = src[sx]
                    # 预乘 alpha 再平均，透明边缘才不会发黑
                    ar += r * a
                    ag += g * a
                    ab += b * a
                    aa += a
                    n += 1
            if aa == 0 or n == 0:
                row.append((0, 0, 0, 0))
            else:
                row.append((ar // aa, ag // aa, ab // aa, aa // n))
        out.append(row)
    return out


def png_bytes(px, size):
    raw = bytearray()
    for y in range(size):
        raw.append(0)  # filter type 0
        for x in range(size):
            r, g, b, a = px[y][x]
            raw += bytes((r, g, b, a))

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    out = b"\x89PNG\r\n\x1a\n"
    out += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    out += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    out += chunk(b"IEND", b"")
    return out


def ico_bytes(images):
    """images: [(size, png_bytes)]，全部用 PNG 内嵌（Vista+）。"""
    n = len(images)
    header = struct.pack("<HHH", 0, 1, n)
    offset = 6 + 16 * n
    entries = b""
    blobs = b""
    for size, data in images:
        w = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        blobs += data
    return header + entries + blobs


def main():
    os.makedirs(OUT, exist_ok=True)
    px = render()
    print("超采样绘制完成", W, "x", W)

    targets = [(16, "16x16.png"), (32, "32x32.png"), (48, None), (64, None),
               (128, "128x128.png"), (256, "128x128@2x.png"), (512, "icon.png")]
    pngs = {}
    for size, name in targets:
        data = png_bytes(downsample(px, size), size)
        pngs[size] = data
        if name:
            with open(os.path.join(OUT, name), "wb") as f:
                f.write(data)
            print(f"  {name:16} {len(data):>6} B")

    ico = ico_bytes([(sz, pngs[sz]) for sz in (16, 32, 48, 64, 128, 256)])
    with open(os.path.join(OUT, "icon.ico"), "wb") as f:
        f.write(ico)
    print(f"  {'icon.ico':16} {len(ico):>6} B")


if __name__ == "__main__":
    main()
