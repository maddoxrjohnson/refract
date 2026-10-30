"""Assembles docs/media/demo.gif from the frames scripts/record-demo.mjs saved.

Draws the pointer (screenshots leave it out), scales the frames down and quantises them
to one shared palette, so the loop stays a few megabytes. Needs Pillow.
"""

import json
import sys
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

APP = Path(__file__).resolve().parent.parent
FRAMES = APP / "test-results" / "demo-frames"
OUT = APP.parent / "docs" / "media" / "demo.gif"
WIDTH = 800
COLOURS = 128
# Pixels whose channels all changed by less than this since the last frame stay
# transparent, so the slow shimmer of the backdrop costs almost nothing.
STILL = 16

# An arrow pointer, tip at (0, 0), in screenshot pixels.
ARROW = [(0, 0), (0, 17), (4, 13), (7, 20), (10, 19), (7, 12), (12, 12)]


def draw_pointer(im: Image.Image, x: int, y: int, click: bool) -> None:
    d = ImageDraw.Draw(im)
    if click:
        d.ellipse((x - 14, y - 14, x + 14, y + 14), outline=(169, 216, 255), width=2)
    d.polygon([(x + px, y + py) for px, py in ARROW], fill=(255, 255, 255), outline=(10, 12, 18))


def main() -> int:
    meta = json.loads((FRAMES / "frames.json").read_text())
    frames = []
    for f in meta["frames"]:
        im = Image.open(FRAMES / f["file"]).convert("RGB")
        draw_pointer(im, f["x"], f["y"], f["click"])
        h = round(im.height * WIDTH / im.width)
        frames.append(im.resize((WIDTH, h), Image.LANCZOS))

    # One palette from a spread of frames keeps colours steady from frame to frame.
    sample = Image.new("RGB", (WIDTH, frames[0].height * 4))
    for i, k in enumerate(range(0, len(frames), max(1, len(frames) // 4))):
        if i < 4:
            sample.paste(frames[k], (0, frames[0].height * i))
    palette = sample.quantize(colors=COLOURS - 1, method=Image.Quantize.MEDIANCUT)
    clear = COLOURS - 1  # the palette's spare last entry marks unchanged pixels

    out = []
    shown = None  # what the viewer sees so far, in RGB
    for im in frames:
        q = im.quantize(palette=palette, dither=Image.Dither.NONE)
        rgb = q.convert("RGB")
        if shown is None:
            shown = rgb
        else:
            r, g, b = ImageChops.difference(rgb, shown).split()
            change = ImageChops.lighter(ImageChops.lighter(r, g), b)
            diff = change.point(lambda v: 255 if v < STILL else 0)
            q.paste(clear, mask=diff)
            shown = Image.composite(shown, rgb, diff)
        out.append(q)

    OUT.parent.mkdir(parents=True, exist_ok=True)
    out[0].save(
        OUT,
        save_all=True,
        append_images=out[1:],
        duration=round(1000 / meta["fps"]),
        loop=0,
        optimize=False,
        disposal=1,
        transparency=clear,
    )
    seconds = len(frames) / meta["fps"]
    print(f"{OUT}: {len(frames)} frames, {seconds:.1f} s, {OUT.stat().st_size / 1e6:.1f} MB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
