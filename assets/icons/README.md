# Interface icons

[Phosphor Icons](https://phosphoricons.com) by Helena Zhang and Tobias Fried, Copyright (c)
2023 Phosphor Icons, licensed under the [MIT License](LICENSE). Source: npm package
`@phosphor-icons/core` version 2.1.1 (repository https://github.com/phosphor-icons/core),
fetched through the jsDelivr npm mirror on September 10, 2026. The unmodified license is
`LICENSE` and is staged in both packages as `licenses/Phosphor-Icons-MIT.txt`.

A hundred and twelve unmodified Phosphor files are scaled to 56×56 pixels, filled white, and
rasterized by `resvg` 0.45.1 into one transparent PNG atlas with 64×64 cells (8 columns,
16 rows). Interface glyphs use the bold weight (`assets/bold/*.svg`, 86 files); brand logos,
badges, the filled star and file types keep the fill weight (`assets/fill/*.svg`, 26 files).
The interface moved from fill to bold on October 14, 2026, refetched from the same pinned
2.1.1 package. `headphones-slash` is derived from `headphones-bold.svg` by masking a diagonal
knockout and adding a 24-unit round-capped stroke, matching the bold weight. The application tints glyphs at draw time; no icon font,
JavaScript or per-icon file is bundled.

Ten brand marks Phosphor does not ship (PlayStation, Battle.net, Epic Games, League of Legends,
Riot Games, Bungie, Roblox, Crunchyroll, eBay, Bluesky) come from
[Simple Icons](https://simpleicons.org) npm package `simple-icons` version 16.30.0
(repository https://github.com/simple-icons/simple-icons), released under
[CC0 1.0](LICENSE-SIMPLE-ICONS) and fetched through the same mirror on September 11, 2026.
Their 24-unit glyphs fill the whole view box, so they are drawn at 80 % of the cell glyph size
to match Phosphor's visual weight. Brand marks remain trademarks of their owners; Simple Icons'
legal disclaimer applies. The license file is staged in both packages as
`licenses/Simple-Icons-CC0.txt`.

Four glyphs are drawn in this repository on the same 256-unit grid, rasterized with the
Phosphor set, and carry no upstream license: `thread.svg` (four slanted round-capped bars,
marks threads), `hang-up.svg` (a handset laid face down, one arched grip between two round
pads; it replaced Phosphor's `phone-disconnect`), `noise.svg` (five voice bars rising to
the middle, Discord's noise suppression waveform; `noise-slash` is the same file with the
generator's diagonal knockout) and `window.svg` (a rounded frame under a solid title bar, a window as a
share source).

- `atlas.png`: 512×1024, 38,652 bytes as a 256-entry white palette (the generator's Pillow
  fallback when `oxipng` is absent). SHA-256
  `f8572bedda59c2b732376e9efad9763146fa1e3cf90c1036094c471c879263da`.
- `index.tsv`: icon name, tab, zero-based cell; SHA-256
  `c437feb74500063517a7d136d1a4c1b99290c931ccc17569b7123f5995c0bdfe`.

Every upstream SVG's SHA-256 is pinned in `tools/generate-icons.py`, which refuses to build
from mismatching files. Regenerate from the repository root:

```sh
cargo install resvg --version 0.45.1 --root /tmp/resvg-tool
python3 tools/generate-icons.py --resvg /tmp/resvg-tool/bin/resvg
```

PNG compression bytes can vary with the resvg/png versions; decoded pixels and cell indices
are deterministic. `cargo test -p ui icons` checks that every `Icon` variant maps to a
distinct, non-blank cell and that the atlas stays below 256 KiB.

The folder and open-folder glyphs are unmodified Phosphor `folder-bold.svg` and
`folder-open-bold.svg` from the same pinned 2.1.1 package.

The media-viewer caret and download glyphs are unmodified Phosphor `caret-left-bold.svg` and
`download-simple-bold.svg`, fetched from the same pinned 2.1.1 package on September 11, 2026.

The screen-share fullscreen glyph is unmodified Phosphor `corners-out-bold.svg`, fetched from the
same pinned 2.1.1 package on September 27, 2026.

The `leech-mark` cell is our own artwork, not an upstream icon: it is the solid silhouette of
the Leech mark from `assets/brand/leech-mark.svg`, trimmed to its bounding box so it
matches Phosphor's glyph weight in the cell. It replaced the Discord brand mark that previously
occupied cell 58; no third-party application logo is bundled any more. `tools/generate-icons.py`
pins its SHA-256 like every upstream source and rasterizes it from the repository rather than a
package mirror.
