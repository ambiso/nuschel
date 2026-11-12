# Regenerating the per-crate graph

Run `just crate2nix` after changing Cargo.lock or a package manifest, including a
new vendored package. The generation command uses the local working tree. Its
fingerprint helper also reads local files, so a new manifest participates before
Git or jj has recorded it. The command does not copy the working directory into
a `path:` flake or include ignored archives and build caches.

The fingerprint matches `flake.nix`'s `crate2nixInputs`: concatenate Cargo.lock,
the root Cargo.toml, then the manifests of sorted direct directories under apps,
crates and vendor, and hash those exact bytes. Directory symlinks and deeper
manifests are excluded; manifest-file symlinks in real directories follow Nix's
read behavior. Keep the helper and Nix expression aligned when changing this
scope. `python3 tests/crate2nix_inputs.py` checks the untracked-vendor regression,
ordering/symlink behavior, and equality with Nix on the current source.

This fingerprint detects stale inputs; it does not prove the generated graph's
contents are correct. Review generated changes and run the normal Nix
`crate2nix-current` check before accepting a dependency update.
