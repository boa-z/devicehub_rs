# devicehub_rs

An open-source iOS **Device Hub** in Rust: a live screen mirror of an iOS device
plus full remote control. Tap, drag, swipe, type, and hardware buttons over
Apple's CoreDevice (DDI) services. No Xcode required.

It's a thin GUI on top of the [`idevice`](../idevice) crate's CoreDevice
`display_stream` + `hid` support:

- **Screen** — `com.apple.coredevice.displayservice` streams plaintext
  RTP/HEVC, which we depacketize (RFC 7798) and decode with `ffmpeg`.
- **Input** — `com.apple.coredevice.hid.*` carries touch (resolution-independent
  normalized coordinates), keyboard, hardware buttons, and the digital crown.

## Requirements

- An iOS device paired and reachable over usbmuxd (USB or network)
  with developer mode on and the DDI mounted.
- `ffmpeg` on your `PATH` (used to decode HEVC). On macOS: `brew install ffmpeg`.
- The device must support the CoreDevice DDI display + HID services (recent iOS).

## Run

```sh
cargo run --release                              # first connected device
RUST_LOG=devicehub_rs=info cargo run --release   # with logs
```

A window opens showing the device screen and a side panel of hardware buttons.

## Controls

| Host gesture | Device action |
| ----------------------- | ------------------------ |
| Click | Tap |
| Press, move, release | Touch drag (move/slider) |
| Two-finger scroll | Live touch scroll (follows the trackpad) |
| Type | Keyboard input |
| Side-panel buttons | Home / Lock / Volume / Mute / Siri |
| Copy on either side | Clipboard auto-syncs host ⇄ device |

Special keys (Enter, Backspace, Tab, Esc, arrows, Delete, Home/End/PageUp/Down)
are forwarded as their HID usages.

## Clipboard sync

Text copied on either side is mirrored to the other automatically, over the
`com.apple.coredevice.pasteboardservice` XPC service.

## VNC server

A device can also be exposed over VNC, so any standard client (TigerVNC,
RealVNC, macOS Screen Sharing, etc) can mirror and control it without the egui
window. In the side panel under "VNC server", set the **Port** (default `5900`)
and tick **Enable**; the status line shows the bound address and connected-client
count. Then point a client at `localhost:<port>`.

The server binds to `127.0.0.1` (loopback) only. There is no real
authentication, but if your client asks for a password (older clients always do),
type anything: the server offers VNC authentication but accepts **any** password.
MacOS screen sharing is silly, I guess.

Details (`src/vnc.rs`):

- **Protocol:** RFB 3.8, Raw encoding (full frames throttled to ~30 fps). It
  honours the client's `SetPixelFormat` (true-colour 16/32-bpp).
- **Orientation:** frames are rotated upright and pointer coordinates inverse-
  rotated back into the device's native touch space, reusing the same
  `unrotate_norm` mapping as the UI. On rotation/resolution change it sends an
  `ExtendedDesktopSize`/`DesktopSize` update to clients that support it.
- **Input:** left button -> live touch (down/move/up, so press-and-hold and drag
  work); keysyms map to the same HID usages as the UI, including ⌘/⌃/⌥ chords.
- **Auth:** offers "None" and VNC authentication, accepting any password (the
  bind is loopback-only). Older clients that always prompt — type anything.
- Compression (zlib/Tight) and wheel-to-scroll aren't implemented yet (drag
  still scrolls).

## MCP server (agent control)

The device is also exposed as an [MCP](https://modelcontextprotocol.io) server,
so an AI agent can use the iPhone the same way a person does through the UI:
**look at the screen, then act, then look again.** It's a third frontend onto
the same live session as the window and the VNC server.

It starts automatically with the app and serves streamable HTTP at
`http://127.0.0.1:8009/mcp` (override the bind with the `DEVICEHUB_MCP_ADDR`
environment variable — e.g. `0.0.0.0:8009` to reach it from another machine;
there's no auth, so keep it on a trusted network).

Point any MCP client at it, e.g. for Claude Code:

```sh
claude mcp add --transport http devicehub http://127.0.0.1:8009/mcp
```

Tools (`src/mcp.rs`):

- **`screenshot`** — returns the current screen as a PNG plus its pixel size.
  This is the agent's vision; tap/swipe coordinates are pixels in this image. A
  labeled coordinate grid is overlaid by default (magenta lines every 100px,
  values labeled on all four edges every 500px) so a model can read coordinates
  straight off the image rather than estimating — pass `grid: false` for a clean
  shot.
- **`tap`**, **`swipe`** — pointer input at screenshot pixel coordinates
  (swipe synthesizes a down/move/up drag whose `duration_ms` sets the velocity).
- **`type_text`**, **`press_key`** (enter, escape, arrows, …), **`press_button`**
  (home, lock, volume, mute, siri, action), **`rotate`** (left/right).
- **`list_devices`**, **`connect_device`** (waits for the screen to stream),
  **`status`** (active device, stream state, screen size, orientation).

Like the VNC server it reads the same `FrameSlot` and writes the same
`InputSink`, and it holds the picker slots + the manager's `ControlCmd` channel
so it can list and switch devices. Coordinates use the same `unrotate_norm`
orientation math as the UI/VNC, so taps land where the agent aimed even when the
device is rotated. A human can watch (and take over) in the window while the
agent drives. See `mcp::serve` (spawned in `main.rs` alongside `vnc::supervise`).

## Architecture

- The egui UI runs on the main thread.
- A dedicated tokio runtime thread owns the device session: the tunnel, the
  screen media stream, the `ffmpeg` decoder, and the HID surfaces.
- They communicate only through shared slots + an input-command channel
  (see `src/protocol.rs`):
  - decode task → `FrameSlot` → UI, any VNC client, **and** the MCP server
    (each tracks the latest frame by version; intermediate frames dropped)
  - UI / VNC / MCP → `InputCmd` channel → session input loop

```
displayservice - RTP/HEVC -> depacketize -> ffmpeg -> PPM -> texture
hid.universal / hid.indigo <- tap/drag/key/button <- egui input
```

## License

This code is licensed as MIT for non-commercial usage.
Commercial users can reach out to Jackson Coxson for a written and explicit
license.

If you're making money, you need an explicit license from Coxson.
