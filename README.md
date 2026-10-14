# NektoMe Client

A cross-platform client for the NektoMe anonymous chat service implemented in
**Rust**, with a typed domain/protocol core and Tokio WebSocket transport. The
wire protocols (text chat + voice roulette) were recovered from
the official Android APKs (`NektoMe_4.1.7_APKPure.xapk` and the companion
`com.nektome.chatruletka.voice` 1.7.1) via static analysis; see
[docs/protocol.md](docs/protocol.md).

## Status

- `cargo build` — clean
- `cargo test` — passing Rust auth and protocol compatibility tests
- Rust currently supports a native desktop UI with persistent device/token identity,
  text-chat authentication, search, stop, dialog tracking, message history, sending,
  and read receipts. Voice Roulette signaling is also ported: register, scan, stop,
  Engine.IO heartbeats, SDP/ICE wire encoding, and Opus/G.711 codecs. Use `cargo run -- --cli`
  for the terminal diagnostic client.
- Portable audio I/O is implemented through queued PCM/WAV sinks, linear resampling,
  and CPAL microphone and speaker streams. `./run.sh` and `run.bat` enable native
  audio automatically on their supported platforms; Linux requires ALSA development files.
- The default `webrtc-media` feature enables the Rust WebRTC peer wrapper with a
  selectable Opus (default), PCMU, or PCMA audio track, offer/answer handling, bidirectional ICE signaling, RTP output,
  decoded remote audio delivery, TURN/STUN extraction, and microphone-to-RTP wiring:
  `cargo check`. Use `--no-default-features` for signaling-only builds.
- Voice identity and resume identifiers are persisted in `~/.nektome-client/voice-user.json`;
  the UI includes mute, hangup, and trouble-report controls.

## Run

```bash
./run.sh # includes live microphone and speaker devices when supported
# force native audio, or disable it for signaling-only diagnostics
./run.sh --native-audio
./run.sh --no-native-audio
# optimized build with LTO and stripped symbols
./run.sh --release
# or: cargo run
```

On Windows, use the equivalent batch launcher from Command Prompt or
PowerShell:

```bat
.\run.bat
.\run.bat --release
.\run.bat --test
```

Windows 10 uses the DirectX 12 or Vulkan renderer and intentionally skips the
OpenGL backend because some AMD OpenGL drivers can terminate the process during
adapter discovery. Set `WGPU_BACKEND=dx12` or `WGPU_BACKEND=vulkan` to force one
of the supported Windows renderers when troubleshooting a specific driver.
The checked-in Cargo configuration targets the original x86-64 CPU baseline;
release builds do not require AVX, AVX2, FMA, or CPU-specific instructions.

The Rust desktop window provides Connect, Disconnect, Find partner, Stop, Leave
dialog, message history, sending, and an event log. The diagnostic client accepts
`/connect`, `/search`, `/stop`, `/leave`, `/send <text>`, and `/quit`.

The Rust desktop window has two tabs:

- **Text chat** — Connect / Disconnect, Find partner / Stop, Leave dialog, message
  input (Enter sends), auto read-receipts, Chrome-assisted reCAPTCHA verification,
  and a raw event log for protocol debugging.
- **Voice roulette** — Connect to the audio signaling server, search filters,
  Find peer / Stop, new identity, mute / hang up / report trouble, online counters, captcha
  resume, captcha counters/metadata, and media-state and event logs. Sex and age filters are sent in the
  same `searchCriteria` shape as the reference client; peer age accepts
  multiple comma-separated ranges. Auto-next is optional
  and persisted. The default build keeps
  decoded remote PCM in a bounded queue; the standard launcher enables `cpal-audio`
  for live speaker and microphone devices when supported. Set `NEKTOME_AUDIO_CODEC=pcma` to negotiate G.711
  A-law (Opus is the default; use `NEKTOME_AUDIO_CODEC=pcmu` for G.711 μ-law).
  Audio is exchanged as mono PCM → Opus by default, or 8 kHz G.711 when a
  G.711 codec is selected. On Linux, the CPAL build additionally requires the ALSA development package
  (`libasound2-dev`/`alsa-lib-devel`).
  Incoming RTP uses a bounded adaptive jitter buffer (60–200 ms). It reorders
  packets, uses Opus in-band FEC or packet-loss concealment for missed frames,
  conceals G.711 gaps, and gradually corrects playback-device clock drift. The
  call status shows the active buffer delay plus RTP and speaker underruns.
  Opus transmission adapts its bitrate, expected-loss protection, FEC, and
  20/40 ms packet size from the measured path quality.
  Signaling reconnects use bounded exponential backoff, cancel superseded
  retries, and preserve an active WebRTC call while registration is restored.
  Enable **Web signaling mode** in the voice tab to use the raw browser-style
  WebSocket transport and its challenge/fingerprint handshake.
  For signaling-only diagnostics, set the Remote WAV field (or
  `NEKTOME_REMOTE_AUDIO_WAV=/path/call.wav`) to record decoded peer audio
  instead of retaining it only in the in-memory queue.
  When native audio is enabled, the microphone path applies noise suppression
  (RNNoise), noise gate, compressor, and makeup gain;
  the voice tab exposes persisted speaker gain/DSP, microphone gain (25–400%),
  microphone noise suppression, microphone DSP, echo-cancellation, and advanced
  gate/compressor controls for that path. Native builds also expose a microphone loopback test
  that plays captured input through the selected speaker device; stop an active
  call before starting the test. Microphone and speaker selections are saved;
  use **Refresh** after connecting a device. If a saved device is missing, the
  next call or test uses the current system default. Enable **Save calls to Downloads as stereo**
  to save each call as FLAC (the default), MP3, or Opus at 48 kHz, with the
  local microphone on the left channel and the peer on the right. Recordings use 3 dB of encoder
  headroom to prevent clipping artifacts at full scale. The recording library
  lists finalized sessions with date, size, and format and can play, reveal, or
  delete them after confirmation.
  During calls, mic mute (`M`) and local speaker mute (`S`) are available from
  the toolbar. Optional push-to-talk keeps the microphone muted until Space is
  held, while leaving session recording and remote playback routing intact.
  **More → Export diagnostics JSON** writes a support snapshot to Downloads
  with build/platform details, connection and audio metrics, device routing,
  recording settings, and bounded logs. Credentials and proxy configuration
  are excluded.

