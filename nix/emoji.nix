# Emoji images and picker data for the desktop and web clients (nix/emoji.py).
# The sources are the `twemoji` and `emojibase-data` flake inputs. Callers
# outside the flake (crate2nix.nix, the distro packaging) get the same pinned
# trees from flake.lock, so every build path uses identical store paths.
{
  pkgs,
  sources ? null,
}:
let
  lock = builtins.fromJSON (builtins.readFile ../flake.lock);
  locked = name: builtins.fetchTree lock.nodes.${name}.locked;
  twemoji = if sources == null then locked "twemoji" else sources.twemoji;
  emojibase = if sources == null then locked "emojibase-data" else sources.emojibase-data;
  revision = lock.nodes.twemoji.locked.rev;
in
pkgs.runCommand "nuschl-emoji-assets" { nativeBuildInputs = [ pkgs.python3 ]; } ''
  python3 -I ${./emoji.py} --twemoji ${twemoji} --emojibase ${emojibase} \
    --version ${lock.nodes.twemoji.original.ref} --revision ${revision} --output "$out"
''
