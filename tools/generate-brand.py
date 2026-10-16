#!/usr/bin/env python3
"""Rebuild every Leech brand asset from the geometry below (development only).

The mark is a leech bent into a lowercase L: a round head with a sucker ring and a body that
tapers along the curve, white on a rose plate. Requires Pillow:

    python tools/generate-brand.py

Writes the SVG sources in `assets/brand`, the packaged icons in `packaging/` and the
`leech-mark` cell of the interface icon atlas, then prints the SVG hash that
`tools/generate-icons.py` pins for that cell.
"""

import hashlib
import math
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
BOX = 1024
SCALE = 4  # supersampling for the raster renders

# Mark geometry in a 1024 box: a cubic spine swept by a circle that shrinks from head to tail.
SPINE = ((374.0, 332.0), (314.0, 682.0), (504.0, 842.0), (744.0, 682.0))
HEAD_RADIUS, TAIL_RADIUS, TAPER = 150.0, 56.0, 0.8
RING_RADIUS = 62.0
PLATE_RADIUS = 228
TOP, BOTTOM = (0xF7, 0x8A, 0xAC), (0xD6, 0x33, 0x6C)
FLAT = (0xE0, 0x4D, 0x80)
INK = "#fff"

# Interface atlas cell of the mark; see `assets/icons/index.tsv`.
ATLAS_CELL, ATLAS_COLUMNS, CELL, PAD = 58, 8, 64, 4


def spine(t):
	u = 1 - t
	return tuple(
		u**3 * SPINE[0][i] + 3 * u * u * t * SPINE[1][i] + 3 * u * t * t * SPINE[2][i] + t**3 * SPINE[3][i]
		for i in (0, 1)
	)


def radius(t):
	return HEAD_RADIUS + (TAIL_RADIUS - HEAD_RADIUS) * t**TAPER


def arc(centre, size, start, end, steps=40):
	"""Points on a circle from angle `start` to `end` (radians), going the increasing way."""
	if end < start:
		end += 2 * math.pi
	return [
		(centre[0] + size * math.cos(start + (end - start) * i / steps), centre[1] + size * math.sin(start + (end - start) * i / steps))
		for i in range(steps + 1)
	]


def outline(samples=160):
	"""Envelope of the swept circle: one closed polygon."""
	left, right = [], []
	for index in range(samples + 1):
		t = index / samples
		before, after = spine(max(t - 1e-3, 0)), spine(min(t + 1e-3, 1))
		dx, dy = after[0] - before[0], after[1] - before[1]
		length = math.hypot(dx, dy)
		tx, ty = dx / length, dy / length
		# A shrinking circle touches its envelope slightly ahead of the normal.
		lean = -(radius(min(t + 1e-3, 1)) - radius(max(t - 1e-3, 0))) / length
		lean = max(-0.95, min(0.95, lean))
		side = math.sqrt(1 - lean * lean)
		(cx, cy), size = spine(t), radius(t)
		left.append((cx + size * (lean * tx + side * ty), cy + size * (lean * ty - side * tx)))
		right.append((cx + size * (lean * tx - side * ty), cy + size * (lean * ty + side * tx)))
	angle = lambda centre, point: math.atan2(point[1] - centre[1], point[0] - centre[0])
	head, tail = spine(0), spine(1)
	points = arc(head, HEAD_RADIUS, angle(head, right[0]), angle(head, left[0]))
	points += left[1:-1]
	points += arc(tail, TAIL_RADIUS, angle(tail, left[-1]), angle(tail, right[-1]))
	points += right[-2:0:-1]
	return points


def path(offset=(0.0, 0.0)):
	points = [f"{x - offset[0]:.1f} {y - offset[1]:.1f}" for x, y in outline()]
	ring = (SPINE[0][0] - offset[0], SPINE[0][1] - offset[1])
	return (
		f"M {' L '.join(points)} Z "
		f"M {ring[0] - RING_RADIUS:.1f} {ring[1]:.1f} "
		f"a {RING_RADIUS:g} {RING_RADIUS:g} 0 1 0 {2 * RING_RADIUS:g} 0 "
		f"a {RING_RADIUS:g} {RING_RADIUS:g} 0 1 0 {-2 * RING_RADIUS:g} 0 Z"
	)


def mark_svg(fill, offset=(0.0, 0.0)):
	return f'<path id="leech-mark" fill-rule="evenodd" fill="{fill}" d="{path(offset)}"/>'


def mask(size=BOX * SCALE):
	"""Alpha of the mark at `size` pixels for the whole 1024 box."""
	scale = size / BOX
	image = Image.new("L", (size, size), 0)
	draw = ImageDraw.Draw(image)
	draw.polygon([(x * scale, y * scale) for x, y in outline()], fill=255)
	(x, y), inner = SPINE[0], RING_RADIUS
	draw.ellipse(((x - inner) * scale, (y - inner) * scale, (x + inner) * scale, (y + inner) * scale), fill=0)
	return image


def bounds():
	"""Square box around the mark, in design units."""
	left, top, right, bottom = mask(BOX).getbbox()
	side = max(right - left, bottom - top) + 4
	return ((left + right - side) / 2, (top + bottom - side) / 2, side)


