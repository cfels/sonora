# Figma implementation and visual verification

## Source inspected

`FIGMA/sonora.fig` was opened as a ZIP and its Kiwi canvas decoded with
`kiwi-schema` and `fzstd` in temporary tooling. It contains 108 nodes, seven
raster assets, metadata and a 400×299 thumbnail. Two player frames are present:
`2:5` (Frame 1) and `3:191` (Frame 2), both 1920×1080. A separate sketch image
lies outside the frames. The original file was preserved unchanged.

The file does **not** contain detailed Home, Settings, library, album, artist,
playlist, lyrics, dialog, responsive, hover or focus-state frames. Those screens
retain real functionality and use the player's shared style; their exact 1:1
appearance cannot be verified against nonexistent reference frames.

## Extracted measurements

Coordinates below are for the 1920×1080 frames.

| Element | x, y | width × height | Detail |
| --- | --- | --- | --- |
| Logo panel | 10, 10 | 90 × 90 | radius 10 |
| Header | 109, 10 | 1801 × 90 | radius 10 |
| Sidebar | 10, 111 | 265 × 960 | radius 13 |
| Main artwork | 18, 118 | 250 × 250 | radius 10 |
| Transport panels | 18, 376 / 474 | 250 × 90 | radius 10 |
| Sidebar navigation | 18, 591 | 250 × 53 per row | font 24 |
| Settings entry | 18, 1015 | 250 × 53 | font 24 |
| Right panel | 1536, 111 | 374 × 960 | radius 13 |
| Queue artwork | 1548, 253 | 64 × 64 | 72 row pitch |
| Queue title / artist | 1624, 257 | — | 35.264 / 17.632 |
| Header title / subtitle | 121, 8 / 57 | — | 40 / 20 |

Background is `#141414`. Panel fill is `#b4b4b4` at 40% opacity; the cover layer
has a black 44% overlay. Extracted blur radii are 347.5 for the scene and 59.2 /
141.2 for panels. These are documented source measurements, not a claim that
Slint and Figma's compositing algorithms produce identical pixels.

The complete decoded visible-node measurements are in `FIGMA_MEASUREMENTS.json`.

Exact logo, previous/pause/next, shuffle/repeat and volume vectors were extracted
into `assets/design/`. Search, play and lyrics icons support real interactions
not fully specified in the frames. See the assets README for provenance.

## Implementation

The native Slint shell uses the measured panel and cover geometry, shared
palette/radii, extracted SVGs, queue sizing and sidebar hierarchy. Header and
sidebar artwork stay visible while the real library/settings screens render
inside the center. Clicking the cover opens the player view.

Requested later by the user: the current track's cover supplies the blurred
background, with independently persisted Appearance switches for panels and
background. This deliberately follows the current-cover request rather than
reproducing the frame's multi-image background collage. Foreground artwork and
text stay sharp. A worker generates 480×270 blur textures; the approximate
Gaussian radii are scaled to that resolution. Cover/background transitions
animate for 140 ms. Slint has no native backdrop filter, so panels sample the
same scene coordinates from the prepared texture.

Unimplemented Cached/Profile/Sync destinations from the design were not turned
into fake buttons. Their reserved sidebar spacing is retained. Real Search,
Queue and Lyrics controls supplement the sparse reference header.

## Comparisons performed

The two frames were reconstructed at 1920×1080 from decoded geometry, vector
glyph outlines and embedded artwork; reference SVG/PNG files and the original
thumbnail accompany the delivery. These PNGs are **reconstructions**, not
full-resolution exports from the Figma application. Blend/blur rendering is an
approximation. They must not be described as authoritative pixel exports.

The real application was launched repeatedly in Xvfb with `winit-software` and
isolated generated music/lyrics. Screenshots cover the player, Home, Tracks,
Settings, Queue, Lyrics/manual scroll, playlists, search, long content, cache
confirmation and appearance options. Comparisons at 1920×1080 drove corrections
to header/control alignment, panel geometry, logo/vector export, text overflow,
lyrics viewport sizing and rail layout. A 920×600 capture and software render
exercise narrow layout with 600 long rows. Selected final screenshots and
reference reconstructions are included in the delivery.

An absolute-difference image and ImageMagick RMSE measurement accompany the
side-by-side comparison (reference reconstruction on the left, native app on the
right). They expose differences; no pass threshold or pixel-identity claim is
assigned, because the background/content/font differences are documented.

## Remaining discrepancies

- Segoe UI used in the design is unavailable on this Linux host. The app uses
  the available sans-serif fallback; font metrics and rasterization differ.
- Figma blur/blend algorithms are not reproduced pixel-for-pixel. A quantitative
  whole-frame pixel identity result would be misleading with reconstructed
  references, different fonts and the requested current-cover background.
- Real search, remove, queue and accessibility controls differ where the sparse
  reference has no functional specification.
- The supplied portrait artwork is very small and appears pixelated when
  enlarged, as it does in the reference asset. No replacement art was invented.
- Detailed missing screens and interaction states need usable design exports
  or additional frames before exact visual sign-off is possible.

The shell was measured and visually compared; this report does not assert
pixel-perfect fidelity for the whole application.
