# EchoBridge

EchoBridge removes headphone sound that leaks into your microphone before call apps hear it.

It listens to what your headphones play, subtracts that from the microphone, and offers the clean microphone to Discord, Zoom, Teams or any call app as a virtual microphone. It is one small program (Rust, about 22 MB of memory) for **Windows and Linux**, with no runtime to install, and everything is processed locally.

```text
 microphone ──────────────┐
                          ├─ EchoBridge ─ clean microphone ─ call app
 what the headphones play ┘
```

## Install

| | Windows | Linux (PipeWire) |
|---|---|---|
| Install | [`EchoBridge-Setup.exe`](https://github.com/darkyeg/echobridge/releases/latest), or the portable `EchoBridge.exe` | `.deb` from [Releases](https://github.com/darkyeg/echobridge/releases/latest), or the script below |
| One-line install | `irm https://github.com/darkyeg/echobridge/releases/latest/download/install.ps1 \| iex` | `curl -fsSL https://github.com/darkyeg/echobridge/releases/latest/download/install.sh \| sh` |
| Virtual microphone | install [VB-CABLE](https://vb-audio.com/Cable/) once (free, needs administrator approval) | nothing: EchoBridge creates it itself |
| In EchoBridge **Setup**, send the clean microphone to | **CABLE Input** | **EchoBridge Microphone** |
| In your call app, choose as the microphone | **CABLE Output** | **EchoBridge Microphone** |

Then keep your headphones as the call app's speaker, and press **Start**. The status turns green when your microphone is protected.

- **Windows** may warn that the app is unrecognized because it is not code-signed yet: choose **More info**, then **Run anyway**.
- **Linux** works on PipeWire, the default on current Ubuntu, Debian, Fedora and Arch; PulseAudio apps such as Discord and Chrome work through `pipewire-pulse`. On GNOME the tray icon needs the AppIndicator extension. See [`START-HERE-LINUX.txt`](START-HERE-LINUX.txt).
- **Nightly builds** of `main` are for testers, and may be unfinished. Add `--nightly` to `install.sh`, or `-Nightly` to `install.ps1`. See [Releasing](docs/RELEASING.md).

To share the program on Windows, see `START-HERE.txt`.

## Features

- **Echo removal**, in three modes:
  - **Clean voice** never alters your voice. It is a full-band linear canceller made for wired leaks, such as a headset on a combined audio jack.
  - **Adaptive** and **Strong** use WebRTC AEC3.
- **Noise removal**: Off, Standard (WebRTC, like Chrome) or AI (DeepFilterNet3; adds 30 ms).
- **Raw microphone capture.** The microphone is read without system voice effects, which would gate it before echo removal, and every block carries the audio system's own timestamp.
- **Live level meters** for the headphones, the microphone and the clean output.
- **Leak test.** It tells whether playback reaches the microphone through the air or through the wiring, and saves a numeric report (never audio).
- **Tray and startup.** Starts when you sign in, sits in the notification area, and saves settings automatically.
- **Private.** Nothing is recorded or uploaded.

## Develop

You need [Rust](https://rustup.rs/) (`rust-toolchain.toml` selects the version) and a few system packages:

- **Windows 10/11 x64:** Visual Studio 2022 C++ build tools.
- **Linux:** a C++ compiler, `pkg-config`, and the development packages for PipeWire (`libpipewire-0.3-dev`), clang (`libclang-dev`), fontconfig (`libfontconfig1-dev`) and xkbcommon (`libxkbcommon-dev`).

The first build downloads the WebRTC sources, pinned by SHA-256, and compiles the AI model in; that takes a few minutes.

```sh
cargo run --release -p echobridge                   # the app
cargo test --workspace -- --test-threads=1          # all tests, including the engine on a fake audio backend
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Useful tools:

```sh
cargo run -p echobridge --example screenshots -- shots   # render every page to PNG
cargo run --release -p echobridge-audio --example devices   # list devices and test capture
cargo run --release -p echobridge-audio --example timing    # check capture timestamps
cargo run --release -p echobridge-audio --example virtual_mic   # Linux: test the virtual microphone
cargo run --release -p echobridge-engine --example latency -- 8 off   # Windows: measure mic → call app delay
EchoBridge --list-devices                                # JSON on stdout
EchoBridge --leak-test 10 --report leak.json
```

Packages: `pwsh scripts/build.ps1` and `pwsh scripts/build-installer.ps1` on Windows, `scripts/build-linux.sh` on Linux (tarball, `.deb`, checksums). CI builds both on every push; stable releases come from tags and nightlies from `main` ([Releasing](docs/RELEASING.md)).

## Learn more

- [Architecture](docs/ARCHITECTURE.md): how the code is organized, where to change what, and how timing works.
- [Releasing](docs/RELEASING.md): stable and nightly channels.
- [Roadmap](docs/ROADMAP.md).

Source, UI text and documentation are English only.
