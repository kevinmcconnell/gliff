# Plan: a native macOS client

A Mac app that connects to a Hyprland machine running `gliff-server`, just as
the GTK client does. The server stays Linux and Hyprland only; this plan is
about the client. Builds run on `nyc-m4` (Apple Silicon M4).

## What carries over and what does not

The GTK client is three layers. Each has a different fate on macOS:

| Layer | Today | On macOS |
|---|---|---|
| Protocol, framing, ssh | `gliff-proto`, `gliff-transport`, `gliff/src/net.rs` | **Reuse.** Pure Rust and tokio. `net.rs` needs its Linux calls moved out first (phase 1). |
| Decode and recombine | `gliff-vk`: Vulkan Video H.264 decode, `recombine.comp` | **Replace.** MoltenVK has no Vulkan Video. Use VideoToolbox for decode and a Metal port of `recombine.comp`. |
| Display and input | GTK4 + libadwaita, dmabuf textures, xkb, Wayland shortcut inhibit | **Replace.** AppKit window, `CAMetalLayer`, `NSEvent`, a key-code table, `CGEventTap`. |

## Status (2026-09-21)

Built and working: `Gliff.app` on nyc-m4 (M4 Mac mini, macOS 15.5)
connects to nyc-a2 (AMD, Hyprland) over ssh (`gliff-server --stdio`) and
over the TCP dev path. Checked live:
- 60 fps Dual420 at 3.4 ms for decode plus recombine, 1080p class
- mirror mode (scaled to the window) and headless mode (resized to the window)
- text typed from the Mac arriving in a terminal on the remote
- the clipboard in both directions
- the decoded frame and the drawn view, saved as PNGs and checked

Phases 0–7 are done, and phase 8 in part: ad hoc signing and a CI job that
ships the zip with each release.

Not done, or only checkable by a person at the Mac:
- Real keyboard and mouse events. The key table, modifier bits and
  scroll signs are unit-tested, and `--type` drives the same session
  calls, but no physical key press has been sent. The ISO section/grave
  swap needs an Apple ISO keyboard to confirm.
- The system-shortcut event tap: it needs the Accessibility permission,
  granted by a person.
- The remote cursor image: Hyprland currently sends only transparent
  cursor images (see `docs/hardware-quirks.md`), so the arrow shows.
- Developer ID signing and notarization (open decision 2). Until then,
  release builds are ad hoc signed.

The only protocol change was for keyboard layouts (phase 7): version 3
lets `Hello.keymap` carry `rmlvo:` names for the server to compile.

## Architecture

```
  Swift / AppKit (gliff.app)                    Rust core (libgliff_client.a)
 ┌──────────────────────────────────┐          ┌───────────────────────────────┐
 │ NSWindow + GliffView (CAMetalLayer)│ ◄─frame─ │ session loop (from net.rs)    │
 │   ▲ BGRA MTLTexture               │          │   handshake, reader, writer   │
 │   │ recombine.metal               │          │   task, acks, stats           │
 │ two VTDecompressionSessions ◄─────┼─ on_video┤   clipboard reassembly        │
 │   (NV12 IOSurface CVPixelBuffers) │ callback │                               │
 │ NSEvent → evdev codes ────────────┼─ C ABI ─►│ input queue → writer task     │
 │ NSPasteboard, NSCursor            │          │ spawn_ssh / TCP               │
 └──────────────────────────────────┘          └───────────────────────────────┘
```

- **Rust owns the session.** The network worker in `crates/gliff/src/net.rs`
  is mostly platform-neutral. The exceptions are `gliff-vk`,
  `crate::keymap::local_keymap()` (Hyprland IPC and `hypr-input`) and
  `hypr_capture::render_node`. It moves into a new
  crate, `gliff-client`, that takes the decoder as a trait. The GTK client
  plugs in `gliff-vk`. The Mac app plugs in a C callback into Swift. This way
  there is one handshake, one ack-pacing loop and one clipboard codec, not a
  second copy of postcard in Swift that can drift.
- **Swift owns the platform.** VideoToolbox, Metal, AppKit and the
  pasteboard are native Swift APIs. From Rust they would all be `unsafe`
  objc2 calls, and Xcode's GPU tools work best on Swift and Metal code.
