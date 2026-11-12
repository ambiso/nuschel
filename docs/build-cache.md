# Dependency caches and cross-build verifier isolation

Crane's pinned `buildPackage` creates an implicit `buildDepsOnly` derivation with
an artifact-only `installPhase`. It nevertheless passes the final `postInstall`
attribute through. Our source-bound notice bundle therefore changed that
otherwise-unused dependency input after an application-only edit. The observed
Windows cache contained a 954 MiB archive and took about nine minutes to produce.

`flake.nix` now explicitly creates the cross dependency pass, removing only final
installation attributes (`preInstall`, `postInstall`, `installPhase` and
`installPhaseCommand`) and replacing its source with `dummySrc`. The cross pass
retains Crane's artifact-only install phase. Final products retain their complete
installation and notice hooks. Compilation/check hooks, real vendored sources
retained by `dummySrc`, toolchains, features, targets and linker arguments remain
dependency inputs. This is cache hygiene, not a migration of every Crane builder.

Run the regression against a committed revision:

```console
python3 tests/build_cache.py --revision REV --output /disk/new-proof-directory
```

It exports only tracked source using `git archive`, evaluates the real Windows
and aarch64 macOS package derivations, edits an application source comment, then
changes the production Windows target flags in the private snapshot. It requires
unchanged dependency derivations and changed final products for the source edit,
and a changed Windows dependency derivation for the flag edit. It also checks
that final installation hooks survive and dependency installation has no notice
hook. This evaluates derivations; it does not compile cross products or establish
native runtime compatibility.

The initial actual evaluation passed at `49bc0fec`; local evidence is
`~/.cache/nuschl-build-cache/equality/run-1/proof.json`.

Official cross verifier feature separation and final-package/native Windows
validation remain a separate follow-up. The broader per-crate migration, native
macOS validation and complete current-head cross GUI validation remain open.
