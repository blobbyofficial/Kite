# Kite — beta 1

A video editor built around one idea: it should stay responsive on a modest laptop.

## Getting going

1. **Import** — drag video files onto the window, or press `Ctrl+I`.
   Kite prepares each file in the background (you'll see a progress bar in the media list).
   That step builds an all-intra playback file, which is why scrubbing is instant afterwards
   regardless of how the original was compressed.
2. **Add to timeline** — double-click an item in the media pool.
3. **Cut** — put the playhead where you want the cut and press `S`. Select the bad take and
   press `Shift+Del` to remove it and close the gap.
4. **Trim** — drag a clip's left or right edge. Drag its middle to move it, including onto
   another track.
5. **Title** — press the `T Text` button, then edit it in the inspector on the right.
6. **Export** — `Ctrl+E`. Export always reads your original files, never the playback proxies,
   so the master is full quality.

## Keyboard

| Key | Does |
|---|---|
| `Space` | Play / pause |
| `←` `→` | Step one frame (hold `Shift` for a second) |
| `Home` / `End` | Jump to start / end |
| `S` or `Ctrl+K` | Split at the playhead |
| `Del` | Delete the selected clip |
| `Shift+Del` | Ripple delete — remove it and close the gap |
| `M` | Snapping on/off |
| `+` / `-` | Zoom the timeline |
| `Ctrl`+scroll | Zoom around the pointer |
| `Shift`+scroll | Scroll the timeline |
| `Ctrl+Z` / `Ctrl+Shift+Z` | Undo / redo |
| `Ctrl+S` / `Ctrl+O` | Save / open project |
| `Ctrl+I` / `Ctrl+E` | Import / export |

## What's in this beta

Multi-track video and audio, trimming and ripple editing, drag between tracks (single or
multiple clips at once), snapping, rubber-band selection, copy and paste, duplicate and nudge.
Per-clip volume with fades, opacity, scale and position, crossfades between clips, speed from
0.25× to 4× with the audio retimed, and brightness/contrast/saturation with one-click looks.
Titles, waveforms and thumbnails on the timeline, audio metering, undo/redo, autosave with
crash recovery, project save/load, and H.264 export with hardware encoders when available.

## What isn't, yet

Colour wheels, curves and scopes; keyframes; transitions other than crossfade. These are next —
the engine underneath them is already in place.

## Extra keys

| Key | Does |
|---|---|
| `Ctrl+T` | Crossfade into the selected clip from the one before |
| `Ctrl+C` / `Ctrl+V` | Copy and paste at the playhead |
| `Ctrl+D` | Duplicate the selection |
| `,` / `.` | Nudge by a frame (Shift for a second) |
| Drag empty space | Rubber-band select |
| `Shift`+scroll | Scroll tracks vertically |

## If something goes wrong

- **"Still importing"** on export — wait for the media list to finish preparing.
- **Playback is silent** — the status bar names the audio device it found; if it says audio is
  unavailable, another application may hold the device exclusively.
- **A file won't import** — the media list shows the reason underneath the name. Most often the
  file is a format ffmpeg can read but has no video or audio stream in it.

Playback files are cached under `%LOCALAPPDATA%\Kite`. Deleting that folder is safe; Kite
rebuilds what it needs.
