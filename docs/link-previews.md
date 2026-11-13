# Sender-generated link previews

Under implementation, 2026-11-13. Native Linux has the sender-side Open Graph
pipeline, composer card and removal, global policy settings, and one-message Mumble
HTML projection. Parsing and image conversion run in confined content workers.
The default is Off. New preview fetching fails closed on other native platforms
until their process-lifetime adapters are implemented and tested.

The negotiated typed-message contract, encrypted structured body and browser sender
below remain unfinished. Compatible Nuschl servers currently receive the same single
legacy HTML message as stock servers; that is not `preview/1`. MLS and durable direct
conversations currently send text alone. Required private HTTP/proxy and desktop
interoperability fixtures are being validated; unit tests do not establish those
runtime guarantees.

## User behavior

Only the sender fetches a preview while composing a message. Receivers and chat
servers never retrieve a message's URLs or image URLs. A receiving card uses carried
metadata and embedded image bytes, decoded through the existing content worker.

The sender sees the actual URL/domain with the proposed title, description and image.
Metadata is an untrusted description of the page, not proof of website identity.
The card has a Remove action before Send. Removing it suppresses further attachment
for that URL until the URL changes or the sender explicitly requests a preview.
Late worker replies cannot undo removal. A failed or unfinished preview never blocks
the original text from being sent.

One message owns its text and preview: one idempotency key, sequence number, storage
transaction, edit and deletion. A preview is not a second image message. Modern
clients use typed, versioned metadata; the legacy projection is ordinary Mumble HTML
in one compatible message, within its server limits, or text alone.

## Global fetch policy

The initial default is **Off**. There is no default public proxy or DNS service.

| Mode | Behavior |
|---|---|
| Off | No automatic network request, DNS lookup or fetch worker. |
| Direct | Explicit opt-in to resolving and connecting to public target addresses directly. |
| ProxyRequired | No fetching until an explicit proxy and proxied DNS/bootstrap configuration are complete; no direct or local target-DNS fallback. |

The policy covers the HTML document, its redirects, the selected image, image
redirects and future metadata providers. A change invalidates late results and
requests cancellation, even when the message URL is unchanged. The native supervisor
keeps its one job slot until owned work has actually retired. A libc DNS lookup
already started in Direct mode can finish after cancellation; no new lookup or
connection is admitted for that revoked job. Displaying a carried preview does not
itself fetch anything, including when this sender-side policy is Off.

Initial standard-proxy support is SOCKS5 with numeric, validated target addresses.
Opaque SOCKS5h/CONNECT hostname resolution alone cannot prove that the proxy did not
resolve a public-looking name to a private address. Instead, target DNS is queried
through the configured proxy using an explicitly configured DNS-over-HTTPS endpoint
and bootstrap addresses; public answers are validated and pinned for the actual
tunnel. The original hostname remains the HTTP Host/TLS verification identity.
There is no implicit third-party resolver. A configured local proxy/resolver is an
explicit service exception, not permission for page-controlled private destinations.

Private, loopback, link-local, multicast, unspecified and other special-purpose
target ranges are denied. The same rule applies after every redirect and to every
image address. Numeric shorthand, mapped IPv4, alternate ports, credentials in URLs,
local names and redirect scheme changes must not bypass it. Tests use explicit
private fixture allowances unavailable to normal remote metadata.

No request carries browser cookies, authorization, referrer or ambient proxy
environment settings. Proxy failures never fall through to direct access.

## Provider contract

