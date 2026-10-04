# Contributing

## Set up

Install [Rust](https://rustup.rs/); `rust-toolchain.toml` selects the version. Then add:

- **Windows 10/11 x64:** Visual Studio 2022 C++ build tools.
- **Linux:** a C++ compiler, `pkg-config`, and the development packages `libpipewire-0.3-dev`, `libclang-dev`, `libfontconfig1-dev` and `libxkbcommon-dev`.

The first build downloads the WebRTC sources (pinned by SHA-256) and compiles the AI model in, which takes a few minutes.

## Everyday commands

```sh
cargo run --release -p echobridge                      # run the app
cargo test --workspace -- --test-threads=1             # tests, including the engine on a fake audio backend
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI runs the last three on Windows and Ubuntu.

## Tools

```sh
cargo run -p echobridge --example screenshots -- shots              # render every page to PNG
cargo run --release -p echobridge-audio --example devices           # list devices, test capture
cargo run --release -p echobridge-audio --example timing            # check capture timestamps
cargo run --release -p echobridge-audio --example virtual_mic       # Linux: test the virtual microphone
cargo run --release -p echobridge-engine --example latency -- 8 off # Windows: measure mic to call app delay
EchoBridge --list-devices                                           # JSON on stdout
EchoBridge --leak-test 10 --report leak.json
```

## Packages

- Windows: `pwsh scripts/build.ps1`, then `pwsh scripts/build-installer.ps1` (Inno Setup).
- Linux: `scripts/build-linux.sh` (tarball, `.deb`, checksums).

Stable releases come from tags and nightlies from `main`: see [Releasing](docs/RELEASING.md). To find where to change something, see [Architecture](docs/ARCHITECTURE.md).

Source, UI text and documentation are English only.
