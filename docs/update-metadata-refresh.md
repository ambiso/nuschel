# Manual renewal of update metadata

The offline `release/refresh-update.py` helper renews the validity of the same
authenticated release without rebuilding or fetching binaries. It writes a new
review directory only. It never uploads assets, creates/enrolls keys, edits client
state, changes a clock, or schedules a signing job. Production public trust is
still deliberately unenrolled; these instructions do not authorize enrollment or
publication. Initial publication uses the normal release preparation, not renewal
of a legacy/minisign document.

`released` is the signed **metadata issuance time**, also used by clients for
anti-rollback. Renewing it does not create a new binary release. Version, notes,
platforms, file paths, artifact URLs, sizes and hashes retain their exact source
bytes, including whitespace, escapes and ordering. Only the two top-level integer
tokens `released` and `expires` change. Original binary provenance remains the
immutable release/tag/source/build records and the retained chain of old metadata
and renewal receipts; do not relabel a refreshed date as a binary build date.

The normal client accepts at most 30 days between issuance and expiry and at most
five minutes of future-clock skew. An expired document remains unusable by clients
even when its signatures are authentic. The operator-only `authenticate` command
checks both signatures on the exact raw bytes/full public pair and every v2
structural/interval limit, separately reporting `current`, `expired` or
`not-yet-valid` status. It is excluded from clients and is not an installation
acceptance command. Ordinary `verify`, runtime verification, download and apply
keep their current-time checks.

## Prepare the review inputs

1. Use the already reviewed signer from the pinned source/toolchain on an
   operator-controlled machine. Build/cache it before the offline session:
   `nix build .#update-signer --cores 2 --max-jobs 2`. Use its absolute binary path
   below. Keep the independent private seeds in the existing protected operator
   key file, outside the repository and source snapshots. On Unix use a private
   directory and mode 0600; Windows needs its own appropriate ACLs. Never pass seed
   values through arguments, environment variables, logs or chat.
2. Obtain the exact current `update.json` and `update.json.sig` pair as local
   files, plus the independently pinned, already enrolled public trust document.
   Do not take trust roots from the same download. Keep the original raw bytes.
3. Consult the protected operator release ledger: identify the **latest issued**
   metadata hash, version and issuance time and its immutable binary provenance.
   Compare the original release/tag/source/build records and retained receipts.
   The required hash pin must come from that trusted record, not from computing a
   hash of an arbitrary download. A signature proves authority, not freshness;
   this offline tool cannot discover a withheld newer release or a stale ledger.
4. Confirm the clock against a trusted time source and review the intended
   remaining validity. Plan renewal comfortably before expiry, for example a
   manual weekly check for a 30-day document with a seven-day safety margin.
   This is an operator runbook, not an implemented reminder/cron service.
5. Check the hosted immutable artifacts and their existing release/source records
   separately. This helper does not fetch or re-hash binaries or independently
   attest the human-provided provenance reference. If any binary, URL, hash, size,
   version or release notes must change, prepare a normal reviewed release instead.

The public pair must match both the authenticated input and the existing secret.
Even another enrolled pair is refused: renewal does not silently perform key
rotation. Use the separately reviewed overlap-release process for rotation.

## Produce the offline candidate

Use a new directory under private, disk-backed operator scratch. No output is
exposed at that final path until authentication, signing and ordinary current-time
verification all succeed. Existing outputs are never overwritten.

```sh
python3 release/refresh-update.py \
  --manifest /operator/review/latest/update.json \
  --signature /operator/review/latest/update.json.sig \
  --expected-manifest-sha256 LATEST_HASH_FROM_PROTECTED_OPERATOR_LEDGER \
  --binary-provenance 'original release/tag, immutable source SHA and reviewed build-record hashes' \
  --public-key /operator/trusted/update-v2.public.json \
  --sign-key /operator/private/update-v2.secret.json \
  --signer /nix/store/REVIEWED_SIGNER/bin/nuschl-update-sign \
  --valid-days 30 --output /operator/scratch/renewal-candidate
```

