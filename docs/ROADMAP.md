# Roadmap

## 1.0: Rust app (current)

- Rust rewrite with the same processing as Python 0.3.3: Clean voice, Adaptive, Strong, Standard and AI noise removal, and the leak test.
- Lower delay (78–89 ms against 131–134 ms), about 22 MB of memory, and no runtime to install.
- Before release:
  - verify it in a real call;
  - build the installer;
  - consider downloading the AI model on demand to shrink the 39 MB executable.

## Next: verify daily call use

- Test with Discord, browser calls and other call apps.
- Handle unplugging, default-device changes, sleep/resume and long sessions without growing delay.
- Measure speech quality and leftover music while the user talks over playback.

## Later: own virtual microphone

- A signed Windows virtual audio driver, packaged separately, that exposes the clean microphone directly without VB-CABLE and with a smaller buffer.
- Reversible install and uninstall. The driver and the app are tested and versioned independently.

## Linux (in progress)

- Done: a PipeWire `AudioBackend`, an owned PipeWire virtual microphone, XDG autostart, single instance, tarball, `.deb` and `install.sh`.
- To measure on real hardware: the timestamp offset between microphone and loopback, and whether the sink monitor includes the hardware volume (the loopback `gain`).
- Later: AppImage and Flatpak, and a PulseAudio-only fallback for systems without PipeWire.

## Later: audio relay

- Paired devices on a LAN, Opus transport, bounded jitter buffers, and latency and packet-loss indicators.
- Received playback feeds the echo reference when it plays locally.
