# EchoBridge

**Stop your headset's echo from reaching your call.**

EchoBridge removes the sound your headphones leak into your microphone, so Discord, Zoom, Teams or any call app hears only your voice. Small, fast, private: everything is processed on your computer.

## Install

**Windows**

Download [`EchoBridge-Setup.exe`](https://github.com/darkyeg/echobridge/releases/latest), then install [VB-CABLE](https://vb-audio.com/Cable/) once (free).

**Linux**

```sh
curl -fsSL https://github.com/darkyeg/echobridge/releases/latest/download/install.sh | sh
```

Or install the `.deb` from [Releases](https://github.com/darkyeg/echobridge/releases/latest). Needs PipeWire, the default on current Ubuntu, Debian, Fedora and Arch. No virtual cable needed.

## Use it

1. Open EchoBridge, then **Setup**: choose your microphone and your headphones.
2. In your call app, choose the virtual microphone as your microphone:
   **CABLE Output** on Windows, **EchoBridge Microphone** on Linux.
3. Press **Start**.

## Learn more

- [Contributing](CONTRIBUTING.md): build, test and run it.
- [Architecture](docs/ARCHITECTURE.md): how it works.
- [Releasing](docs/RELEASING.md): stable and nightly builds.
- [Roadmap](docs/ROADMAP.md).

Windows warns that the app is unrecognized until it is code-signed: choose **More info**, then **Run anyway**.
