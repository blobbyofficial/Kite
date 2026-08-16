# Third-party components

## FFmpeg

Kite bundles `ffmpeg.exe` and `ffprobe.exe` and runs them as **separate programs**, communicating
over pipes and the command line. Kite does not link against FFmpeg's libraries and contains no
FFmpeg code. The two are distributed together but remain independent works, so FFmpeg's licence
does not extend to Kite itself.

The binaries we ship are the **GPL builds** from
[BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds), which include libx264 and other
GPL-licensed components. Those binaries remain under the GNU General Public License, version 3.

### Written offer for FFmpeg source

The complete corresponding source for the FFmpeg build shipped with Kite is published by the build
maintainers at <https://github.com/BtbN/FFmpeg-Builds>, and FFmpeg's own source is at
<https://github.com/FFmpeg/FFmpeg>. Each Kite release records the exact build it bundles in
`ffmpeg-build.txt`, installed alongside the application, so the matching sources can be
identified. If you would like the corresponding source and cannot obtain it from those locations,
open an issue and we will provide it.

FFmpeg's licensing terms: <https://ffmpeg.org/legal.html>.

### Alternative, if the GPL build becomes inconvenient

Switching to BtbN's **LGPL** build removes the GPL obligations entirely. The cost is libx264, so
software H.264 encoding would fall back to OpenH264 or rely on the hardware encoders Kite already
prefers. This is a one-line change to `FFMPEG_URL` in `.github/workflows/windows.yml`.

## H.264 and H.265 patents

Distributing an H.264 encoder at consumer scale can carry patent licensing obligations that are
separate from, and unaffected by, any software licence above. Worth legal review before a paid
release or large-scale distribution.

## Rust crates

Kite is built on `eframe`/`egui` (GPU-drawn interface), `wgpu`, `winit`, `cpal` (audio),
`zune-jpeg` (playback-file decoding), `memmap2`, `rfd`, `serde`, `crossbeam-channel` and
`parking_lot`. All are MIT or Apache-2.0 licensed. Run `cargo tree` for the complete list.
