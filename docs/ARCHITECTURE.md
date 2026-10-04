# Architecture

```text
Physical microphone (RAW) ─┐
                           ├─ timestamp alignment ─ echo removal ─ noise removal (optional) ─ clean microphone
Headphone loopback ────────┘                                                                  │
                                                                         ├─ meters
                                                                         └─ virtual cable ─ call app
```

EchoBridge is one `EchoBridge.exe`, built from the Cargo workspace at the repository root (Rust 2024 edition, Slint UI). Each crate depends only on the crates above it in this list:

- `crates/dsp`: pure math, with no devices and no threads.
  - Block placement (`BlockClock`) and the reference timeline.
  - The output buffer (`ElasticBuffer`) and levels.
  - The Clean voice `LinearCanceller`.
  - Leak analysis.
- `crates/aec3`: WebRTC AEC3 and noise suppression, behind a C shim.
  - Compiled from the pywebrtc-audio sources, pinned and hash-checked.
  - No C++ exception crosses the boundary.
- `crates/denoise`: DeepFilterNet3 (model compiled in).
  - 30 ms delay, 24 dB attenuation limit.
  - Passes audio through, with the same delay, when the CPU cannot keep up.
- `crates/audio`: the `AudioBackend` trait (devices, capture, render).
  - Implementations: the WASAPI backend (Windows), the PipeWire backend (Linux) and a scripted `fake` backend for tests.
  - On Linux, EchoBridge creates its own virtual microphone (an `Audio/Source` node named "EchoBridge Microphone") while it renders to it, so no virtual cable is installed. The engine sees it as one more output device.
- `crates/engine`: the live engine and the leak recorder.
  - Threads, `Pipeline`, statistics and lock-free `Meters`.
  - It sees only `dyn AudioBackend`.
- `crates/app`: the program itself.
  - The Slint UI (`ui/*.slint`) and tray icon.
  - Settings, which also reads version 1 files from the old Python app.
  - The control thread (`service.rs`), status texts, the guided leak test and command-line diagnostics.
  - OS-specific code lives in `src/platform/`, with a fallback for other systems: start at sign-in, single instance, opening links.

## Threads and timing

- **Capture.**
  - The microphone opens in RAW mode when the device supports it, so Windows voice effects cannot gate it before echo removal. The stream category stays "Other", so Windows does not duck music.
  - Every block is stamped with the performance-counter time that WASAPI reports for its first frame, so the microphone and the loopback share one clock.
  - Capture checks stamps against the current performance counter and the device's buffer duration. Implausible stamps (including a measured loopback stamp 3.49 seconds in the future) receive a stable offset; healthy stamps retain their precision. `TIMESTAMP_ERROR` packets continue the sample clock when possible and reanchor after gaps. A changed mapping marks a discontinuity so the engine retrains. Recovery warnings include the correction and cumulative counts, at most once per 30 seconds per stream. See Microsoft's [GetBuffer](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer) and [timestamp flags](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/ne-audioclient-_audclnt_bufferflags) contracts.
  - Loopback blocks are scaled by the endpoint volume, which Windows applies after the loopback tap.
