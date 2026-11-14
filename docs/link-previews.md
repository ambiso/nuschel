# Sender-generated link previews

Initial legacy delivery, 2026-11-14. Native Linux has the sender-side Open Graph
pipeline, composer card and removal, global policy settings, and one-message Mumble
HTML projection. Parsing and image conversion run in confined content workers.
The default is Off. New preview fetching fails closed on other native platforms
until their process-lifetime adapters are implemented and tested.

The negotiated typed-message contract and encrypted structured body below remain
unfinished. Compatible Nuschl servers currently receive the same single legacy HTML
message as stock servers; that is not `preview/1`. MLS and durable direct
conversations currently send text alone. Browser sender generation remains disabled
by design; receiving carried cards does not require it.

## Current delivery scope

The initial release targets legacy Mumble servers: a visible composer card and a
single ordinary HTML message carrying the original text, actual host, linked title,
description and embedded image. Native and web recipients display that card through
the existing inert legacy-content path; neither recipient fetches its resources.
Stock Mumble clients receive the same compatible message. The quote-style card
remains available on legacy servers without plugin-message support.

Negotiated typed previews, new archive/persistence handling and MLS preview support
are deferred. This does not disable ordinary existing local chat history. Issue
[#11](https://github.com/ambiso/nuschl/issues/11) remains open for the deferred work.
The sections below describe both this delivery and the remaining modern contracts;
they are not a claim that those contracts have shipped.

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

The planned negotiated format gives text and preview one idempotency key, sequence
number, storage transaction, edit and deletion. A preview is not a second image message. In that
format, clients use typed, versioned metadata; the legacy projection is ordinary Mumble HTML
in one compatible message, within its server limits, or text alone.

## Global fetch policy

Choose the global mode in **Settings → Link previews** on Linux. The initial
default is **Off**, and the setting applies to every server in the current profile.
There is no default public proxy or DNS service. The web adapter displays metadata
and embedded images carried in messages, with sender-side preview generation disabled.

| Mode | Behavior |
|---|---|
| Off | No automatic network request, DNS lookup or fetch worker. |
| Direct | Explicit opt-in to resolving and connecting to public target addresses directly. |
| ProxyRequired | No fetching until an explicit proxy and proxied DNS/bootstrap configuration are complete; no direct or local target-DNS fallback. |

The mode selector does not edit proxy or resolver addresses. To use **Proxy
required**, close Nuschl and create or edit `link-previews.json` beside
`settings.json`: normally `~/.config/nuschl/link-previews.json` on Linux (or
`$XDG_CONFIG_HOME/nuschl/link-previews.json` when set). With `--profile-dir DIR`,
use `DIR/config/link-previews.json`. For example:

```json
{
  "version": 1,
  "mode": "proxy_required",
  "proxy": "socks5://127.0.0.1:9050",
  "dns": {
    "url": "https://resolver.example/dns-query",
    "bootstrap": ["192.0.2.53"]
  }
}
```

Replace the proxy address, resolver URL and bootstrap IP with services you have
configured; the resolver values above are documentation placeholders. The proxy
must use `socks5://` with an IP literal and explicit port, without authentication.
The resolver must support DNS-over-HTTPS JSON GET requests using `name` and
`type` query parameters for A and AAAA records, not only binary DNS wire responses.
Supply one to four bootstrap IP addresses for the resolver: Nuschl reaches those
through the proxy while verifying TLS against the resolver URL's hostname and
built-in WebPKI roots. It resolves page and image hosts through that resolver
rather than system DNS.
Restart Nuschl after editing the file; **Active policy** and any configuration
problem appear below the mode selector. Missing or invalid proxy configuration
fetches nothing and never falls back to Direct.

An optional `managed-link-previews.json` uses the same format to restrict the
user's choice and, when requiring a proxy, supply its route. It can sit beside the
user file or in the system Nuschl configuration directory. It cannot enable
fetching when the user's mode is Off. The active policy may therefore be stricter
than the saved selection.

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
| Extraction | 4096 tags/comments/doctypes, 256 metadata tags, 4 KiB per attribute value |
| Redirects | At most three across the preview fetch |
| Source image | 2 MiB encoded; existing worker pixel/dimension/CPU limits |
| Carried image | At most 32 KiB, canonical PNG/JPEG, at most 640×360 |
| Title / description | 512 / 2048 UTF-8 bytes, plain text |
| Message previews | One |
| Retained drafts | 64, at most 1 MiB of draft text in total |
| Cached results | 16, at most 1 MiB; success expires after 10 minutes, failure after 2 |
| Policy file | 4 KiB; explicit user saves are atomic and owner-only |

The legacy projection may shorten the displayed title and description to 200 and
300 UTF-16 units. It preserves the entire original message text, obeys the server's
text/image limits and never exceeds the 256 KiB content limit. Attribute-name work
is bounded by the document cap and the worker's CPU/wall limits; it is not a claim
that html5ever's handling of many distinct attributes is linear.

The carried image budget must also fit the actual message/encryption envelope.
Existing MLS bodies are bounded near 64 KiB: preview preparation must adapt the image
budget or omit the preview rather than increasing that limit or truncating text.

## Execution model and remaining contracts

1. **Provider and policy library:** `crates/link-preview`, including URL/address
   validation, explicit routes, request budgets and draft/result tickets. No UI or
   received-message code can initiate a fetch implicitly.
2. **Portable model:** `crates/core/src/preview.rs` holds the versioned preview value
   with provider, actual URL, title, description and embedded image.
3. **Future compatible-server contract:** negotiated `preview/1` must keep typed
   text and card atomic through persistence, idempotency, live delivery, history,
   edits and deletion. Authoritative committed revisions must order updates.
   The current legacy path uses the established Mumble schema and existing local
   history. The internal core preview value is rendered into one compatible HTML
   message. `preview/1` remains unadvertised; its wire and storage integration is
   follow-up work in [#11](https://github.com/ambiso/nuschl/issues/11).
4. **MLS (remaining):** protect the entire structured body and archive record. A new authenticated
   application-body kind/version must not cause older clients to display raw JSON.
   Until group capability is authenticated, send text only. No cleartext preview
   field is added alongside ciphertext.
5. **Existing content worker:** bounded Open Graph extraction, source-image
   preparation and received-card rendering. Image decoding and HTML parsing remain
   outside the frontend.
6. **Native fetch supervisor/worker:** isolated HTTP response parsing with no home,
   credentials or new network connections in the worker. The supervisor validates
   DNS and establishes the one approved direct connection/proxy tunnel. A bounded
   duplex transport over inherited IPC pipes lets the worker exchange bytes only
   with that connection; redirects return to supervisor policy. Versioned preview
   requests use the existing worker framing and containment.
7. **SDK and native UI:** frozen composer/conversation/certificate/generation/policy tickets,
   debounced sender work, removal suppression, preview-aware atomic sends and
   acknowledgment handling; global settings and a sender card. Existing receiving
   clients render the carried legacy HTML through their content adapter, without
   fetching URLs. Browser sender work and typed receiver cards remain unfinished.

The network and HTML parsers' dependency safety review determines which operations
need the isolated worker. No new unsafe parser is silently introduced into the
frontend under the existing OS-binding exception.

## Validation and remaining tests

`checks.x86_64-linux.preview-worker` exercises the actual confined worker protocol,
including cancellation and process retirement. `checks.x86_64-linux.vm-link-previews`
runs synthetic HTTP/proxy/DNS peers and the packaged desktop in a software-rendered
Linux VM against stock Murmur and a compatible Nuschl server. It checks sender-only
fetching, Off/proxy failure behavior, one-message delivery to an independent native
recipient, removal and late results, and compact/zoomed composer and recipient layouts.

The `legacy-preview` scene in `tests/web_broker.browser.mjs` checks a real browser
recipient through stock Murmur and the web relay using a message exported by the
native fixture. It checks metadata inside the quote card, embedded image pixels,
one message, 1280px/390px layouts and forbidden browser requests. The coordinated
Linux VM and headless Chromium runs passed on `6f8b185e`; this does not establish
physical Windows/macOS/iOS/Safari behavior or kernel-wide browser egress containment.
Browser codec/content workers retain the browser isolation limitations described
in the architecture documentation. Final required CI remains a separate merge gate.

All HTTP, proxy, DNS, TLS and image fixtures use owned synthetic peers. No tests
fetch public sites. The complete feature's requirements include the following;
typed-history and MLS cases remain deferred with their implementation:

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
