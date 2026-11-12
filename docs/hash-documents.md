# Hash-only Mumble descriptions and comments

Stock Murmur sends a SHA-1 hash instead of a long channel description or user
comment. The SDK now remembers valid description-hash advertisements for the
current connection and requests the selected channel's missing body. Receiving
that body schedules the existing isolated HTML renderer. The first selection no
longer needs an unrelated editor action to fetch a description.

Opening a desktop profile establishes one SDK comment interest, bound to server,
connection generation, session number and the user's presence instance. A missing
comment is requested once per advertised hash; its arriving body is rendered
without closing the profile. A changed hash while that profile remains open
requests and renders the replacement. Repeated opens do not enqueue the same
pending body, and an exact prepared UI source reuses its existing document.

The desktop synchronizes its still-open profile before applying each event batch.
That operation can only retain or revoke interest; it cannot recreate a profile
interest after reconnect or session-number reuse. A hidden/replaced profile cannot
request or render subsequent comment changes. Reopening explicitly asks for the
latest body. Renderer answers still require the current generation, source and
presence; comments additionally require current interest. Existing cached document
leases, visible-surface checks and safe link dispatch remain in force.

Only explicit selection/profile actions or normalized model events schedule this
work. There is no HTML parsing, networking or polling in a UI render callback.
Description request metadata holds at most one 40-hex-character hash and request
flag per currently advertised channel. Removed channels and connection lifecycle
changes discard it. There is one comment interest, with one bounded hash. An
unchanged advertisement does not reissue a successful request. A failed immediate
platform enqueue can be retried by a later relevant action/event; an unanswered
successful request is not retried by an unbounded timer.

## Independent private proof

```sh
TMPDIR=/owned/disk/cache/tmp NUSCHL_WL_TMP=/owned/short/cache/path \
python3 tests/hash_documents.py \
  --binaries /path/to/package/bin \
  --murmur /path/to/mumble-server \
  --at-spi /path/to/at-spi2-core \
  --pulse-bin /path/to/pulseaudio/bin \
  --output /owned/disk/cache/hash-documents
```

The test imports the unchanged private welcome-document harness, configures its
fixture hooks with bodies longer than 128 bytes and launches disposable GNOME,
null PulseAudio, stock Murmur and private profiles. A synthetic URL handler only
records arguments; no external URL is retrieved and no host profile, microphone,
speaker or clipboard is used.

A separate bounded raw TLS probe pins that owned server's certificate and retains
actual control-frame bytes. It confirms that the initial long description/comment
contain the expected 20-byte hashes and no body; explicit `RequestBlob` replies
contain the exact body and no hash or request ID. SHA-1 is checked independently
against those controlled wire bodies. A second real native CLI observer confirms
the normalized SDK inputs as well. The plain-text comment command linkifies its
leading URL before sending; the fixture compares the explicit expected wire HTML,
not the original plain-text argument.

The old packaged executable reproduces both failures with `--expect-missing`:
selected description absent, comment appearing only after a second profile open.
The baseline result deliberately retains `document_links.passed:false`; the outer
welcome/control checks still pass. The fixed package demonstrates first-selection
description, first-open comment, replacement without reopening, old target removal,
and a hidden update followed by explicit reopening of the latest body. Four URL
activations reach only the owned handler. Source tests separately verify request
counts, malformed hash metadata, stale generations/sources, closed/replaced
interests, reused sessions, connection teardown and prepared/pending cache reuse.

AT-SPI object paths refer to current accessible nodes. AccessKit can reuse a path
when replacing a link's label and target; keeping a Python `Node` object does not
retain the previous Rust callback. The fixture checks current label/target
replacement and does not claim that such a reused path exercises an old callback.
An initial fixture that assumed otherwise is retained as failed harness evidence.
The existing document-link lease/activation tests remain separate evidence.

November 12, 2026 evidence is under
`~/.cache/nuschl-next/hash-documents/`: `baseline3/`, `fixed2/`, source/unit build
logs and `handoff.json`. Final SDK tests pass 99 with one existing ignored;
all 79 native desktop unit tests pass. The fixed distribution at production source `b22221b4`
is `/nix/store/dcdx6plh0fa45g4blqv9ka028zc6jglx-nuschl-0.1.0`; its scoped notice
reader passes. Later changes only extend tests/documentation. This is a Linux
private-GNOME proof, not native Windows/macOS, physical-device or new browser
profile-UI verification.

## Separate hash/body normalization audit

This work does not make server-controlled HTML cryptographically authoritative.
The existing native session retains an older description/comment hash when a new
small inline body arrives without a hash, and it does not independently digest
body-only blob responses. Incoming hash shape validation in that normalized model
also remains a separate audit; the new SDK request metadata copies only valid
40-character hexadecimal hashes.

The exact cached Murmur 1.5.939 source confirms synchronous current-body replies in
`src/murmur/Messages.cpp::msgRequestBlob`, with no hash/request ID in the response.
`Server::hashAssign` uses UTF-8 SHA-1 for strings at least 128 characters long;
smaller inline replacements can omit a hash. Those synchronous replies use ordered
TLS. An old response overtaking a newer hash has not been reproduced with honest
stock Murmur and is not claimed as an exploit. Unconditionally comparing every
inline replacement to a retained older hash would reject legitimate updates.
Any normalization follow-up must preserve those wire semantics and prove its
behavior independently. The source-level limitation remains recorded in TODO;
current stored-source, connection/presence checks and HTML sanitization still
apply to every delivered document.