- **Decode stays synchronous in the ack path.** The Rust reader calls
  `on_video(frame_id, main, aux)` on its worker thread. Swift decodes both
  streams, submits the recombine, waits for its command buffer to complete
  (not for display), and returns a status code. Only then does Rust send
  `FrameAck`, so the server's ack window measures real decode time.
- **Exactly one ack per video message, whatever happens.** The server
  frees a slot in its in-flight window only on `FrameAck`;
  `RequestKeyframe` frees nothing. The Linux client already acks errors
  and frames that produce no output. The core keeps that rule in one place,
  so no sink can drop an ack. An error return also sends
  `RequestKeyframe`. Add a test for repeated errors.
- **The GPU path stays zero-copy.** VideoToolbox writes into IOSurface-backed
  NV12 `CVPixelBuffer`s. `CVMetalTextureCache` wraps each plane as an R8 or
  RG8 `MTLTexture` without a copy. The Metal compute kernel writes a BGRA
  texture that the layer presents. The CPU only handles coded bytes.
- **Frame ownership is explicit.** Retaining an `MTLTexture` alone is not
  enough: the `CVMetalTexture` wrapper and its `CVPixelBuffer` must also
  stay retained until the recombine command buffer completes. Release them
  in its completion handler. Each BGRA output texture in the pool belongs
  to the display until the presenting command buffer completes. If the pool
  runs out, drop the new frame (latest-wins) and still ack it; never block
  the reader on the display.

## Phases

Each phase ends with something runnable on nyc-m4.

### 0. Build host and scaffolding

- **Done (2026-09-21).** nyc-m4 is an Apple M4 on macOS 15.5 with
  Homebrew, the Command Line Tools 16.4 (Swift 6.1.2), and `brew install
  rustup cbindgen` (Rust 1.98.1). Not Homebrew's `rust`: its standard
  library is built for macOS 15, which the linker flags for a macOS 14 app. No full Xcode. Checked there: SwiftPM
  builds, Metal compiles shaders at runtime, and VideoToolbox reports
  hardware H.264 and AV1 decode. The Command Line Tools have no XCTest, so
  Swift tests use `swift-testing` (`import Testing`), which works.
- **Done.** `scripts/mac-build.sh` rsyncs the working tree to
  `nyc-m4:~/src/gliff` and runs a command there. By default it runs the
  portable crates' tests, plus `swift build` and `swift test` once
  `macos/` exists. It reuses the ssh master at `~/.ssh/cm-nyc-m4`. The
  default now builds `dist/Gliff.app` with `macos/build.sh test`.
- Build with no Xcode project: a SwiftPM package in `macos/` for the app,
  plus a small script that assembles `gliff.app` (Info.plist, binary,
  resources) and signs it ad hoc. Everything then runs headless over ssh,
  and there is no `.pbxproj` in diffs.
- Compile the Metal shader at runtime from bundled source
  (`MTLDevice.makeLibrary(source:)`). Command-line SwiftPM does not build
  `.metal` files. Recent Xcode ships the offline Metal compiler as a separate
  download, so a runtime compile keeps the build dependency-free. A
  precompiled `.metallib` can come later if startup time matters.
- Cargo workspace: the Linux-only crates (`hypr-*`, `gliff-vk`,
  `gliff-server`, `gliff-probe`, the GTK `gliff`) must stay out of a macOS
  build. Build with `-p gliff-client -p gliff-ffi` there, and gate
  `default-members` if needed.

### 1. Extract the client core (`crates/gliff-client`)

- Move `Worker`, `Endpoint`, `Status` and `session()` out of
  `crates/gliff/src/net.rs`. `session()` becomes generic over
  `trait VideoSink { fn configure(&mut self, &StreamConfig) -> Result<()>;
  fn decode(&mut self, frame_id, main: &[u8], aux: &[u8]) ->
  Result<Option<Self::Frame>> }`. `configure` can fail, because decoder
  creation can.
- Inject what is Linux-specific today. The caller passes in the keymap text
  and the `ClientCaps`: the Mac's maximum size is not the hard-coded
  3840x2160, and the server clamps resizes to whatever the client
  advertises. The Linux sink opens its own `Gpu`, so `render_node` stays
  out of the core.
- Add a cancellation path. Today the worker only ends when the socket
  closes. Give it a stop signal that the reader loop `select!`s on and that
  kills the ssh child.
- Also move the platform-neutral helpers that the Mac needs too:
  `has_visible_shape`, the letterbox and scale maths in `to_remote`, and
  the hotkey model (as data, without `gdk::Key`).
