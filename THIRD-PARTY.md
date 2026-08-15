# Third-party components

## FFmpeg

Kite bundles `ffmpeg.exe` and `ffprobe.exe` and invokes them as separate processes for
decoding on import, and for encoding on export. Kite does not link against FFmpeg libraries.

The bundled binaries are the **GPL** builds from
[BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds), which include libx264 and other
GPL-licensed encoders. FFmpeg is licensed under the LGPL 2.1 or later, with GPL components
enabled in this build; see <https://ffmpeg.org/legal.html>.

**Before distributing Kite publicly**, decide how to handle this:

- Ship the **LGPL** build instead (`ffmpeg-master-latest-win64-lgpl.zip`) and rely on hardware
  encoders plus OpenH264. Simplest legally, but software H.264 quality drops.
- Or keep the GPL build and license Kite itself under a GPL-compatible licence.
- Or have users supply their own FFmpeg.

For a private beta this is not an issue; for a public release it needs a decision.

## H.264 / H.265 patents

Distributing an H.264 encoder at consumer scale can carry patent licensing obligations
independent of the software licence above. Worth legal review before a paid release.

## Rust crates

Built on `eframe`/`egui` (GPU-drawn UI), `wgpu`, `winit`, `cpal` (audio), `zune-jpeg`
(proxy decoding), `memmap2`, `rfd`, `serde`, `crossbeam-channel` and `parking_lot`.
All are MIT or Apache-2.0 licensed. Run `cargo tree` for the full list.
