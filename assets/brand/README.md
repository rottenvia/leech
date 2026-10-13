# Leech brand assets

Leech's own application mark: a leech bent into a lowercase L, with a sucker ring in its head,
white on a rose plate. These files are Leech design assets, not Discord's or any other vendor's
branding, and no third-party font is embedded in them.

- `leech.svg` — full-colour gradient mark on its rounded plate; the master vector source.
- `leech-flat.svg` — flat (non-gradient) variant for small or low-colour contexts.
- `leech-mark.svg` — solid white silhouette, view box trimmed to the mark's bounding box. The
  interface icon atlas carries it as `leech-mark`, tinted at draw time.
- `leech-1024.png` — 1024×1024 sRGB RGBA render of the full-colour mark.
- `leech-tray.png` — 72×72 black-on-transparent render of the silhouette, embedded by
  `crates/platform` as the macOS menu bar template image; macOS tints it per appearance, so only
  its alpha is used.

All of them, the packaged platform icons in `packaging/` (`macos/Leech.icns` and `Leech.icon`,
`windows/Leech.ico`, `linux/hicolor/*`) and the atlas cell come from one script. Change the
geometry or colours there and run it again:

```sh
python tools/generate-brand.py
```

It prints the SHA-256 of `leech-mark.svg`; copy it into the `leech-mark` row of
`tools/generate-icons.py`.

Brand palette: `#F78AAC` → `#D6336C` plate gradient, white mark. The interface accent comes from
the same family: `#F0719B` under dark text on dark appearances, `#D6336C`
(`design::DEFAULT_PRIMARY_RGB`) under white text on light ones.