- `crates/gliff` keeps its behaviour and uses the crate with a `gliff-vk`
  sink. `scripts/e2e.sh` does not cover this: it drives
  `gliff-probe serve-test`, not the GTK client or `net.rs`. Add two things:
  - `gliff-client` tests against an in-memory duplex with a fake server.
    They cover the handshake, one ack per frame including errors, keyframe
    requests, a `StreamConfig` change mid-stream, and stop.
  - An e2e step that runs the real `gliff --connect` against the nested
    server for a few seconds, with a resize and a reconnect.
- This refactor is worth doing on its own even without the Mac app.

### 2. C ABI (`crates/gliff-ffi`)

- A `staticlib` with a header generated by `cbindgen`. Its surface is small:
  - `gliff_session_start(config, callbacks, ctx) -> *Session`, where
    `config` holds the host, server path, mode (mirror, `--output`,
    headless), low-bandwidth flag and keymap string, and `callbacks` has
    `on_stream_config`, `on_video`, `on_cursor`, `on_clipboard`, `on_status`
    and `on_closed`
  - `gliff_send_key`, `gliff_send_pointer_motion`,
    `gliff_send_pointer_button`, `gliff_send_axis`, `gliff_send_resize`,
    `gliff_send_clipboard_text`, `gliff_release_all_input`
  - `gliff_session_stop`
- Callbacks run on the Rust worker thread. The Swift side treats them as
  such: decode runs inline, and UI work hops to the main actor.
- **Lifetime contract.** `gliff_session_stop` signals the worker, which
  cancels the reader and kills ssh. It blocks until the worker thread has
  joined, and after that no callback runs again. Only then may Swift release
  `ctx` and the decoders. Calling stop from inside a callback is a
  programming error; the core detects it and aborts with a message rather
  than deadlocking. Each session carries a generation number, and main-actor
  work queued by an old session checks it and does nothing. Reconnect
  creates a new session; it does not reuse the old one.
- Payload pointers are only valid for the length of the callback. The
  header says so, and Swift copies what it keeps (cursor, clipboard).
  Video bytes go straight into a `CMBlockBuffer` and do not outlive the
  call.

### 3. App skeleton

- `NSApplication` with one window: a host field, a mode popup (focused
  screen, named output, headless), a Connect button, and a status line. The
  content is `GliffView`, an `NSView` backed by a `CAMetalLayer`.
- Persist recent hosts in `UserDefaults`. Follow the macOS light or dark
  appearance; the Omarchy theme code does not apply here.
- Connect over ssh through `/usr/bin/ssh`, via the existing `spawn_ssh`.
  macOS gives GUI apps `SSH_AUTH_SOCK` from launchd, and `~/.ssh/config`
  applies. There is no TTY for a password prompt, so key auth is required.
  Surface ssh's stderr in the status line rather than inheriting it, since a
  Finder-launched app has no terminal. That needs a small change to
  `spawn_ssh` to pipe stderr.
- No App Sandbox, because it forbids spawning ssh with the user's config
  and keys. Distribute outside the Mac App Store (phase 8).

### 4. Video

- **Annex B to VideoToolbox.** The server sends Annex B with SPS and PPS
  prepended to every IDR. For each access unit, split the NAL units, build a
  `CMVideoFormatDescription` from the SPS and PPS
  (`CMVideoFormatDescriptionCreateFromH264ParameterSets`), rebuild it
  whenever they change, and rewrite start codes as 4-byte big-endian lengths
  (AVCC) in a `CMBlockBuffer`. A small, unit-testable Swift parser covers
  this; `gliff-vk/src/h264/annexb.rs` is the reference.
- **Two decoders.** One `VTDecompressionSession` per stream, both asking for
  `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` with
  `kCVPixelBufferMetalCompatibilityKey` and
  `kVTDecompressionPropertyKey_RealTime`. Decode synchronously: set neither
  the asynchronous-decompression flag nor the temporal-processing flag. The
  output callback then finishes before `DecodeFrame` returns, so no extra
  wait is needed. The stream has no reordering, so each input gives at most
  one output. `Single420` uses one decoder. Apply every `StreamConfig`: it
  rebuilds the decoders and updates the stream size and the pointer scale.
