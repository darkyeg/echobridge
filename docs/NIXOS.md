# NixOS

The flake packages the prebuilt **x86_64 Linux** release. Nix verifies the pinned download,
unpacks it, and adjusts its library paths. There is no Rust or C++ compilation. Runtime
libraries come from the usual Nix binary cache; no extra cache or `nix-ld` setup is needed.
ARM Linux binaries are not currently published, so this flake does not advertise ARM support.

## Run or install

With the `nix-command` and `flakes` experimental features enabled:

```sh
nix run github:darkyeg/echobridge
nix profile add github:darkyeg/echobridge
```

After installation, open EchoBridge from the applications menu or run `EchoBridge`.
Choose the physical microphone and headphones in Setup, and send the result to
**EchoBridge Microphone**. Choose **EchoBridge Microphone** in your call app too.

The package does not change microphone gain, boost, default devices, or autostart.
In-app microphone gain/boost controls are deferred; adjust the physical microphone in
Plasma's Sound settings or its hardware mixer. For sign-in startup, use the declarative
configuration below instead of the app's **Start when you sign in** toggle: that toggle
records the current executable's store path, which can become stale after an upgrade.

## Declarative installation

Add this input to your system or Home Manager flake:

```nix
inputs.echobridge.url = "github:darkyeg/echobridge";
```

Make the input available to your configuration, for example through `specialArgs`, then
include its package:

```nix
{ pkgs, echobridge, ... }:
{
  environment.systemPackages = [
    echobridge.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];

  services.pipewire = {
    enable = true;
    alsa.enable = true;
    pulse.enable = true;
  };
  security.rtkit.enable = true;
}
```

For Home Manager, put the same package in `home.packages` instead. Enable PipeWire in
the system configuration. Update your flake lock to receive packaging updates; the
packaged release version is explicitly pinned in `packaging/nix/package.nix` and can
differ from the source checkout's Cargo version.

## Optional autostart with Home Manager

Manage the desktop entry along with the package, so both update together and startup
uses the public wrapper. Disable the app's autostart toggle before enabling this configuration.

```nix
{ pkgs, echobridge, ... }:
let
  package = echobridge.packages.${pkgs.stdenv.hostPlatform.system}.default;
in
{
  home.packages = [ package ];
  xdg.configFile."autostart/echobridge.desktop".text = ''
    [Desktop Entry]
    Type=Application
    Name=EchoBridge
    Exec=${package}/bin/EchoBridge --background --autostart
    Icon=echobridge
    Terminal=false
  '';
}
```

## Microphone checks on Plasma

- Keep your headphones as the playback device. EchoBridge's virtual microphone is a
  recording device; it is not the speaker destination.
- If EchoBridge reports clipping, lower the **physical** microphone's recording level
  or its hardware Mic Boost. Gain settings depend on the microphone; a percentage that
  works for one device is not a universal default. Software cannot recover clipped audio.
- Echo removal targets playback leakage. Keyboard noise needs Noise removal; try **AI**
  if the processor can keep up. **Adaptive** starts with Strong protection and only
  relaxes it after sustained measurements; inconclusive measurements keep Strong active.
  Clean voice preserves the voice without suppression, but can leave a weak wired leak
  audible when microphone noise prevents its linear filter from learning it.
- After restarting EchoBridge, check the microphone selection in your call app again.
  A call app can fall back to the physical microphone while the virtual one is absent.
- If an echo appears briefly and disappears, note the time, the active modes, and whether
  you were speaking or changing devices. EchoBridge's diagnostic log is in
  `~/.local/share/EchoBridge/echobridge.log` (or under `$XDG_DATA_HOME`). It records
  counters and messages, not audio. Keep that evidence before changing settings.

## Verify the package

```sh
nix flake check
nix run . -- --list-devices
```

The flake check verifies that the binary starts without an audio server and that a
visible window appears under a virtual X11 display, without inherited library paths.
The device-list command checks the connection to your live PipeWire session. These
checks do not prove microphone quality or diagnose intermittent echoes in a real call.
