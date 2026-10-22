{
  lib,
  stdenvNoCC,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  makeWrapper,
  fontconfig,
  pipewire,
  wayland,
  libxkbcommon,
  libX11,
  libXcursor,
  libXi,
  libXrandr,
  libXrender,
  libxcb,
  xdg-utils,
}:

stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "echobridge";
  version = "1.0.1";

  src = fetchurl {
    url =
      "https://github.com/darkyeg/echobridge/releases/download/v${finalAttrs.version}"
      + "/EchoBridge-${finalAttrs.version}-linux-x86_64.tar.gz";
    hash = "sha256-KhUQMQm+Ito03L1MkKBSfd496zwJH7IPEOC0a8DUOUk=";
  };

  nativeBuildInputs = [
    autoPatchelfHook
    makeWrapper
  ];

  buildInputs = [
    fontconfig
    pipewire
    (lib.getLib stdenv.cc.cc)
  ];

  # Winit loads the desktop backend libraries dynamically.
  runtimeDependencies = map lib.getLib [
    wayland
    libxkbcommon
    libX11
    libXcursor
    libXi
    libXrandr
    libXrender
    libxcb
  ];

  dontBuild = true;
  dontConfigure = true;

  installPhase = ''
    runHook preInstall
    install -Dm755 EchoBridge "$out/bin/EchoBridge"
    install -Dm644 echobridge.desktop "$out/share/applications/echobridge.desktop"
    install -Dm644 echobridge.png "$out/share/icons/hicolor/256x256/apps/echobridge.png"
    install -Dm644 LICENSE "$out/share/licenses/echobridge/LICENSE"
    if [ -f THIRD-PARTY-NOTICES.txt ]; then
      install -Dm644 THIRD-PARTY-NOTICES.txt "$out/share/licenses/echobridge/THIRD-PARTY-NOTICES.txt"
    fi
    substituteInPlace "$out/share/applications/echobridge.desktop" \
      --replace-fail "Exec=EchoBridge" "Exec=$out/bin/EchoBridge"
    runHook postInstall
  '';

  postFixup = ''
    wrapProgram "$out/bin/EchoBridge" --prefix PATH : ${lib.makeBinPath [ xdg-utils ]}
  '';

  meta = {
    description = "Removes headphone playback leaking into your microphone";
    homepage = "https://github.com/darkyeg/echobridge";
    license = lib.licenses.asl20;
    platforms = [ "x86_64-linux" ];
    mainProgram = "EchoBridge";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
})