- **Processing.**
  - The processing thread runs at MMCSS "Pro Audio" priority.
  - It places each 10 ms microphone block on the device clock and waits up to 20 ms plus any positive alignment shift (from the block's arrival) for matching playback. The output reserves enough audio for that wait, with a bounded queue. Missing reference becomes silence and counts against reference coverage; Clean voice does not learn a new filter from incomplete reference.
  - Lost microphone blocks carry their skipped sample count to the next delivered block, preserving the clock's phase instead of smoothing away the gap. Device discontinuities discard partial packets; loopback discontinuities clear the reference timeline. Gaps that keep the clock mapping clear stream history while retaining the learned Clean voice filter. Changed microphone clock phase, a restarted reference timeline, and alignment changes retrain the obsolete path.
  - It runs the `Pipeline` and feeds the `ElasticBuffer`.
- **Alignment.** Windows timestamps can carry a bias between the loopback and the microphone that changes while the system runs. On the test headset the leak appeared 0.6 ms before its reference one morning and 19 ms before it that evening.
  - The canceller only models leaks that come *after* the reference, so an early leak removed about 1 dB, in Python 0.3.3 as well as in Rust.
  - `delay::Alignment` measures the lag continuously (GCC-PHAT at 12 kHz over 1.4 s, every 0.5 s, accepted after three agreeing measurements). When the leak falls outside 0.5–20 ms, it moves the reference read point so the leak sits 3 ms after it, and resets the processors.
  - Reading the reference later adds the same delay to the clean microphone (about 22 ms in the evening case). After the fix, the same song lost 17–25 dB with noise removal Off.
- **Output.**
  - The clean microphone renders in shared low-latency mode (`IAudioClient3`, the device's smallest period) when the device runs 32-bit float at 48 kHz. Otherwise it uses a normal 20 ms buffer.
  - The `ElasticBuffer` targets 20 ms. Its ±10 ms jitter band then leaves a 10 ms reserve at the lower edge, so a slower microphone clock is corrected before whole device packets run dry. A 10 ms target with the same band exhausted this reserve before correction began. Before a reference wait, it temporarily reserves the actual missing reference duration plus scheduling margin, capped one frame below the 150 ms queue limit. The reserve remains through processing; unused tail silence is reclaimed and replaced with the processed voice under one lock, keeping consecutive voice frames intact. The steady target stays unchanged. Underflow restores the steady margin, and a smaller target removes surplus delay immediately. Within ±10 ms of the target it passes audio through bit-exact. It resamples only for real clock drift, because interpolation dulls high frequencies, and it trims an excessive backlog.
- **Keep-alive.** The engine plays silence to the headphones in normal mode, because Windows loopback stops delivering while nothing plays.
- **Window.**
  - The window never waits on audio. `Service` runs engine start, stop and option changes on a control thread.
  - The window reads a status snapshot every 100 ms. It reads the meters from `Meters` atomics every 33 ms, and only while it is visible.
  - Meter lag is under about 80 ms: one 10 ms frame, up to 33 ms to the next read, one display frame, and a 30 ms slide.
  - The software renderer keeps memory low without a GPU context. With the window open, drawing costs about 60 ms of one core per second.

Pause (`set_processing(false)`) forwards the raw microphone through the same streams. Resume retrains the processors on the next frame and discards old lag measurements while keeping the known timestamp correction. Only Turn off, Quit or a device failure closes the streams.

## Diagnostics

Audio threads enqueue log messages without waiting for disk writes. The logging thread rotates the active file at 1 MiB and retains `echobridge.old.log`, `echobridge.old2.log`, and `echobridge.old3.log`. An oversized log from an older version is preserved when first rotated. Failed rotation preserves existing history without appending past the limit; queue or storage losses are counted and reported when logging recovers.

The first audio-health problem is logged promptly, followed by one aggregate summary every 30 seconds. Summaries retain counter deltas and totals, incomplete-reference percentage, retraining and stream-reset counts, clipped microphone-input frames, current modes and capture/AI flags, and sampled timing and buffer extremes. Clipping is aggregated instead of logging each onset. Temporary reserves are counted as information; reserves alone do not establish an audible gap. Pause, resume, option changes, failures, and shutdown preserve pending counters; shutdown joins the worker before taking the final summary. Discrete state changes remain immediate. Saturated logging queues count skipped messages instead of blocking audio.

Moving a microphone can change the acoustic leak path even when reference timestamps remain stable. Clean voice validates a new path shape over five seconds of suitable audio to protect overlapping speech. After accepting a candidate, foreground fit and energy are rebased to that filter before starting another trial; measurements from the old path must not impose another shape-validation period. This recovery is not instantaneous, and a full reference alone does not prove echo removal.

## Measured delay

These figures are the microphone-to-CABLE-Output delay, measured on 2026-10-02 with `cargo run --release -p echobridge-engine --example latency -- 8 off` (cross-correlated on the audio clock):

| Noise removal | Rust version | Python 0.3.3, same session |
|---|---|---|
| Off | 78–89 ms | 131–134 ms |
| AI | 146–154 ms | 179 ms |

Most of the remaining delay in those measurements was VB-CABLE's own buffer (`VBAudioCableWDM_Latency` = 7168 samples, up to 149 ms, set in its control panel) and it varied between runs. EchoBridge's own share was about 25 ms. The current 20 ms steady target adds 10 ms over the older 10 ms setting. Temporarily waiting for late reference adds the necessary delay; current physical-device latency has not been remeasured.

Processing costs 0.15–0.2 ms per 10 ms frame, or about 2 ms with AI. Memory is about 22 MB private.

## Tests and packaging

- `cargo test --workspace` runs every crate's tests, including the live engine on the fake backend.
- `cargo run -p echobridge --example screenshots -- <folder>` renders each page offscreen for design review.
- `scripts/build.ps1` builds the release into `dist/`, with license notices from `scripts/notices.ps1`.
- `scripts/build-installer.ps1` packs it with Inno Setup (`installer/EchoBridge.iss`).
- `scripts/build-linux.sh` builds the tarball and the `.deb` into `dist/linux/`. `scripts/install.sh` (Linux) and `scripts/install.ps1` (Windows) install a release for the current user, checking it against the release's `SHA256SUMS-*.txt`.

## Extension seams

- **Network transport.** It should receive clean frames after processing, or supply a render stream with its playout timestamps. Keep packet handling outside the processing thread. Jitter buffers must stay bounded and report discontinuities so the processors can reset after a reconnection.
- **Own virtual microphone on Windows.** An owned virtual driver replaces VB-CABLE as one more output device, as the PipeWire virtual source already does on Linux. Its lifecycle and timing must stay isolated from the UI and DSP.