Open Graph v1 is the first provider: `og:title`, `og:description` and `og:image`,
with a bounded image and optional alternative text. Metadata strings are inert text.
The first property wins as specified by [Open Graph](https://ogp.me/); relative image
references are resolved against the fetched document URL and revalidated.
Canonical metadata URLs never replace the sender's actual card URL/domain.

Provider identity, schema version and capabilities are explicit. New providers must
use the same fetch policy, resource limits and sender-only lifecycle. Unknown
providers are not invoked. [RFC 6903](https://www.rfc-editor.org/rfc/rfc6903.html#section-3)
defines the `preview` link relation; following those links is future work and is not
silently implemented by the Open Graph provider.

Proposed initial limits:

| Resource | Limit |
|---|---|
| Candidate URL | 2 KiB, HTTP(S), no userinfo/control characters |
| Metadata document | 256 KiB, HTML only, no content decompression initially |
| Redirects | At most three across the preview fetch |
| Source image | 2 MiB encoded; existing worker pixel/dimension/CPU limits |
| Carried image | At most 32 KiB, canonical PNG/JPEG, at most 640×360 |
| Title / description | 512 / 2048 UTF-8 bytes, plain text |
| Message previews | One |

The carried image budget must also fit the actual message/encryption envelope.
Existing MLS bodies are bounded near 64 KiB: preview preparation must adapt the image
budget or omit the preview rather than increasing that limit or truncating text.

## Execution model and remaining contracts

1. **Provider and policy library:** `crates/link-preview`, including URL/address
   validation, explicit routes, request budgets and draft/result tickets. No UI or
   received-message code can initiate a fetch implicitly.
2. **Portable model:** `crates/core/src/preview.rs` holds the versioned preview value
   with provider, actual URL, title, description and embedded image.
3. **Atomic compatible-server contract (remaining):** negotiated `preview/1`; optional typed
   preview fields on send/message/edit; SQLite/Postgres and local-history migrations.
   Idempotency comparison includes the preview. Delete erases text and preview;
   editing without a preview removes the old one. Existing text remains literal text.
4. **MLS (remaining):** protect the entire structured body and archive record. A new authenticated
   application-body kind/version must not cause older clients to display raw JSON.
   Until group capability is authenticated, send text only. No cleartext preview
   field is added alongside ciphertext.
5. **Existing content worker:** bounded Open Graph extraction, source-image
   preparation and received-card rendering. Image decoding and HTML parsing remain
   outside the frontend. New IPC definitions avoid the inference supervisor's owned
   files.
6. **Native fetch supervisor/worker:** isolated HTTP response parsing with no home,
   credentials or new network connections in the worker. The supervisor validates
   DNS and establishes the one approved direct connection/proxy tunnel. A bounded
   duplex transport over inherited IPC pipes lets the worker exchange bytes only
   with that connection; redirects return to supervisor policy. This reuses worker
   framing/containment rather than adding a general plugin framework.
7. **SDK and native UI:** frozen composer/conversation/certificate/generation/policy tickets,
   debounced sender work, removal suppression, preview-aware atomic sends and
   acknowledgment handling; global settings and a sender card. Existing receiving
   clients render the carried legacy HTML through their content adapter, without
   fetching URLs. Browser sender work and typed receiver cards remain unfinished.

The network and HTML parsers' dependency safety review determines which operations
need the isolated worker. No new unsafe parser is silently introduced into the
frontend under the existing OS-binding exception.

## Required private tests

All HTTP, proxy, DNS, TLS and image fixtures are private. No tests fetch public sites.

- Off/incomplete ProxyRequired starts no DNS, socket or worker; changing policy and
  dismissing a card reject late replies. Failed previews preserve text sending.
- Recorded proxy/DNS fixtures prove every HTML/image/redirect/DNS request follows
  policy, numeric target pinning survives rebinding, forbidden addresses are refused
  and neither ambient proxy settings nor a failed proxy cause direct access.
- Bounds cover headers, bodies, redirect loops, malformed HTML, duplicate metadata,
  invalid URLs, oversized images, decompression and decoder rejection.
- Compatible and legacy servers carry one logical message. History, retries, edits
  and deletion preserve atomic behavior; local text search does not index image data.
- MLS records carry no cleartext metadata and preserve preview bytes through the
  protected application/archive path; unsupported group capabilities use text only.
- Receiver/server fixture request counts remain zero; only embedded pixels render.
- The sender card always names the actual URL/domain; Remove stays removed across
  late answers and Send, including navigation, reconnects and recipient changes.
