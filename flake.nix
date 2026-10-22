{
  description = "EchoBridge: prebuilt echo removal for PipeWire";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      package = pkgs.callPackage ./packaging/nix/package.nix { };
    in
    {
      packages.${system} = {
        default = package;
        echobridge = package;
      };

      checks.${system}.runtime =
        pkgs.runCommand "echobridge-runtime-check"
          {
            nativeBuildInputs = [
              pkgs.xvfb-run
              pkgs.xdotool
            ];
          }
          ''
            export HOME="$TMPDIR/home"
            export XDG_RUNTIME_DIR="$TMPDIR/runtime"
            mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
            chmod 700 "$XDG_RUNTIME_DIR"
            unset LD_LIBRARY_PATH WAYLAND_DISPLAY

            # A missing audio server must produce an app error, not a loader error.
            status=0
            ${package}/bin/EchoBridge --list-devices > devices.log 2>&1 || status=$?
            test "$status" -eq 1
            grep -F "could not reach PipeWire" devices.log

            # Wait for a visible window, rather than accepting a hung process.
            xvfb-run -a bash -eu -c '
              ${package}/bin/EchoBridge > window.log 2>&1 &
              app_pid=$!
              trap "kill $app_pid 2>/dev/null || true; wait $app_pid 2>/dev/null || true" EXIT
              if ! timeout 10s xdotool search --sync --onlyvisible --all \
                  --pid "$app_pid" --name "^EchoBridge$"; then
                cat window.log
                exit 1
              fi
            '
            touch "$out"
          '';
    };
}