The helper copies bounded regular inputs into private staging, authenticates those
copied raw bytes, requires the ledger hash and the same pair, and refuses a clock
at/before the previous issuance or a shortened validity window. It signs the
byte-preserving replacement and runs **ordinary `verify`** on that new document.
The new interval remains 1–30 days; no clock override is available.

Review all candidate files:

- `update.json` and `update.json.sig` are the only candidate publication assets.
- `previous-update.json` and its signature retain the authenticated original bytes.
- `public-trust.json` records the public authority used by the operator; it grants
  no authority to a running client and must not be mistaken for adjacent trust.
- `refresh-receipt.json` records old/new raw/signature hashes, original issuance,
  new metadata times, the same public pair ID and the hash of all raw bytes apart
  from the two time tokens. It explicitly labels binary provenance as an operator
  reference, not independently verified evidence, and `published` remains false.

Independently review that the version and every artifact binding are unchanged,
the dates are sensible, the receipt's old hash matches the protected latest
ledger, and normal `verify` succeeds with the trusted public document. Archive the
complete review set and original binary provenance. Do not reset a client's
remembered version/issuance to make an older release work.

## Already-expired metadata and clock recovery

First distinguish a genuine expiry from an incorrect clock. Ordinary `verify`
reports expired or future-clock rejection, while `authenticate` can still tell
whether the raw old document is genuinely signed. Never turn back a clock, remove
rollback state, disable verification, or treat authentication-only output as
client acceptance.

```sh
/nix/store/REVIEWED_SIGNER/bin/nuschl-update-sign authenticate \
  --public-file /operator/trusted/update-v2.public.json \
  --manifest /operator/review/latest/update.json \
  --signature-file /operator/review/latest/update.json.sig
```

If the **operator's** clock is wrong, correct it using a trusted source and start
over. A genuine future-issued source is refused by renewal even with recovery
selected. If the latest genuine metadata has expired, review the trusted latest
ledger and unchanged original artifacts, then repeat the same helper invocation
with `--recover-expired`. That explicit operator recovery permits authentication
of the expired input; it does not extend the old signature or waive client expiry.
The output is a fresh, normally valid, signed document. Corrupt, duplicated,
malformed, wrong-pair or overlong-validity input remains rejected.

For a **client** reporting expiry, check local date/time first and then retrieve
the currently published signed metadata from the official source. If the operator
has not renewed it, wait for a valid signed refresh; do not install expired data.
For a future-clock error, correct the clock and retry. For rollback rejection,
retrieve the actual latest release/metadata and investigate the ledger/source;
renewing an older version does not defeat a newer version already remembered.

After normal verification, same-version metadata advances the remembered issuance
but does not reinstall an already-current binary. Its older metadata is then
rejected as stale. Expiry is still checked again before download and apply.

## Publication remains a separate action

No publication command or credential operation is performed here. After a
separate explicit publication review, replace **only the metadata/signature pair**
on the intended existing release; do not retag, recreate binaries or alter their
immutable URLs. Use an atomic pair switch where the hosting service supports it.
If it cannot replace two assets atomically, document the brief maintenance window:
a mismatched old/new pair must fail closed until both assets are present, and
clients can retry. Never weaken verification to hide that window. Verify the
downloaded pair afterward, record its exact hash as the new latest trusted ledger
checkpoint, and record the next manual deadline. No such production action was
performed by this work.

Actual fixture evidence: 9 offline raw-span/signature/recovery tests, 13 updater
unit tests including same-version persisted anti-rollback and clock/expiry checks,
and the existing 15 release-gate regressions pass with temporary fixture keys.
The native Linux/Windows runtime fixtures separately reject expired manifests;
the new operator command changes neither runtime. No public service is contacted
by these tests.
