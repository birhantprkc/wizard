# Fonts (the GUI)

The two typefaces the window embeds, in the format a window can actually load.
This is the only copy in the repository.

They came from `gui/assets/fonts/`, which served the browser GUI over `/fonts/`
as **woff2** — a browser format: brotli-compressed, where `fontdb` (the face
database underneath iced, through cosmic-text) reads TrueType and OpenType and
nothing else. So these are the same subsets, brotli-decompressed back to plain
TTF with [`woff2_decompress`](https://github.com/google/woff2). Byte-for-byte
the same glyph outlines, the same latin coverage, the same variation axes; only
the container differed. That directory was deleted with the browser GUI, so the
row below is now the whole provenance record.

| File | Family | Axis | Source |
| --- | --- | --- | --- |
| `inter.ttf` | Inter | `wght` 400–700 | the browser GUI's `inter.woff2`, decompressed |
| `jetbrains-mono.ttf` | JetBrains Mono | `wght` 400–600 | the browser GUI's `jetbrains-mono.woff2`, decompressed |

Both are **variable** fonts, and that is load-bearing rather than incidental:
one face carries every weight the UI asks for, and cosmic-text sets the `wght`
axis from the requested weight, so a bold heading is really bold instead of a
synthetically smeared regular. It is also why two files cover a design with
five weights in it.

Licensed under the SIL Open Font License 1.1 (`OFL.txt`), which permits
bundling and redistribution with the license attached.

- Inter — © 2016 The Inter Project Authors <https://github.com/rsms/inter>
- JetBrains Mono — © 2020 The JetBrains Mono Project Authors <https://github.com/JetBrains/JetBrainsMono>

To change a face, replace the TTF here and re-run the pixel snapshot test
(`the_bundled_fonts_rasterize_to_a_committed_digest`): the committed digest is a
function of these exact bytes, so a new subset is a deliberate, visible change
rather than a silent one.
