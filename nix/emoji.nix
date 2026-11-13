# Emoji images and picker data for the desktop and web clients (nix/emoji.py).
#
# The three sources are pinned here, together, because they belong together:
# Twemoji draws the emoji of one Unicode Emoji version, emojibase-data names
# and groups that version's emoji from one CLDR release, and that CLDR
# release's licence covers the names and keywords. Update all three at once
# (docs/emoji.md, "Updating"). Every caller (the flake, crate2nix.nix, the
# distro packaging) imports this file, so all build paths use identical
# store paths.
#
# The desktop embeds the images gzip-compressed (`svgz/`, by zopfli: about
# 4.2 MB instead of 10 MB); GPUI's SVG parser (usvg) reads gzip as is. The
# web client serves the plain `svg/`, which HTTP compression covers.
{ pkgs }:
let
  pins = {
    twemoji = {
      version = "17.0.3";
      rev = "b6b55fef1e8636b540a6d016a4729ca8cdf2e60b"; # tag v17.0.3
      hash = "sha256-qgdguv5/EhjLlKxIvvQF7+s+AkY7KnFhaKRziOR1hhY=";
    };
    emojibase = {
      version = "17.0.0"; # Emoji 17, CLDR 48 (its CHANGELOG)
      url = "https://registry.npmjs.org/emojibase-data/-/emojibase-data-17.0.0.tgz";
      hash = "sha256-zXqdNh61bse6wES1olFEyMyaYYk6OlgaFZ9/Kzl3MD4=";
    };
    # emojibase-data's names and keywords derive from CLDR's annotations
    # (Unicode License v3); the npm package carries no Unicode notice, so the
    # licence comes from the CLDR release it was built from.
    cldr = {
      version = "48";
      rev = "acd6d88ae493633240e19a87a721076a8a75c310"; # tag release-48
      hash = "sha256-tMCujvBPcFn5bOW74EZ/n+b22Bu+E1F3Ad/rlh+00LY=";
    };
  };
  twemoji = pkgs.fetchFromGitHub {
    owner = "jdecked";
    repo = "twemoji";
    inherit (pins.twemoji) rev hash;
  };
  emojibase = pkgs.fetchzip { inherit (pins.emojibase) url hash; };
  cldrLicense = pkgs.fetchurl {
    url = "https://raw.githubusercontent.com/unicode-org/cldr/${pins.cldr.rev}/LICENSE";
    inherit (pins.cldr) hash;
  };
in
pkgs.runCommand "nuschl-emoji-assets"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.zopfli
    ];
    # For the release notices (nix/release-external-notices.nix).
    passthru = {
      inherit pins;
      sources = {
        twemoji = "https://github.com/jdecked/twemoji/tree/${pins.twemoji.rev} (${pins.twemoji.hash})";
        emojibase = "${pins.emojibase.url} (${pins.emojibase.hash})";
        cldr = "${cldrLicense.url} (${pins.cldr.hash})";
      };
    };
  }
  ''
    python3 -I ${./emoji.py} --twemoji ${twemoji} --emojibase ${emojibase} \
      --cldr-license ${cldrLicense} \
      --version ${pins.twemoji.version} --revision ${pins.twemoji.rev} \
      --emojibase-version ${pins.emojibase.version} --cldr-version ${pins.cldr.version} \
      --output "$out"

    # zopfli's gzip has no file name and a zero time: reproducible. Each file
    # must decompress to exactly the checked SVG.
    mkdir "$out/svgz"
    compress() {
      zopfli -c "$1" > "$out/svgz/$(basename "$1" .svg).svgz"
    }
    export -f compress
    export out
    find "$out/svg" -name '*.svg' -print0 \
      | xargs -0 -n 64 -P "''${NIX_BUILD_CORES:-1}" bash -c 'for f; do compress "$f"; done' _
    for svg in "$out"/svg/*.svg; do
      gzip -dc "$out/svgz/$(basename "$svg" .svg).svgz" | cmp -s - "$svg" \
        || { echo "svgz of $svg does not match it" >&2; exit 1; }
    done
    test "$(ls "$out/svgz" | wc -l)" = "$(ls "$out/svg" | wc -l)"
  ''
