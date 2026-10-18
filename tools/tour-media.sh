#!/bin/sh
# Make the welcome tour's and What's New's films (assets/tour/*.webp): the simulator
# photographs each feature at work (sim::journeys::tour_films, in the sandbox, in the
# Graphite theme with Google Sans Flex and Google Sans Code), then Pillow draws the keys
# pressed over the frames and packs each film as an animated WebP.
# Usage: tour-media.sh [film,film...]   Needs both fonts installed, ImageMagick in the
# sandbox and python3 with Pillow built with WebP.
set -e
C=$(dirname "$(dirname "$(readlink -f "$0")")")
W=${TOUR_WORK:-$C/target/tour-media}
rm -rf "$W"; mkdir -p "$W/fonts"
for f in "Google Sans Flex" "Google Sans Code"; do
  p=$(fc-match --format=%{file} "$f")
  case "$p" in *GoogleSans*) cp "$p" "$W/fonts/" ;; *) echo "$f is not installed" >&2; exit 1 ;; esac
done
# Arrows and other symbols the interface font lacks, for the key chips.
SYM=$(fc-match --format=%{file} "sans-serif:charset=2190 2192")
MAIA_TOUR_MEDIA=1 MAIA_TOUR_FILM=${1:-} MAIA_SIM_SHOTS="$W" "$C/tools/sim.sh" sim::journeys::tour_films
python3 -I - "$W" "$C/assets/tour" "$SYM" <<'PY'
import os, sys
from PIL import Image, ImageDraw, ImageFont, _webp
work, out, sym = sys.argv[1], sys.argv[2], sys.argv[3]
films = os.path.join(work, "films")
fonts = os.path.join(work, "fonts")
flex = next(os.path.join(fonts, f) for f in os.listdir(fonts) if f.startswith("GoogleSansFlex"))
for name in sorted(os.listdir(films)):
    d = os.path.join(films, name)
    lines = open(os.path.join(d, "frames.txt")).read().splitlines()
    head = dict(kv.split("=") for kv in lines[0].split()[2:])
    col = lambda k: tuple(int(head[k][i:i + 2], 16) for i in (0, 2, 4))
    ppp = float(head["ppp"])
    font = ImageFont.truetype(flex, round(13 * ppp))
    arrows = ImageFont.truetype(sym, round(13 * ppp))
    frames, ms = [], []
    for row in lines[1:]:
        f, t, keys, at = row.split("\t")
        im = Image.open(os.path.join(d, f)).convert("RGB")
        dr = ImageDraw.Draw(im, "RGBA")
        if keys != "-":
            # Each key in a chip of its own, as the app draws its key hints.
            parts = keys.split(" ")
            pad, gap, h = round(9 * ppp), round(6 * ppp), round(26 * ppp)
            face = lambda p: font if all(ord(ch) < 0x2000 for ch in p) else arrows
            widths = [max(dr.textlength(p, font=face(p)) + 2 * pad, h) for p in parts]
            # In the title bar's right end, over the window buttons: nothing a film is
            # about happens there.
            x = im.width - sum(widths) - gap * (len(parts) - 1) - round(10 * ppp)
            y = round(6 * ppp)
            for p, w in zip(parts, widths):
                dr.rounded_rectangle([x, y, x + w, y + h], radius=round(6 * ppp), fill=col("raised") + (245,), outline=col("border") + (255,), width=max(1, round(ppp)))
                dr.text((x + w / 2, y + h / 2), p, font=face(p), fill=col("text") + (255,), anchor="mm")
                x += w + gap
        if at != "-":
            # Where the pointer clicked: an arrow, light with a dark edge.
            px, py = (float(v) for v in at.split(","))
            s = ppp
            arrow = [(0, 0), (0, 17), (4.5, 13), (7.5, 19.5), (10, 18.5), (7, 12), (12.5, 12)]
            dr.polygon([(px + ax * s, py + ay * s) for ax, ay in arrow], fill=(250, 250, 250, 255), outline=(20, 20, 20, 255), width=max(1, round(s)))
        frames.append(im)
        ms.append(int(t))
    # Every frame whole (a key frame): a frame kept as a lossy difference from the one
    # before left faint traces of rows that had gone. 17% more bytes for clean frames.
    enc = _webp.WebPAnimEncoder(frames[0].size, 0xff000000, 0, False, 0, 1, True, 0)
    t = 0
    for im, m in zip(frames, ms):
        enc.add(im.convert("RGBA").getim(), t, False, int(os.environ.get("TOUR_QUALITY", "62")), 100, 6)
        t += m
    enc.add(None, t, False, 80, 100, 0)
    dst = os.path.join(out, name + ".webp")
    open(dst, "wb").write(enc.assemble("", "", ""))
    frames[-1].save(os.path.join(work, name + "-last.png"))
    print(f"{name}: {len(frames)} states, {t / 1000:.1f} s, {os.path.getsize(dst)} bytes")
PY