- **Recombine.** Port `recombine.comp` and `common.glsl` to a Metal compute
  kernel with the same BT.709 limited-range constants. It reads four plane
  textures and writes `bgra8Unorm`. Do not carry over the `.bgr` swizzle:
  the Vulkan shader needs it because it views a BGRA image as RGBA, but
  Metal's `bgra8Unorm` stores logical RGBA in BGRA order itself. Keeping the
  swizzle would swap red and blue. Pure-red and pure-blue fixtures catch
  this. A pool of three output textures avoids waiting on the display, with
  the ownership rules above.
- **Present.** Blit or draw the BGRA texture into the layer's drawable,
  letterboxed. Keep the GTK client's `ScaleDown` rule: one stream pixel per
  backing pixel when it fits, shrink when it does not, never enlarge. Use
  `displaySyncEnabled` and present the latest frame only (latest-wins, as
  on Linux).
- **Verification against the real encoder.** A VideoToolbox-to-
  VideoToolbox round trip would prove nothing about gliff's streams. The
  Vulkan encoder emits SPS frame cropping and no VUI, which is exactly where
  decoders differ. So:
  - Add a capture option to `gliff-probe roundtrip` (or `serve-test`) that
    writes the main and aux Annex B streams and the source BGRA to files. Record
    IDR-plus-P sequences at 1920x1080, at a size that is not a multiple of
    16 (for example 1366x768 and 1856x1238), and across a mid-stream
    resize.
  - `swift test` replays those fixtures through the decoders and the Metal
    kernel, and compares PSNR against the source.
  - A kernel-only test feeds raw NV12 planes to Metal and checks the result
    is within ±1 of `recombine_yuv444` + `yuv444_to_bgra`.
  - Report decode and recombine times at 1080p and 3840x2160.

  Do this as a spike before phase 1 (see "Order of work"): it retires the
  biggest risk with a few hundred lines of Swift.

  **Spike result (2026-09-21).** Done: `gliff-probe roundtrip --dump` and
  `macos/` (`GliffVideo`: `AnnexB`, `H264Decoder`, `Recombiner`). On
  nyc-m4, VideoToolbox decodes the Vulkan encoder's streams, including the
  cropped 1366x768 and 1856x1238 sizes and the forced keyframe mid-run.
  The Metal recombine matches the Vulkan decoder's output to within ±1 on
  every sample of every frame, and the synthetic frames' pure red, green and
  blue stripes confirm the channel order. There was no reordering delay:
  every access unit produced its picture at once, so the SPS needs no VUI.
  Release build, both streams decoded one after the other, then the
  recombine, waiting on the GPU, averaged over 8 frames:

  | Stream | avg | worst |
  |---|---|---|
  | Dual420 1366x768 | 4.0 ms | 9.2 ms |
  | Dual420 1920x1080 | 5.4 ms | 9.8 ms |
  | Dual420 1856x1238 | 5.8 ms | 11.6 ms |
  | Dual420 3840x2160 | 13.7 ms | 23.0 ms |
  | Single420 1366x768 | 4.7 ms | 22.9 ms |

  The worst cases are the first frame (session setup and allocations). To
  make 4K Dual420 fit comfortably inside 16.7 ms, decode main and aux on
  two threads at once and reuse a pool of output textures instead of
  allocating one per frame. Run it with
  `GLIFF_VT_FIXTURES=<dump dir> swift test -c release --no-parallel`.

### 5. Input

- **Keys.** Build a static table from macOS virtual key codes (`kVK_*`) to
  evdev codes, with a unit test that the table is injective. Handle
  `keyDown`/`keyUp` plus `flagsChanged` for modifiers, since modifiers send
  no key-down. Caps Lock arrives as one toggle, so send a press and a
  release. Mapping: Command → `KEY_LEFTMETA`/`KEY_RIGHTMETA` (Super, which
  Hyprland binds), Option → Alt, Control → Ctrl. Ignore key auto-repeat
  (`isARepeat`), because the remote repeats on its own. Track held keys and
  release them all on focus loss or deactivate, as `release_pressed_keys`
  does.
- **ISO keyboards.** On Apple ISO keyboards, `kVK_ISO_Section` and
  `kVK_ANSI_Grave` are swapped relative to a PC layout. The table needs an
  ISO/ANSI switch from `KBGetLayoutType(LMGetKbdType())`; a Danish Mac
  keyboard will show this at once.
