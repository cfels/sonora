# Design asset provenance

Extracted without redrawing from `FIGMA/sonora.fig` (108 decoded nodes):

| File | Figma node |
|---|---|
| `logo.svg` | `6:2`, visible vector children `6:3` and `6:4` |
| `previous.svg` | `3:65` |
| `pause.svg` | `3:71` |
| `next.svg` | `3:73`, including its reflected transform |
| `repeat.svg` | `3:97` |
| `shuffle.svg` | `3:99` |
| `volume.svg` | `3:101`, including its two vector children |

The Figma nodes use white fills. Invisible frame fills are omitted. Path
coordinates are decoded from the file's geometry blobs; they are not traced
from the thumbnail. `play.svg`, `lyrics.svg` and `search.svg` are simple vector
companions for real controls whose corresponding states are absent from the
supplied frames. The original branding SVG is preserved separately.