Tokens persist to `~/.nektome-client/token.txt`, the stable device identity to
`~/.nektome-client/device.json`, the voice identity and resume identifiers to
`~/.nektome-client/voice-user.json`, and voice roulette filters to
`~/.nektome-client/voice-preferences.json` (including the Android profile
user-agent, locale, and timezone). Delete these files to start fresh.
Both WebSocket transports fail a stalled Engine.IO handshake after 15 seconds.
Production endpoints can be overridden for staging or diagnostics with the
NEKTOME_CHAT_SERVER, NEKTOME_VOICE_SERVER, and NEKTOME_WEB_VOICE_SERVER
environment variables; unset variables retain the bundled endpoints.
For legacy deployments that publish an audio-server bootstrap document, set
NEKTOME_VOICE_BOOTSTRAP_URL to a JSON endpoint containing socket, url, and
optional path fields. A non-enabled document is rejected.
The voice tab's server button selects the bundled `.me`/`.kz` hosts and persists
an explicit choice; until changed, environment and bootstrap endpoint overrides
remain authoritative.
Set `NEKTOME_PROXY=http://[user:password@]host:port`,
`NEKTOME_PROXY=socks5://[user:password@]host:port`, or
`NEKTOME_PROXY=socks5h://[user:password@]host:port` to route chat and voice
  WebSockets through a proxy; bare `host:port` entries are treated as HTTP
  CONNECT proxies. `socks5` resolves destination hostnames locally, while
  `socks5h` delegates hostname resolution to the proxy. With the default WebRTC build, TURN-over-TCP endpoints are
  also forwarded through the selected proxy for media. The voice UI also accepts multiple proxy URLs
  separated by commas, semicolons, or newlines and tries them in order.
Text-chat locale and timezone metadata can be customized with
`NEKTOME_LOCALE` and `NEKTOME_TIME_ZONE`.
Android voice profiles can be imported from a rooted emulator with the
`Import from emulator` button when `adb` is available; it reads the voice
service preferences database and safely disconnects the active voice session
before applying the imported identity.
  The ◀/▶ controls select a specific entry and reconnect the active voice
  session through that proxy.
  The voice tab can also persist a Proxy-Seller API key and fetch the configured
  resident pool (reusing or creating the app-owned list) directly into the
  proxy pool.
  Android voice mode also exposes a broad randomized device-profile set for
  varying the transport user-agent between sessions.

## Rust architecture

The active application is the Rust binary crate at the repository root:

```
eframe UI / terminal CLI
          │
          ▼
  app + voice session state
          │
   ┌──────┴────────┐
   │               │
chat protocol   voice protocol
   │               │
Tokio WebSocket  Tokio WebSocket (Android or Web) ── optional WebRTC/CPAL media
   │               │
file persistence  Opus/G.711 + ICE/SDP
```

`--no-default-features` keeps the signaling-only build; the default build
includes the WebRTC media layer and a pure-Rust Opus codec. `cpal-audio` adds native microphone
and speaker devices.

## Rebuilding from the APK findings

Protocol facts (endpoints, action names, payloads, auth key derivation) were
recovered by static analysis of the Android APK and are documented in
[`docs/protocol.md`](docs/protocol.md). The analysis workspace with the
APK-derived endpoint report lives in `/Volumes/External/Code/apk_flash`
(`reports/nektome_4.1.7_endpoints.md`).
