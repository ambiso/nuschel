# Desktop integration generated from the application icon
# (apps/desktop/assets/nuschl.svg): the freedesktop entry and hicolor icons
# for Linux, Nuschl.icns for macOS and the Windows icon.
#
# apps/desktop/build.rs compiles the Windows icon into nuschl.exe, so it is
# committed as apps/desktop/assets/nuschl.ico. After changing the SVG:
#   nix build --cores 2 --max-jobs 1 --impure --expr \
#     '(import ./nix/desktop-files.nix { pkgs = (builtins.getFlake (toString ./.)).inputs.nixpkgs.legacyPackages.${builtins.currentSystem}; }).windowsIcon'
#   install -m644 result apps/desktop/assets/nuschl.ico
{ pkgs }:
let
  appId = "org.nuschl.Nuschl";
  svg = ../apps/desktop/assets/nuschl.svg;
  # The window's Wayland app id and X11 class are the same id, so shells
  # match the window to this entry and show its icon.
  desktopEntry = pkgs.writeText "${appId}.desktop" ''
    [Desktop Entry]
    Type=Application
    Name=Nuschl
    GenericName=Voice Chat
    Comment=Talk with people on Mumble servers
    Exec=nuschl %u
    Icon=${appId}
    Terminal=false
    Categories=Network;Chat;AudioVideo;
    MimeType=x-scheme-handler/mumble;x-scheme-handler/nuschl;
    Keywords=Mumble;voice;VoIP;
    StartupWMClass=${appId}
    X-GNOME-UsesNotifications=true
  '';
in
{
  # share/applications and share/icons/hicolor, for the package's share/.
  linux =
    pkgs.runCommand "nuschl-desktop-files"
      {
        nativeBuildInputs = [
          pkgs.resvg
          pkgs.desktop-file-utils
        ];
      }
      ''
        install -Dm644 ${desktopEntry} "$out/share/applications/${appId}.desktop"
        desktop-file-validate "$out/share/applications/${appId}.desktop"
        install -Dm644 ${svg} "$out/share/icons/hicolor/scalable/apps/${appId}.svg"
        for size in 16 24 32 48 64 128 256; do
          directory="$out/share/icons/hicolor/''${size}x''${size}/apps"
          mkdir -p "$directory"
          resvg --width "$size" --height "$size" ${svg} "$directory/${appId}.png"
        done
      '';

  # Apple's icon grid leaves a margin: the tile fills 824 of 1024 pixels.
  # Widening the view box by 152 units on each side does that at every size.
  macIcon =
    pkgs.runCommand "Nuschl.icns"
      {
        nativeBuildInputs = [
          pkgs.resvg
          pkgs.libicns
        ];
      }
      ''
        sed 's/viewBox="0 0 1254 1254" width="1254" height="1254"/viewBox="-152 -152 1558 1558"/' \
          ${svg} > padded.svg
        grep -q 'viewBox="-152 -152 1558 1558"' padded.svg
        for size in 16 32 48 128 256 512 1024; do
          resvg --width "$size" --height "$size" padded.svg "$size.png"
        done
        png2icns "$out" 16.png 32.png 48.png 128.png 256.png 512.png 1024.png
      '';

  # Uncompressed images up to 64 pixels and PNG at 256, as Windows expects.
  windowsIcon =
    pkgs.runCommand "nuschl.ico"
      {
        nativeBuildInputs = [
          pkgs.resvg
          pkgs.icoutils
        ];
      }
      ''
        for size in 16 24 32 48 64 256; do
          resvg --width "$size" --height "$size" ${svg} "$size.png"
        done
        icotool --create --output="$out" 16.png 24.png 32.png 48.png 64.png --raw=256.png
      '';
}
