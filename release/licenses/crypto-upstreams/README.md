# Crypto upstream notice supplements

These files supplement the selected runtime Cargo packages whose published
archives omitted an upstream license or notice file. `provenance.json` maps each
package version to its published VCS revision, exact upstream file URL, and
SHA-256. Original file bytes, including whitespace, are retained. The files were
also checked against the Git blob IDs in the complete, untruncated upstream
trees at those revisions.

This directory resolves 45 selected package versions: AKD, hax-lib, the libcrux
algorithm and utility crates, and OpenMLS. The libcrux root Apache license and
accompanying MIT notice are both retained. The unrelated p384 package's notices
are excluded because that package is not in the selected runtime inventory.
The repository names `cryspen/libcrux` and `celabshq/libcrux` expose the same pinned
Git objects; each package's original declared repository is preserved.

Six HPKE package versions remain explicitly **unresolved**. Their pinned
manifests declare MPL-2.0, but the complete pinned libcrux trees do not contain a
corresponding MPL license or notice text. The filtered imported HPKE history
also omits that text. Exact manifests are retained as evidence under
`hpke-license-evidence/`; the unrelated Apache and MIT root licenses are not
presented as substitutes. These entries must not be counted as complete merely
because a nearby notice file exists.

For the two locally patched packages, the published base revisions were
recovered from the original registry archives' `.cargo_vcs_info.json`:
`hpke-rs-libcrux` 0.7.0 and `openmls_libcrux_crypto` 0.4.0. Their existing local
`NUSCHEL-PATCH.md` files document dependency-only changes.

This is a source-backed supplement for the selected Cargo runtime inventory,
not an assessment of all distribution obligations, native libraries, fonts,
syntax definitions, or other bundled assets.