- **System shortcuts.** A normal window never sees Cmd-Tab, Cmd-Space,
  Mission Control or Cmd-Q (the menu takes it). Two steps:
  1. Override `performKeyEquivalent` in the view and keep the menu bar
     minimal while focused. That covers most Cmd chords.
  2. An opt-in `CGEventTap`, active only while the view has focus. It
     swallows the rest and forwards it. This needs the Accessibility
     permission, so ask for it on first use with an explanation. It is the
     counterpart of `inhibit_system_shortcuts`.

  The release hotkey (default Shift+Esc) drops focus and removes the tap,
  as on Linux. While the tap is active it is the only input source: the
  view ignores `NSEvent` key events, so nothing is sent twice. macOS turns
  off a slow tap (`tapDisabledByTimeout`, `tapDisabledByUserInput`). On that
  event, release everything held, re-enable the tap, and fall back to plain
  `NSEvent` delivery if re-enabling fails. Keep the tap callback trivial:
  it enqueues and returns.
- **Release everything on focus loss.** Track held mouse buttons as well as
  keys. A focus change in the middle of a drag must not leave the remote
  button down. `gliff_release_all_input` sends the releases for both.
- **Pointer.** Map view coordinates to the remote's logical space with the
  shared letterbox maths. The Y axis is flipped in AppKit. Buttons map
  left, right and other to `BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`,
  `BTN_SIDE` and `BTN_EXTRA`.
- **Scroll.** For trackpads (`hasPreciseScrollingDeltas`), send continuous
  values with `discrete: None` and `stop: true` when the gesture or momentum
  phase ends. For wheels, send `discrete: ±1`. macOS has already applied the
  user's natural-scrolling setting to the deltas, so negate them to match
  the GTK sign convention and send the result without a second inversion.
- **Cursor.** Turn `CursorShape` ARGB into an `NSImage` sized in points
  (pixels / backing scale), then an `NSCursor` with the hotspot, set through
  a cursor rect on the view. Fall back to the arrow when
  `has_visible_shape` is false.

### 6. Window, HiDPI and resize

- Measure the view in backing pixels and round down to even. Send
  `Resize { width, height, scale: backingScaleFactor }` after 200 ms of no
  change, as `install_resize_handler` does. Three sizes are in play and
  should be kept apart:
  - the client's advertised cap (`ClientCaps.max_width`/`max_height`),
    which the server clamps every resize to;
  - the server's encoder limit, which is queried from the driver
    (4096x4096 on AMD VCN) rather than fixed;
  - the remote output itself.

  In headless mode the remote output follows the window at 2x, within the
  cap. In mirror mode the output keeps its size, but the server's
  `fit_mirror` resizes the encoded stream to the window. Either way a new
  `StreamConfig` follows and the client applies it. The Mac's cap should be
  the largest backing size of any attached display, not 3840x2160, or a
  full-screen window on a 5K display will be downscaled.
- Native fullscreen (`toggleFullScreen`), with the menu bar and Dock hidden
  while the view has focus.
- On `NSWindow.didChangeBackingPropertiesNotification` (moving between a
  Retina and a non-Retina display), recompute and resize.

### 7. Clipboard, reconnect, keyboard layout

- **Clipboard.** `NSPasteboard` sends no change events, so poll
  `changeCount` about every 250 ms while the app is active, and once on
  `didBecomeActive`. Keep the same one-shot echo guard as the GTK client.
  Text only, matching the server.
- **Reconnect.** Up to five retries at 1.5 s intervals, and a reconnect
  brings the remote back to the window size. This can move into
  `gliff-client` so both clients share it.
- **Keyboard layout.** The server wants full xkb keymap text, which a Mac
  cannot produce without shipping libxkbcommon and xkeyboard-config. Send
  RMLVO names instead: map the current input source
  (`TISCopyCurrentKeyboardInputSource`, e.g. `com.apple.keylayout.Danish`)
  to an xkb layout and variant (`dk`, `mac`). On the server,
  `hypr-input` compiles the keymap when the Hello `keymap` field is not
  `xkb_keymap` text. This is an additive change in meaning, but bump
  `PROTOCOL_VERSION` so an old server fails clearly. Until then, an empty
  keymap gives `us` on the server, which is fine for a first cut.
  Follow input-source changes by reconnecting, or add a new additive
  `ClientMsg::Keymap` later.

### 8. Packaging and CI

