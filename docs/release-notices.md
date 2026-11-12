# Release notices and source evidence

The notice pipeline collects the normal dependency union of explicitly packaged
Rust roots and targets, including both fixed updater verifier features. Build,
development and procedural-macro dependencies are excluded. Actual bundled
npm code is measured separately from Vite's emitted main and worker chunk graph;
Svelte's runtime is included despite its package being a development dependency.
Generated WASM/glue hashes are associated with the client/audio/content Cargo
roots and the exact Cargo.lock used by the build.

`release/notice-roots.json` and `release/notice-scopes.json` are the source
authority. Package builders and the publisher choose the expected scope from a
fixed package/platform mapping; an artifact's own scope claim grants no authority.
Universal macOS requires both slices. The ARM Linux entry is metadata coverage
for an existing flake export, with no new ARM runtime or CI proof.

| Scope | Target coverage | External runtime/asset terms |
| --- | --- | --- |
| `linux-nix` | x86_64 Linux native/server/relay | Rust standard library, NewCM math, syntect data |
| `linux-aarch64-nix` | ARM Linux native/server/relay | Same families, independently selected normal target graph |
| `windows-x86_64` | Windows x86_64 desktop/helpers | Rust, GCC runtime exception, MinGW runtime/winpthreads, NewCM, syntect |
| `macos-universal` | x86_64 and ARM Darwin desktop/helpers | Rust, NewCM, syntect |
| `web` | Client/audio/content WASM build roots | Rust, syntect, actual emitted npm/helpers and retained Temml ancestry |
| `all` | Complete target union | Complete inspectable source audit, retained with CI diagnostic artifacts |

Native and cross notice derivations do not depend on the web App/WASM output
builds. The Linux CI job separately checks native-package and static-web notices.
Downloadable payload upload and release preparation reject missing inventories,
wrong roots/features/scope, absent notice bytes and source/hash mismatches.
Diagnostic logs remain available after failure. Signing secrets are only exposed
after prepared platform artifacts pass these gates.

## Compact payload and corresponding source

The original inventory remains inspectable as
`nix build .#release-license-inventory --cores 2 --max-jobs 2`. Each strict
platform bundle carries `THIRD-PARTY.txt`, `THIRD-PARTY.json`, Cargo.lock and a
README. Native scopes also contain the patched HPKE source archive. This keeps
notice delivery within the updater's existing 256-file installation bound.

Format 2 preserves exact original notice bytes, with a whole-payload digest and
per-source-file byte ranges/hashes in the index. Consumers verify bounds,
nonoverlap, aliases, every indexed body and every package/external reference.
Compaction neither clears nor reclassifies unresolved entries. The five-file
native bundles and four-file web bundle replace hundreds of per-crate files;
no notice is abbreviated to a license identifier or truncated.

Six exact `hpke-rs`, `hpke-rs-crypto` and `hpke-rs-libcrux` 0.7/0.8 pins have an
explicit curated declared-MPL2 decision. Their upstream package declarations,
authors, repository identities and actual source-header scans are retained,
along with canonical Mozilla-steward MPL2 bytes and exact source retrieval
instructions. This does not invent an upstream copyright notice or choose
Apache/MIT. Other packages/versions cannot obtain this exception automatically.
The complete patched `hpke-rs-libcrux` 0.7 Source Code Form is included as a
bounded tar; its member set and each file hash are checked against the reviewed
policy, beyond the outer archive hash.

SeaHash's actual original MIT notice was recovered from the precise source tree
copied from TFS. Its initial independent repository tree is identical, with an
immutable Software Heritage revision/ancestry record connecting it to published
4.1.0. Current author/license declarations are retained without claiming the
historical notice names every later author.

Native evidence uses actual pinned toolchain/source files. Rust's library
copyright document and accompanying complete term set are retained; GCC provides
GPL3 and the runtime exception. MinGW's own binary-runtime notice and the
explicitly linked winpthreads notice are selected, excluding unrelated utility
and profiling-library licenses. The unmodified math font is hash-matched to its
pinned Typst asset, with its embedded copyright, GUST grant and referenced LPPL
terms. Syntect's syntax data has a custom permissive grant with exceptions,
not a blanket MIT grant; the pinned submodule grants, relevant inline source
notices and all seven theme attribution sources are preserved.

## Proof and remaining release gates

At the Speex graph checkpoint, actual Nix builds and the production notice reader
accepted zero unresolved entries in Linux (841 packages), Windows (732), both-Mac
(739), web (130) and the complete six-target audit (931). This is metadata/notice
proof, not execution of those platform applications. The Linux notice derivation's
input graph was checked to exclude web App/SDK/WASM output builds.

Actual Vite fixture compilations cover worker inclusion, dead dependency
exclusion, missing real terms, lock drift and a reused build config dropping
removed worker evidence. The real app build identified Svelte 5.57.3, Temml
0.14.0 in the math worker, and Vite 8.3.8's emitted preload helper. Existing
Temml/KaTeX/React ancestry notices are retained. Emitted SDK/audio WASM bytes
match their Cargo-built artifacts. The content codec is built/exported but
unreferenced in current app chunks; the async SDK content broker and real
inbound/outgoing image UI remain queued, with original authenticated message
source preserved as authority.

After source/dependency updates, rerun the exact integrated package/archive gates
and inspect the final artifacts. Native Windows/macOS hybrid updater runtime
proof, hardware audio/accessibility tests, the first actual all-system CI run,
production public-pair/secret enrollment, and trusted metadata refresh before the
30-day maximum expiry remain separate gates. No production key, tag, push,
dispatch, deployment or publication was performed by this work. The source and
notice evidence is inspectable; it is not a legal-compliance assurance or proof
that future embedded assets can bypass review.

```sh
python3 tests/release_notices.py
python3 tests/update_release.py --signer /nix/store/…/bin/nuschl-update-sign
NUSCHL_VITE_PACKAGE=/path/to/pinned/node_modules/vite node --test tests/web_notice_inventory.mjs
python3 release/check-notices.py --scope windows-x86_64 /path/to/install/windows/licenses
```

Curated-source tests read the already cached exact published crates; set
`NUSCHL_LICENSE_CRATE_SOURCES` if they are in a different vendor/registry root.
Use a disk-backed TMPDIR for fixtures. None of these tests needs a host
microphone, camera, audio output, clipboard or public Mumble server.

Production trust update (2026-11-12): the dedicated public pair is now enrolled
in `crates/update/update-trust.json`. Statements above about absent enrollment
describe the recorded historical work, not the current source trust store.
This does not establish CI secret availability, a remote release or new platform proof.
