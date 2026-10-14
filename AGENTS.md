# AGENTS.md

This file applies to the entire repository.

## Project overview

This is a Rust 2024 desktop and CLI client for NektoMe text chat and voice
roulette. The UI uses `eframe`; network transports use Tokio and WebSockets;
voice calls use WebRTC with optional CPAL audio; Opus encoding and decoding are
implemented in pure Rust. Protocol compatibility details live in
`docs/protocol.md`.

The default feature set includes WebRTC and Opus. The `cpal-audio` feature adds
native microphone and speaker support. A `--no-default-features` build must
continue to support signaling-only use.

## Tokio and concurrency guidelines

Follow the principles in [Principles for fast Tokio
applications](https://dial9-rs.github.io/blog/principles-for-fast-tokio-applications/).
Optimize only a demonstrated or clearly blocking path. Preserve correctness,
protocol ordering, and readable ownership before pursuing throughput.

- Keep every async poll short. Never call blocking `std` channel receives,
  filesystem operations, process waits, sleeps, or synchronous network I/O on a
  Tokio worker.
- Use Tokio-aware APIs for bounded-duration async work. Add explicit timeouts to
  handshakes, sends, subprocesses, and other operations that can stall.
- Use bounded channels for WebSocket frames, media packets, and other sustained
  streams. Document any unbounded channel and keep its producers naturally
  bounded; the UI command mailbox is unbounded because GUI callbacks cannot
  await, so handlers must drain it promptly.
- Keep the main UI command actor responsive. Long network probes, retries, and
  maintenance work belong in cancellable background tasks. Store their
  `JoinHandle`, abort obsolete work on reconnect/disconnect/shutdown, and avoid
  sending results through a superseded transport.
- Bound concurrency explicitly for work driven by server or network input. Do
  not `join_all` an arbitrary list or spawn one task/process per untrusted item.
  Server ping probes are capped in both input count and concurrent processes.
- Batch tiny operations when throughput matters, but yield or split work when a
  batch could delay signaling, WebSocket heartbeats, RTP reads, or UI commands.
- Avoid `spawn_blocking` per packet, frame, or sample. Sustained blocking work
  such as MP3 encoding and native audio device loops should use a dedicated OS
  thread with a bounded queue and an explicit shutdown path.
- Do not add extra Tokio runtimes, worker pinning, busy spinning, or custom
  executors without measurements showing scheduler saturation and a benchmark
  demonstrating the improvement.

## Locks and shared state

- Never hold a `std::sync::Mutex` or `RwLock` guard across `.await`.
- Keep audio callback locks allocation-free and brief. Copy or drain the needed
  data, release the guard, and do expensive processing afterward.
- Keep async mutex critical sections small. Prefer actor ownership and message
  passing when state changes have a natural order.
- Do not hold locks while performing file I/O, WebSocket sends, subprocess work,
  codec setup, or device initialization.
- Use atomics only for small independent state such as mute/readiness flags.
  Use explicit memory ordering and retain stronger synchronization when values
  must change together.

## WebSocket and voice behavior

- Preserve Engine.IO ping payloads when replying with pong and retain periodic
  transport keepalives.
- A signaling reconnect during an active call must preserve the WebRTC peer and
  microphone when the session can be resumed. A user disconnect or shutdown
  must release them.
- Keep socket reads and writes independent through bounded channels. Treat a
  full or closed outbound queue as a transport failure instead of waiting
  forever.
- Preserve event ordering within a connection. When work is detached, make sure
  its result cannot mutate a newer peer, generation, or connection.
- Audio paths must not grow queues without limit. Prefer dropping stale realtime
  audio over accumulating latency, while preserving control and signaling
  messages.
- Do not perform logging, disk writes, or expensive allocation in CPAL callback
  loops. Session recording stays isolated behind its bounded recorder channel.

## Measuring performance changes

For a performance or stability change, identify the suspected delay before
editing. Useful evidence includes task schedule latency, queue depth, timeout or
reconnect frequency, audio underruns, and the duration of critical sections.
Do not infer a scheduler problem solely from total CPU usage.

Prefer focused instrumentation that can be removed or kept at low cost. Compare
the same workload before and after a change. A benchmark that improves bulk
throughput is insufficient if it increases heartbeat, signaling, or audio
latency.

## Code changes

- Keep protocol payload shapes compatible with `docs/protocol.md` and existing
  protocol tests.
- Maintain feature-gated builds. New native audio code belongs behind
  `cpal-audio`; WebRTC-specific code belongs behind `webrtc-media`.
- Keep platform-specific subprocess and launcher behavior working on macOS,
  Linux, and Windows. Do not introduce native Opus library requirements.
- Add tests for protocol transformations, state-machine transitions, queue
  bounds, cancellation decisions, and regressions. Avoid tests that only repeat
  an implementation detail.
- Run `cargo fmt` after Rust edits.

## Validation

Run the checks appropriate to the change. Before committing a Tokio, transport,
or audio change, run all of these:

```text
cargo fmt --all -- --check
cargo test --all-features
cargo test --no-default-features
cargo clippy --all-targets --all-features -- -W clippy::pedantic
git diff --check
```

Pedantic Clippy currently reports an existing warning backlog. Its command must
complete successfully, and changed code should not introduce new warnings.
For launcher changes, also exercise the relevant `run.sh` or `run.bat` mode on
the available platform.
