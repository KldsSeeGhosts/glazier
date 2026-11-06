<img src="assets/icon.svg" width="64" height="64" alt="">

# Glazier

A screenshot annotator for Windows, ported from the Noctalia Screen Toolkit
annotator. It runs in the tray. A shortcut
freezes all monitors, you pick a region, and the editor opens
in place over it.

## Use

1. Run `glazier.exe`. It adds a tray icon and stays running.
2. Press **PrintScreen** or **Win+Shift+S** (both replace the Snipping Tool
   shortcuts while Glazier runs). You can also left-click the tray icon or run
   `glazier.exe --capture`.
3. Pick a region. Hover to snap to a window, drag to draw a region, or press
   Enter to take what is highlighted. Esc or right-click cancels.
4. Annotate, then **Copy** (Ctrl+C) or **Save** (Ctrl+S).

You can start the shortcut on one monitor and select on another, or drag across
monitor edges. Window snapping works across the desktop. Selection and export
use physical pixels, including on monitors with different DPI scales or negative
desktop coordinates. Gaps between offset monitors export as black.

The toolbar stays on the monitor containing the largest part of the selection,
using that monitor's scale for the toolbar and annotations. It sits below the
selection when there is room, otherwise above, otherwise inside the selection
near its bottom edge. Placement is clamped to that monitor's work area, so a
full-monitor or multi-monitor selection cannot hide the toolbar under a taskbar
or off screen. Esc also works when another application has taken keyboard focus.
When editing pending text, the first Esc discards it and the next Esc closes the
editor. A display layout change cancels the current capture so the next shortcut
can freeze the new layout.

### Editor

| Tool | Key | Behavior |
| --- | --- | --- |
| Pencil | P | Freehand, thinner when drawn fast |
| Highlighter | H | Wide translucent marker |
| Line, Arrow | L, A | Straight line, optionally with a head |
| Rectangle, Circle | R, C | Circle is drawn from its center |
| Text | T | Click, type, Enter to commit, Esc to discard |
| Blur | B | Pixelates the dragged area |
| Numbered step | N | Click to drop 1, 2, 3, ... (renumbered after undo or erase) |
| Ruler | M | Measures distance in screen pixels |

The toolbar groups the tools into freehand, shapes and markup. The selection
highlight glides to the tool you pick, unless Windows animation effects are off.
Its translucent surface blurs the capture behind it. Hover any control for its
name and shortcut.

- Right-click or right-drag erases the annotation under the cursor.
- Ctrl+Z undo, Ctrl+Y or Ctrl+Shift+Z redo.
- The color dot shows the current color and line size. It opens the palette and
  the S/M/L line sizes.
- Clear all asks for a second click.
- When the full toolbar would be wider than the selection, the tool groups
  collapse into buttons that show the last tool used in each group, and Redo,
  New capture, Save and Clear all move into the ⋯ menu.
- Zoom (1x to 5x) is a view-only magnifier, controlled from the pill in the
  selection's bottom-right corner. Drag to pan, and click Done to return to
  editing. Copy or Save while zoomed exports the enlarged image without
  annotations, as in Noctalia.
- Drag any empty part of the toolbar to move it. New capture starts again.
- Esc closes an open popover first. Clicking outside the region and toolbar
  closes the editor, as does Esc.

Copies go to the clipboard as PNG and as a bitmap. Saves go to
`Pictures\Screenshots` (or `Pictures`). When previews are disabled, the save
notification opens the file.

After Copy or Save, a floating thumbnail slides into the bottom-right of the
selection monitor's work area, above its taskbar. It uses that monitor's DPI.
Up to five previews stack with the newest at the bottom. Hover pauses the
six-second dismissal timer and shows Copy, Save, Show in folder and Close.
Show in folder saves copied captures first. Saved captures reuse their file.

Drag the image into Explorer or an application's file-drop area to copy a real
PNG. The OLE drag also offers PNG and DIB image data, with a preview drag image.
Click the preview to open a fresh editor over the exported pixels at the original
screen position, clamped to that monitor's work area. Images larger than the work
area are centered with offscreen edges clipped, preserving all original pixels
in exports. Esc closes the reopened editor.

Previews stay topmost without taking focus or appearing in Alt+Tab or the
taskbar. They are hidden before freezing the desktop (with a compositor flush)
and stay hidden while the picker/editor is open. Only a scaled preview and a
file path stay in memory. Copied captures use
`%TEMP%\Glazier-thumbnails`; their PNGs remain until exit so drop recipients
can finish reading them. Clean exit and the next startup remove Glazier's cache
files, including files left by a crash. Saved screenshots are never cleaned up.

