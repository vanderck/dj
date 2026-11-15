"""Generates the Collective app icon (packaging/macos/AppIcon.png, 1024x1024).

A dark rounded square (macOS icon grid: 824 px body on a 1024 canvas) with a platter made of four
arcs in the deck colours, record grooves and a centre spindle. Drawn at 4x and downsampled for
smooth edges.

    pip install pillow
    python packaging/icon/make_icon.py
"""
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

SS = 4  # supersampling
N = 1024 * SS
DECK = [(0xE5, 0x48, 0x4D), (0xF2, 0xC8, 0x3C), (0x38, 0x8E, 0xEC), (0x8B, 0x5C, 0xF6)]


def s(v):
    return int(v * SS)


def main(out: Path):
    img = Image.new("RGBA", (N, N), (0, 0, 0, 0))

    # Soft drop shadow under the body.
    shadow = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle((s(100), s(112), s(924), s(936)), radius=s(185), fill=(0, 0, 0, 120))
    img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(s(18))))

    # Body: vertical gradient, clipped to the rounded square.
    body = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    grad = Image.new("RGBA", (1, N))
    top, bottom = (0x2A, 0x2A, 0x32), (0x0E, 0x0E, 0x12)
    for y in range(N):
        t = y / (N - 1)
        grad.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,))
    grad = grad.resize((N, N))
    mask = Image.new("L", (N, N), 0)
    ImageDraw.Draw(mask).rounded_rectangle((s(100), s(100), s(924), s(924)), radius=s(185), fill=255)
    body.paste(grad, (0, 0), mask)
    d = ImageDraw.Draw(body)
    # Hairline highlight around the edge.
    d.rounded_rectangle((s(101), s(101), s(923), s(923)), radius=s(184), outline=(255, 255, 255, 28), width=s(3))

    cx = cy = N // 2
    # Platter disc and grooves.
    r_disc = s(300)
    d.ellipse((cx - r_disc, cy - r_disc, cx + r_disc, cy + r_disc), fill=(0x16, 0x16, 0x1B, 255))
    for r in range(130, 290, 22):
        d.ellipse((cx - s(r), cy - s(r), cx + s(r), cy + s(r)), outline=(255, 255, 255, 16), width=s(2))

    # Four deck-coloured arcs around the platter (one per deck), with small gaps.
    r_arc, w_arc = s(338), s(58)
    box = (cx - r_arc, cy - r_arc, cx + r_arc, cy + r_arc)
    for i, col in enumerate(DECK):
        start = -90 + i * 90 + 6  # clockwise from 12 o'clock: deck 1 top-right
        d.arc(box, start, start + 78, fill=col + (255,), width=w_arc)

    # Spindle with a deck-1 red ring.
    for r, col in ((s(70), DECK[0] + (255,)), (s(46), (0x16, 0x16, 0x1B, 255)), (s(22), (0xEE, 0xEE, 0xF2, 255))):
        d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=col)

    img.alpha_composite(body)
    out.parent.mkdir(parents=True, exist_ok=True)
    img.resize((1024, 1024), Image.LANCZOS).save(out)
    print(f"wrote {out}")
    return img


if __name__ == "__main__":
    root = Path(__file__).resolve().parents[2]
    big = main(root / "packaging" / "macos" / "AppIcon.png")
    # Window icon compiled into the app.
    small = root / "crates" / "app" / "assets" / "icon-256.png"
    big.resize((256, 256), Image.LANCZOS).save(small)
    print(f"wrote {small}")
