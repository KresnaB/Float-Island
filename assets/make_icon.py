"""Draws the FloatIsland logo: a black island pill floating over a sky-to-sea gradient.

    python assets/make_icon.py   ->  assets/icon.ico (16..256 px), assets/logo.png (512 px)
                                     and assets/tray-32.argb (Linux tray icon, raw ARGB32)
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

S = 1024  # draw large, downsample for smooth edges
HERE = Path(__file__).parent


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def logo() -> Image.Image:
    # Background: rounded square, indigo sky -> cyan sea, diagonal.
    grad = Image.new("RGBA", (S, S))
    top, bottom = (79, 70, 229), (6, 182, 212)
    px = grad.load()
    for y in range(S):
        for x in range(S):
            px[x, y] = (*lerp(top, bottom, (x * 0.35 + y) / (S * 1.35)), 255)
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, S - 1, S - 1), radius=230, fill=255)
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    img.paste(grad, mask=mask)

    # Soft shadow under the pill: it floats.
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).ellipse((240, 676, 784, 752), fill=(10, 20, 60, 120))
    img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(28)))

    # Pill is chunky on purpose so it still reads at 16 px.
    d = ImageDraw.Draw(img)
    pill = (124, 352, 900, 616)
    d.rounded_rectangle(pill, radius=132, fill=(8, 8, 10, 255), outline=(255, 255, 255, 46), width=8)
    # "Content" inside the pill: a status line and Claude's orange dot.
    d.rounded_rectangle((250, 461, 570, 507), radius=23, fill=(255, 255, 255, 230))
    d.ellipse((690, 426, 806, 542), fill=(217, 119, 87, 255))
    return img


def main():
    big = logo()
    big.resize((512, 512), Image.LANCZOS).save(HERE / "logo.png")
    sizes = [256, 128, 64, 48, 32, 24, 16]
    big.resize((256, 256), Image.LANCZOS).save(HERE / "icon.ico", sizes=[(s, s) for s in sizes])
    # StatusNotifierItem pixmap: ARGB32 in network byte order, embedded by src/linux/mod.rs.
    rgba = big.resize((32, 32), Image.LANCZOS).tobytes()
    argb = bytes(b for i in range(0, len(rgba), 4) for b in (rgba[i + 3], *rgba[i : i + 3]))
    (HERE / "tray-32.argb").write_bytes(argb)


if __name__ == "__main__":
    main()
