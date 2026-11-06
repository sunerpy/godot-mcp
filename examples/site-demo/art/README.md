# Demo art

The pictures in the demo project that the documentation site's screenshots are taken from
(`scripts/site/capture_screens.py`). They were generated for this project on 2026-11-06 and show
nothing from an existing game.

| File             | Size      | Model         | Seed     | Generated at | Background removed with | Fitted into |
| ---------------- | --------- | ------------- | -------- | ------------ | ----------------------- | ----------- |
| `player.png`     | 75 × 96   | Z-Image Turbo | 20261106 | 1024 × 1024  | BiRefNet                | 96 × 96     |
| `coin.png`       | 40 × 40   | Z-Image Turbo | 20261107 | 1024 × 1024  | BiRefNet                | 40 × 40     |
| `crate.png`      | 64 × 60   | Z-Image Turbo | 20261108 | 1024 × 1024  | BiRefNet                | 64 × 64     |
| `ground.png`     | 950 × 48  | Z-Image Turbo | 20261109 | 1920 × 192   | BiRefNet                | 960 × 48    |
| `background.png` | 960 × 540 | Z-Image Turbo | 20261110 | 1792 × 1008  | —                       | 960 × 540   |

Both models are licensed for any use, including commercial: Z-Image Turbo under Apache-2.0 and
BiRefNet under MIT. Each image was made by ComfyUI's text-to-image workflow for Z-Image Turbo
with 8 steps, and the background was removed with a BiRefNet workflow. The fitting is the `fit`
command of `scripts/site/image_tool.gd`: it crops an image with transparency to its visible
pixels, then scales it with Lanczos to fit inside the box, keeping its aspect ratio. Running it
again on the generated images gives these files byte for byte.

Every prompt ends with the same style text:

> flat vector illustration for a 2D side-scrolling game, bold dark outlines, soft cel shading,
> clean shapes, no text, no letters, no logo, no watermark

and begins with:

- `player.png`: a cute small round robot character facing right, teal and white body, one big
  round glowing cyan eye, a short antenna with a ball tip, two short legs, centered, full body,
  isolated on a plain light grey background
- `coin.png`: a shiny gold game coin with an embossed five-pointed star in the center, front
  view, centered, isolated on a plain light grey background
- `crate.png`: a sturdy wooden crate game prop, front view, warm brown planks with dark metal
  corner brackets, centered, isolated on a plain light grey background
- `ground.png`: a long horizontal strip of grassy ground seen from the side, bright green grass
  tufts on top of rich brown soil with small round stones, the strip fills the whole width of
  the frame, plain light grey background above the grass
- `background.png`: a calm daytime landscape background, soft blue sky with a few round white
  clouds, layered rolling green hills fading into the distance, a gentle meadow at the bottom,
  warm light, simple shapes, no characters, no buildings