## Recording

Press **Win+Shift+R** or choose **New recording** from the tray. The same frozen
picker opens: hover to snap to a window, drag a region, Enter accepts, and Esc
or right-click cancels. Recording has its own controls and shortcut, separate
from the screenshot toolbar.

The live desktop returns after selection. A dashed border shows the exact
recording area, with a small bar below it when there is room, otherwise above,
otherwise inside the area near its bottom edge, always within the work area.
Selections spanning monitors are clamped to the monitor containing the largest
part of the selection. An odd width or height is cropped by one pixel for H.264.
Review the adjusted border, toggle the microphone and system sound icons, then
click the red record button. Hover any icon for a tooltip. Highlighted toggles
are on. System sound defaults on and the microphone off. These choices persist in
`%APPDATA%\Glazier\state.toml`, keeping comments in `config.toml` intact.

During recording, the bar shows a pulsing dot with the elapsed time, then
pause/resume, stop and save, the live microphone and sound toggles, and discard. Pauses are removed from both
video and audio timestamps. Stop also works through **Win+Shift+R** and the
tray's **Stop recording** item. The tray changes to a red R while recording.
Controls do not take focus, and the border and bar are excluded from capture.
Screenshots remain available during a recording.

Recordings are H.264 MP4 with one mixed AAC track for system loopback and mic.
If neither source was enabled, the final file has no audio track. Muting keeps
audio clients alive; enabling a source first opens its device. Missing or
disconnected audio devices produce a tray notification while video continues.
Toggle the affected source off and on to reconnect after a device change.
Windows Graphics Capture includes the cursor and crops the selected monitor
on the GPU. Media Foundation prefers hardware encoders, with software fallback.
Capture, mixing and encoding run on a separate worker with bounded buffers.

Files go to `Videos\Screen Recordings`. The timestamp filename style follows
screenshots, with `recording-` as the default prefix. A temporary MP4 is finalized
before it is renamed to its final path. Discard removes the temporary file.
After stopping, a floating poster preview uses the existing thumbnail system:
click opens the default player, drag offers the MP4 file, and hover shows
**Show in folder**, **Copy** (the file), and **Close**.

Recording supports Windows 10 2004 or later, which provides
[capture exclusion](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity).
Windows may retain its own capture indicator on versions that
do not allow applications to suppress it. Very large regions or slow software
encoders can reduce the delivered frame rate.

## External recording controls

Glazier exposes [control protocol v1](docs/control-protocol.md) for shells and
other apps. For example, `glazier.exe --record --fullscreen --system-sound on
--mic off`, then `--pause`, `--resume`, `--stop` or `--status`. A plain `--record`
opens the region picker and starts after selection. Tray/hotkey recordings keep
their existing review step and are visible through the same status interface.
Status and stop never start the tray app. No installed binary is updated by building.

## Shell integration

A host shell can bundle Glazier, start it hidden in the tray, and own the
capture shortcuts itself:

```powershell
glazier.exe --no-hotkeys --theme-file "C:\path\to\theme-tokens.json"
```

- `--theme-file <path>` uses that theme-token JSON instead of `config.toml`'s
  `theme_file`. Like `theme_file`, it is read again at each capture or
  recording, so editing the file affects the next one. It survives **Reload
  settings**. Launch options apply to the process that starts the host; a
  command forwarded to an already running host keeps that host's appearance.
- `--no-hotkeys` does not register PrintScreen, Win+Shift+S or Win+Shift+R, so
  the shell can own them and trigger `glazier.exe --capture` and the recording
  command itself. The tray menu, capture and recording commands, and control
  protocol v1 continue to work, and Escape from the editor is still routed.
  No window appears at startup; only the tray icon is shown.

Launch options may be combined with a command that starts the host, for example
`glazier.exe --no-hotkeys --capture`. When Glazier is already running, a later
`--capture`, `--record`, `--stop`, `--pause`, `--resume` or `--status` is
forwarded to it through the control protocol rather than starting a second
instance.

## Settings

The tray menu has **Edit settings** (`%APPDATA%\Glazier\config.toml`),
**Reload settings**, **Start with Windows** and **Open screenshots folder**.
The file is commented. It covers shortcuts, save folder, file name format,
default tool, color and size, which toolbar groups to show, and
`toolbar_theme` (`"auto"` follows the Windows app theme, or `"dark"` / `"light"`).
`show_thumbnail = true` enables previews; `thumbnail_seconds = 6` sets their
unhovered lifetime in seconds (`0` keeps them until closed). Reload applies
these settings to existing previews as well as future captures.