def plate(size, flat=False):
	big = size * SCALE
	if flat:
		fill = Image.new("RGB", (big, big), FLAT)
	else:
		# Diagonal rose gradient, lighter at the top left.
		ramp = Image.linear_gradient("L").resize((big * 2, big * 2)).rotate(45, resample=Image.BICUBIC)
		ramp = ramp.crop((big // 2, big // 2, big // 2 + big, big // 2 + big))
		fill = Image.merge(
			"RGB",
			[ramp.point(lambda v, a=TOP[i], b=BOTTOM[i]: round(a + (b - a) * v / 255)) for i in range(3)],
		)
	shape = Image.new("L", (big, big), 0)
	ImageDraw.Draw(shape).rounded_rectangle(
		(0, 0, big - 1, big - 1), radius=PLATE_RADIUS * big // BOX, fill=255
	)
	fill.putalpha(shape)
	return fill


def app_icon(size, flat=False):
	image = plate(size, flat)
	white = Image.new("RGBA", image.size, (255, 255, 255, 255))
	image.paste(white, (0, 0), mask(size * SCALE))
	return image.resize((size, size), Image.LANCZOS)


def silhouette(size, color):
	"""The mark alone, trimmed to its bounds, `color` on transparent."""
	left, top, side = bounds()
	scale = SCALE * size / side
	full = mask(round(BOX * scale))
	crop = full.crop(
		(round(left * scale), round(top * scale), round(left * scale) + size * SCALE, round(top * scale) + size * SCALE)
	)
	image = Image.new("RGBA", crop.size, color + (255,))
	image.putalpha(crop)
	return image.resize((size, size), Image.LANCZOS)


def write(relative, text):
	(ROOT / relative).write_text(text, encoding="utf-8", newline="\n")


def svg(title, body, view=f"0 0 {BOX} {BOX}", size=BOX):
	return (
		'<?xml version="1.0" encoding="UTF-8"?>\n'
		f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="{view}" fill="none">\n'
		f"  <title>{title}</title>\n"
		"  <desc>Leech mark: a leech curled into a speech mark. Vector contours only; no embedded raster.</desc>\n"
		f"{body}\n</svg>\n"
	)


def main():
	hexa = lambda rgb: "#%02x%02x%02x" % rgb
	rounded = f'<rect id="leech-plate" width="{BOX}" height="{BOX}" rx="{PLATE_RADIUS}" fill="{{}}"/>'
	gradient = (
		'<defs><linearGradient id="leech-rose" x1="0" y1="0" x2="1" y2="1">'
		f'<stop offset="0" stop-color="{hexa(TOP)}"/><stop offset="1" stop-color="{hexa(BOTTOM)}"/>'
		"</linearGradient></defs>\n"
	)
	write("assets/brand/leech.svg", svg("Leech app icon", gradient + rounded.format("url(#leech-rose)") + "\n" + mark_svg(INK)))
	write("assets/brand/leech-flat.svg", svg("Leech flat app icon", rounded.format(hexa(FLAT)) + "\n" + mark_svg(INK)))
	write("packaging/linux/hicolor/scalable/apps/leech.svg", (ROOT / "assets/brand/leech.svg").read_text(encoding="utf-8"))
	write("packaging/linux/hicolor/scalable/apps/leech-symbolic.svg", svg("Leech symbolic icon", mark_svg("#2e3436")))

	left, top, side = bounds()
	mark = svg("Leech mark", mark_svg(INK, (left, top)), f"0 0 {side:g} {side:g}", f"{side:g}")
	write("assets/brand/leech-mark.svg", mark)
	# The layered macOS icon tints one white layer; the system supplies the plate.
	write("packaging/macos/Leech.icon/Assets/01-mark.svg", svg("Leech mark layer", mark_svg(INK)))

	app_icon(BOX).save(ROOT / "assets/brand/leech-1024.png")
	silhouette(72, (0, 0, 0)).save(ROOT / "assets/brand/leech-tray.png")
	app_icon(256).save(ROOT / "packaging/windows/leech.png")
	sizes = [16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 128, 192, 256]
	app_icon(256).save(
		ROOT / "packaging/windows/Leech.ico",
		sizes=[(n, n) for n in sizes],
		append_images=[app_icon(n) for n in sizes[:-1]],
	)
	app_icon(BOX).save(ROOT / "packaging/macos/Leech.icns")
	for folder in sorted((ROOT / "packaging/linux/hicolor").glob("*x*")):
		app_icon(int(folder.name.split("x")[0])).save(folder / "apps/leech.png")

	# Patch only the brand cell; every other cell keeps its pinned upstream render.
	atlas_path = ROOT / "assets/icons/atlas.png"
	atlas = Image.open(atlas_path).convert("RGBA")
	x, y = (ATLAS_CELL % ATLAS_COLUMNS) * CELL, (ATLAS_CELL // ATLAS_COLUMNS) * CELL
	atlas.paste(Image.new("RGBA", (CELL, CELL), (255, 255, 255, 0)), (x, y))
	atlas.paste(silhouette(CELL - 2 * PAD, (255, 255, 255)), (x + PAD, y + PAD))
	# White at every alpha fits one 256-entry palette, as the generated atlas uses.
	indexed = Image.frombytes("P", atlas.size, atlas.getchannel("A").tobytes())
	indexed.putpalette([255, 255, 255] * 256)
	indexed.save(atlas_path, transparency=bytes(range(256)), optimize=True)

	print("leech-mark.svg sha256", hashlib.sha256(mark.encode("utf-8")).hexdigest())


if __name__ == "__main__":
	main()
