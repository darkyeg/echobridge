# EchoBridge

EchoBridge removes headphone sound that leaks into your microphone before call apps hear it. It listens to what your headphones play, subtracts that from the microphone, and sends the clean microphone to a virtual cable that Discord or any call app uses as its microphone.

It is one small program (Rust, about 22 MB of memory) for Windows and Linux, with no runtime to install.

## Features

- **Echo removal**:
  - **Clean voice** never alters your voice. It is a full-band linear canceller made for wired leaks, such as a headset on a combined audio jack.
  - **Adaptive** and **Strong** use WebRTC AEC3.
- **Noise removal**: Off, Standard (WebRTC, like Chrome) or AI (DeepFilterNet3; adds 30 ms).
- **RAW microphone capture.** The microphone is read like Chrome reads it, so Windows voice effects cannot gate it before echo removal. Every block carries the audio engine's own timestamp.
- **Live level meters** for the headphones, the microphone and the clean output, redrawn 30 times a second.
- **Leak test.** It tells whether playback reaches the microphone through the air or through the wiring, and saves a numeric report (never audio).
- **Tray and startup.** Starts with Windows, sits in the notification area, and saves settings automatically.
- **Private.** Everything is processed locally in memory. Nothing is recorded or uploaded.

## Use it

Download `EchoBridge.exe` from [Releases](https://github.com/darkyeg/echobridge/releases) and run it; nothing else to install. Windows may warn that the app is unrecognized because it is not code-signed yet: choose **More info**, then **Run anyway**.

1. Install [VB-CABLE](https://vb-audio.com/Cable/) (free; needs administrator approval once).
2. In EchoBridge **Setup**, choose your headset microphone, the headphones it hears, and **CABLE Input** as the destination.
3. In your call app, choose **CABLE Output** as the microphone.
4. Keep your headphones as the call app's speaker.

**Linux** (PipeWire, the default on current Ubuntu, Debian, Fedora and Arch): no virtual cable is needed, because EchoBridge creates the virtual microphone itself.

```sh
curl -fsSL https://github.com/darkyeg/echobridge/releases/latest/download/install.sh | sh   # any distribution
sudo apt install ./echobridge_*_amd64.deb                                                     # or on Debian and Ubuntu
```

Then choose **EchoBridge Microphone** in Setup and in your call app. See `START-HERE-LINUX.txt`.

Windows also has a PowerShell installer: `irm https://github.com/darkyeg/echobridge/releases/latest/download/install.ps1 | iex`.

To share the program, see `START-HERE.txt`.

## Develop

You need [Rust](https://rustup.rs/) (`rust-toolchain.toml` selects the version) and, on Windows 10/11 x64, Visual Studio 2022 C++ build tools. On Linux you need a C++ compiler, `pkg-config` and the development packages for PipeWire (`libpipewire-0.3-dev`), clang (`libclang-dev`), fontconfig and xkbcommon. The first build downloads the WebRTC sources, pinned by SHA-256, and compiles the AI model in. That takes a few minutes.

```powershell
cargo run --release -p echobridge                   # the app
cargo test --workspace                              # all tests, including the engine on a fake audio backend
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p echobridge --example screenshots -- shots  # render every page to PNG
cargo run --release -p echobridge-engine --example latency -- 8 off  # measure mic → call app delay
```

Diagnostics from the command line (JSON on stdout):

```powershell
EchoBridge.exe --list-devices
EchoBridge.exe --leak-test 10 --report leak.json
```

Package with PowerShell 7:

```powershell
pwsh scripts\build.ps1                          # dist\EchoBridge.exe, notices, START-HERE, LICENSE
pwsh scripts\build-installer.ps1 -Compiler <ISCC.exe>  # dist\EchoBridge-Setup.exe (Inno Setup)
```

On Linux, `scripts/build-linux.sh` builds `dist/linux/` (tarball, `.deb`, checksums).

CI on Windows and Ubuntu checks formatting, clippy, tests and the release build.

## Learn more

- [Architecture](docs/ARCHITECTURE.md): the crates, threads, timing and measured delay.
- [Roadmap](docs/ROADMAP.md).

Source, UI text and documentation are English only.