`theme_file` optionally points to an absolute path for a shared theme-token
JSON file. `--theme-file <path>` overrides it for the host that launch starts.
For Caelestia for Windows, set it to the full path of
`%LOCALAPPDATA%\caelestia-windows\state\caelestia\theme-tokens.json`. A TOML
literal string avoids backslash escaping, for example:
`theme_file = 'C:\ExampleProfile\state\caelestia\theme-tokens.json'`.
A custom Caelestia profile uses its own `state\caelestia\theme-tokens.json`.
Reload settings after changing the path. Glazier reads the file again at the
next capture. Missing or invalid tokens, including an unreadable supplied font
file, retain the built-in appearance. The theme file is limited to 64 KiB;
both paths must be plain local drive-letter paths, not network or device paths.
The theme affects controls only, not
screenshot pixels or annotation strokes.

Version 1 is a JSON object with `version: 1`, `mode` (`"dark"` or `"light"`),
`colours` (six-digit RGB strings, with or without `#`), `font` and `radii`.
The required colour keys are `surface`, `surfaceContainerLow`,
`surfaceContainer`, `surfaceContainerHigh`, `surfaceContainerHighest`,
`onSurface`, `onSurfaceVariant`, `primary`, `onPrimary`, `primaryContainer`,
`secondaryContainer`, `outline` and `error`. `font.family` is the UI family;
`font.file` is an optional absolute path loaded privately by DirectWrite.
`radii` contains nonnegative `small`, `normal`, `large` and `full` values in
logical pixels. The first three are limited to 4096.

`recording_hotkey = "Win+Shift+R"` controls the separate recording shortcut
(empty disables it). `recording_directory = ""` uses Videos\Screen Recordings
and accepts environment variables for a custom folder. `recording_fps = 30`
sets the target frame rate, clamped to 1 through 60. `filename_format` also
applies to recordings. Audio preferences live in the separate state file.

## Build

Requires Rust (MSVC toolchain) on Windows 10 1809 or later.

```powershell
cargo build --release
.\target\release\glazier.exe
```

Run `cargo test` for the automated tests. With the release app running,
`cargo test real_input_desktop -- --ignored --nocapture --test-threads=1`
briefly operates the desktop through real `SendInput` keyboard/mouse events.
It checks both shortcuts, focus loss, text cancellation, selection on another
monitor, a spanning selection, pixel-exact clipboard output, and repeated closing.
It replaces the clipboard during this opt-in check.

`cargo test real_input_thumbnails -- --ignored --nocapture --test-threads=1`
checks both monitors' previews, hover pause and controls, reopening, capture
exclusion, a real PNG drop into a scratch Explorer folder, saving before reveal,
and auto-dismiss. It removes its scratch files. The separate
`real_input_thumbnail_settings_and_exit` opt-in test temporarily edits and restores
the settings file, checks live reload and zero lifetime, then exits Glazier to
verify temporary-file cleanup. Relaunch the release app after that test.

`cargo test real_input_recording -- --ignored --nocapture --test-threads=1`
temporarily changes and restores settings and audio preferences, operates both monitors,
and plays a short generated tone. It checks the recording shortcut, odd-size
cropping, pause/resume duration, hotkey/button stop, screenshot capture during
recording, poster previews, file clipboard data, Discard, and video-only MP4.
Media Foundation decodes the finalized files to verify H.264/AAC, dimensions,
timestamps, non-silent loopback samples, and exclusion of controls deliberately
placed inside the selected area. It toggles the mic but does not require a
physical mic signal. It also checks live unmute after starting with both sources
off and stopping from the tray. Scratch files are removed. The regular suite covers
region clamping, even dimensions, pause math, audio state round trips, and the
recording hotkey parser, and finalizes and decodes a short software-encoded MP4.

## Differences from the Linux version

- Upload/share is not included. It was disabled by default upstream.
- Capture freezes the entire virtual desktop, allowing selections across monitors.
- Keyboard shortcuts beyond Esc and Ctrl+C were added (undo, redo, save,
  single-key tools, Enter in the picker).
- The toolbar was redesigned: a translucent grouped bar with a compact layout
  for narrow selections, zoom on the selection, and a two-step Clear all.
- Clicking with the pencil leaves a dot, and clicking elsewhere with the text
  tool commits the pending text instead of discarding it.

## Credits

Editor behavior and the original toolbar design are from the Noctalia Screen Toolkit
(`noctalia-dev/legacy-v4-plugins`, MIT). Icons are Tabler Icons (MIT), from
the font bundled with Noctalia shell. See `THIRD_PARTY_NOTICES.md`.