- **Signing.** Developer ID Application certificate, hardened runtime, and
  `notarytool` notarization. The only entitlement needed is
  `com.apple.security.cs.allow-jit`, and only if the Metal runtime compile
  turns out to need it (it should not). Ship a zipped `.app` or a DMG.
- **Release.** Add a `macos` job to `.github/workflows/ci.yml` on a
  `macos-15` arm64 runner: `cargo test -p gliff-proto -p gliff-transport -p
  gliff-client`, build `gliff-ffi`, `swift build`, `swift test` and bundle.
  Do not let it upload to the release itself. The Linux `release` job
  deletes and recreates `latest`, so a separate Mac upload would race it.
  Instead, both build jobs upload workflow artifacts, and one `publish`
  job that `needs` both creates the release with the Linux tarball and
  `gliff-<version>-arm64-macos.zip`. Signing secrets go in repo secrets.
  Until those exist, ship ad hoc builds that need right-click → Open.
- Apple Silicon only (`arm64`) at first. A universal binary is easy to add
  later, but Intel Macs are not a target.
- Optionally, a Homebrew cask pointing at the release zip.

## Testing

- Rust: the existing unit tests, plus `gliff-client` tests for the session
  loop against an in-memory duplex (a fake server that sends `HelloAck`,
  `StreamConfig` and frames, and checks acks and keyframe requests).
- Swift (`swift test` on nyc-m4): the Annex B to AVCC parser against the
  x264 fixture in `gliff-vk/src/h264/testdata`, the key table, the scroll
  and coordinate maths, and the Metal recombine bit-exactness test.
- End to end: from nyc-m4, `gliff --connect <linux-box>:9000` against
  `gliff-server --listen ... --headless` in a nested Hyprland, then the real
  path, `gliff user@linux-box`. Check what `scripts/e2e.sh` checks with its
  probe client (connect, stream, both chroma modes, the clipboard both
  ways), plus resize, reconnect, and release of held keys and buttons on
  focus loss.

## Risks and unknowns

- **VideoToolbox decode latency.** VideoToolbox on Apple Silicon is fast,
  but its low-delay behaviour with our POC-type-0, one-reference stream
  needs measuring in the spike. Our SPS has no VUI, so a decoder may assume
  it can reorder frames and hold one back. If VideoToolbox does that, add a
  VUI with `bitstream_restriction_flag` and `max_num_reorder_frames = 0` in
  `gliff-vk`'s encoder. That is a server change and helps every client.
  (`kVTDecodeFrame_1xRealTimePlayback` does not help: it allows a
  low-power mode that is capped at real time.)
- **Two sessions in lockstep.** Main and aux must decode the same
  `frame_id`. Synchronous decode of both before recombining keeps this
  simple. A decode error in either one drops the frame, still acks it, and
  requests a keyframe.
- **Stream size.** The encoder limit is queried from the server's driver
  (4096 wide on AMD). M4 decodes H.264 at that size comfortably. A 5K
  Studio Display window will be scaled, as on Linux, until AV1 lands. M3
  and later decode AV1 in hardware, which suits the AV1 item in
  `docs/architecture.md`.
- **Event tap permissions** are the least pleasant part of the user
  experience. Keep the tap optional; the app works without it, minus
  Cmd-Tab and friends.
- **Remote reachability from nyc-m4.** End-to-end testing needs nyc-m4 to
  reach a Hyprland box over ssh, presumably over Tailscale. Confirm which
  machine and user to use.

## Open decisions

1. Minimum macOS version. Suggest macOS 14 (Sonoma), which covers every
   API here and most Apple Silicon machines in use.
2. Signing identity: is there a Developer ID team to notarize under, or ad
   hoc builds for now?
3. Bundle id and name: `com.gliff.Client` matches the GTK app id. Should the
   app be called "gliff"?
4. Which Hyprland host nyc-m4 should use for end-to-end tests.

## Order of work

1. (Done.) Phase 0, then the **decode spike**: dump real main and aux streams with
   `gliff-probe`, and decode and recombine them in a bare Swift test on
   nyc-m4 (the verification part of phase 4). This retires the biggest
   risk, compatibility with the Vulkan encoder's streams, before any
   refactoring.
2. Phase 1 lands on its own, since it improves the Linux client too.
3. Phases 2–4 get live video on screen on nyc-m4.
4. Phases 5–7 make it usable day to day.
5. Phase 8 makes it shippable.
