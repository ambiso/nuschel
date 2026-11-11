# The web host, share links and the web client

Decisions with the user (2026-11-11, docs/clients.md) and what was built from them:

- **One domain the user hosts** serves the web client, so the browser keeps one
  cross-server identity; others may host the same files. Signal-like version
  requirements apply as for the apps.
- **Svelte** for the web UI, over the SDK in WebAssembly.
- **NixOS first**: a module with `services.nginx.virtualHosts` and `enableACME`,
  optionally on a subdomain.
- **Legacy Mumble servers** work from the web where possible: the browser reaches
  every server through an untrusted tunnel and runs TLS itself.
- **Share links** open the installed app (Android App Links, iOS Universal Links,
  the desktop's `nuschl://`), and otherwise join in the browser. The project
  domain is optional: any web host can serve them.

## The tunnel relay (`apps/web-relay`)

Browsers cannot open TCP or UDP sockets. The web client opens a WebSocket to the
relay (`wss://<web host>/relay`, subprotocol `nuschl-tunnel.1`); the relay connects
to the Mumble or Nuschl server's TCP port and, if the server answers a Mumble ping,
a UDP socket for voice datagrams. TLS runs in the browser (rustls in WebAssembly),
so the relay forwards ciphertext it cannot read and the client checks the server's
certificate itself, exactly as the apps do. Voice datagrams are OCB2-encrypted with
keys from that TLS session.

**Framing** (`nuschl_protocol::tunnel`, fuzz target `tunnel`): every WebSocket
message is a type byte and a body: `Open` (port, host; first and once), `Stream`
(TCP bytes, at most 64 KiB), `Datagram` (one UDP datagram, at most 1024 bytes),
`Opened` (whether UDP works) and `Closed` (a reason with a fixed text). Decoding is
strict; anything else closes the tunnel.

**Not an open proxy.** A tunnel is refused unless all of these hold:

- The port is one the relay forwards to: 64738–64799 unless configured (`ports`).
- Every address is public. The relay resolves the name once, drops private,
  loopback, link-local, CGNAT, multicast, documentation, benchmarking, 6to4,
  Teredo, NAT64 and other special ranges (IPv4-mapped IPv6 is checked as IPv4),
  and connects only to an address it checked, so a name cannot change its answer
  in between (no DNS rebinding). `allow_networks` admits a LAN or test network;
  `deny_networks` adds ranges. On NixOS the kernel refuses the same ranges to the
  relay's unit (`IPAddressDeny`).
- The destination is a Mumble server: it answers the unencrypted 12-byte Mumble
  ping (then datagrams go over UDP), or, failing that, a TLS handshake. Either way
  the client's first bytes must be a TLS ClientHello record and the server's first
  bytes a ServerHello or an alert; until then nothing from the server is forwarded,
  so a relay cannot be used to talk to SSH, SMTP or HTTP services, only to send them
  a ClientHello. `require_ping` makes the ping mandatory.
- With `allowed_origins` (the module sets the web host), only the web client's
  pages may open tunnels; other sites cannot use visitors' browsers.

**Limits** follow the user's view that limits fail only what reaches them: per
client address (an IPv4 address or an IPv6 /64, taken from nginx's `X-Real-IP`,
trusted only from the Unix socket or `trusted_proxies`) at most 16 open tunnels and
30 new ones a minute; at most 4096 tunnels on the relay, beyond which new ones are
refused as busy; per tunnel and direction 512 KiB/s with 4 MiB of burst, enforced by
slowing that tunnel, never by dropping data or touching other tunnels; 120 s without
traffic closes a tunnel (Mumble pings every 5–15 s). A refused tunnel is answered
with `Closed` and a reason the client shows.

**Logs** name refusals (reason and destination), never payloads, user names or
client addresses; tunnel lifetimes and byte counts are debug-level.

**Settings** (`nuschl-web-relay.toml`, apps/web-relay/src/config.rs): `listen`
(`127.0.0.1:8090` or `unix:/run/…/relay.sock`), `ports`, `allow_networks`,
`deny_networks`, `allowed_origins`, `trusted_proxies`, `require_ping`, `max_tunnels`,
`client_tunnels`, `client_opens_per_minute`, `bytes_per_second`, `burst_bytes`,
`idle_timeout_seconds`.

`nuschl-web-relay-probe` logs in through a relay with TLS on its side (the loopback
test and the VM check use it):
`nuschl-web-relay-probe --relay wss://web.example.org/relay --origin https://web.example.org --server voice.example.org`.

Later: WebTransport (datagrams without the TCP tunnel), and Nuschl servers offering
the tunnel endpoint themselves.

## The NixOS module (`services.nuschl-web`)

```nix
imports = [ inputs.nuschl.nixosModules.web ];
security.acme = { acceptTerms = true; defaults.email = "admin@example.org"; };
services.nuschl-web = {
  enable = true;
  hostName = "web.example.org";   # a domain or subdomain; keep it for good
  # appLinks.android = [ { package = "org.nuschl.android"; certificateFingerprints = [ "…" ]; } ];
  # appLinks.apple = [ "TEAMID.org.nuschl.Nuschl" ];
  # relay.ports = [ "64738-64799" "10000-20000" ];
};
```

One nginx virtual host with `enableACME` and `forceSSL` (both on by default) serves:

- `/`: the web client's static files (`package`, by default `packages.web-site`:
  today the landing page and a placeholder; the Svelte build joins it later).
- `/join/…`: the share-link landing page, with access logging off for the path.
- `/.well-known/assetlinks.json` and `/.well-known/apple-app-site-association`, from
  `appLinks` (empty lists by default), served as JSON.
- `/relay`: the relay, through a WebSocket upgrade over a Unix socket in
  `/run/nuschl-web-relay` that only nginx's group reaches.

Every response carries a strict Content-Security-Policy (only this origin; the relay
as the one connection; `wasm-unsafe-eval` for WebAssembly), `Referrer-Policy:
no-referrer`, `nosniff` and a Permissions-Policy allowing only the microphone. The
relay runs as its own system user (static, so nginx can share the socket's group)
under the server module's systemd hardening. `installUrl` sets where "Get Nuschl"
leads.

`nix build .#checks.x86_64-linux.vm-web` boots the module (plain HTTP, no ACME in a
VM), checks the site, the landing page, its headers, the app-link files and that
tokens are not logged, logs in to a stock Murmur through the relay with the probe
(TLS on the client, a ping datagram answered over UDP), and checks that other origins,
other ports, loopback and private addresses and a service that is not Mumble are
refused.

## Share links

```text
https://<web host>/join/<token>#s=<host>[:<port>]&c=<channel path>&fp=sha256:<hex>&policy=nuschl&n=<name>
nuschl://join/<token>#<the same fragment>
```

`nuschl_core::share::ShareLink` reads and writes them; web/site/join.js reads the
same format.

- **The token** is opaque: 16 random bytes in base64url, issued by a Nuschl server
  (`share/1`), which resolves it to a channel after the client connects. Links to
  stock Murmur, and from users the server issues no tokens to, have no token
  (`/join/#…`) and name the channel by its path in the fragment. A token link
  carries the path too, for the landing page and as the fallback where the token
  cannot be resolved.
- **The fragment** never reaches the web host: the server's address, its certificate
  pin and policy (as in `mumble://` invites, docs/extension.md §1, rule 3), the
  channel path and an optional display name. Values are percent-encoded (`+` is a
  space, as browsers read it). A link is at most 8 KiB, and only what writes itself
  again within that parses (found by fuzzing `mumble_url`).
- The token is in the path, so the web host sees it (over TLS) but cannot use it
  without the fragment's server; the module keeps it out of nginx's logs. A token
  with an access token is a bearer secret for that channel: revoke it when it leaks.

**The landing page** (`web/site/join.html`, no tracking, nothing loaded from
elsewhere, readable by screen readers) shows the channel and server, and offers
**Open in Nuschl** (`nuschl://join/…`), **Join in the browser** (`/#t=<token>&…`,
the web client's entry) and **Get Nuschl**. After "Open in Nuschl" was used once,
it opens the app at once next time (one local preference, never sent anywhere).
Android and iOS open `/join/` links themselves once `appLinks` lists the apps.

**The desktop** registers `nuschl://` next to `mumble://` (the Linux desktop entry)
and opens share links from the command line, from links clicked in messages and from
the landing page. A link to a server not saved yet opens the server form; the link's
pin, policy, channel and token apply to the server saved from it. Once connected, the
token is resolved (`share/1`); a private channel's access token is kept with the
server like one typed in, then the channel is joined; without `share/1` the path
decides. **Copy share link** in a channel's menu asks a Nuschl server with `share/1`
for a token (registered users), and elsewhere copies a link with the channel's path,
the pin and the policy. The web host in copied links is the server operator's
(`share_links.web_host` in nuschld.toml), else the user's `share_web_host` setting,
else the project's, nuschl.eu (`nuschl_core::share::PROJECT_WEB_HOST`; until the
module runs there, its links open only in Nuschl, which reads them itself); with
none, the app's own `nuschl://` link would be copied. macOS and Windows do not
register the scheme yet.

**nuschld** (`share/1`, apps/server/src/share.rs; messages 0x4E90–0x4E93, 0x4E94–
0x4E97 reserved, docs/extension.md):

- `ShareLinkCreate` → `ShareLink`: registered users who may enter the channel; a link
  with an access token (Mumble's `#token` ACL groups, at most 128 bytes) needs Write
  on the channel or the root; an optional expiry (Unix seconds). The reply carries
  the operator's web host.
- `ShareLinkResolve` → `ShareLink` (channel, expiry, access token): any authenticated
  connection, rate-limited with the connection's request budget. Unknown, revoked and
  expired tokens and removed channels are the same `NOT_FOUND`.
- `ShareLinkRevoke` → `Done`: the creator, or Write on the link's channel.
- At most 64 links per channel and 256 per account (`LIMIT_REACHED`, failing only that
  request). Links are kept in the server's key-value table under `S` ‖ token and
  survive restarts; expired ones are dropped when met. Listing a channel's links and
  revoking them from the desktop are left open.

## The web client (`crates/client-wasm`, `web/app`)

The SDK runs in WebAssembly behind `WebClient` (wasm-bindgen): `add_server`,
`connect`, `disconnect`, `accept_certificate`, `join`, `tick` (keep-alive pings, every
few seconds), `poll` (applies what arrived and returns the updates as JSON) and
`servers` (status, notice, certificate prompt, channels and users as JSON);
`on_change` names a callback for when something arrived. Its `Platform` keeps one
WebSocket per server; each carries a `Session` (plain Rust, no I/O, tested natively):
the tunnel's frames, rustls with ring built for wasm32, the certificate checked
against the pin or the web's certificate authorities (anything else is the user's
decision, as in the apps), the Mumble login, the channel tree and users. It uses no
DOM, so a Web Worker can host it.

- `just wasm-client` builds it (ring's C needs an unwrapped clang for wasm32; the
  wasm-bindgen CLI is nixpkgs' `wasm-bindgen-cli_0_2_130`, the crate's exact pinned
  version) into `web/app/src/sdk`; `just web` builds the Svelte page into
  `web/app/dist`. The module is about 1.4 MB (520 KB compressed).
- `web/app` is a minimal Svelte 5 page: server, port, name and optional pin, the
  relay's address (the same origin by default), "Connected" or "Not connected" as a
  live status, a certificate prompt and the channels with their users. A share link's
  "Join in the browser" fills in the server; with a name (`u=`) it connects at once.
  Certificate pins are preserved. Invites requiring `policy=nuschl` are refused
  with an app-opening explanation until the browser implements that negotiation;
  they must never silently fall back to legacy Mumble. Malformed or ambiguous
  security options also fail closed. `npm test` covers these fragment rules.
- Tests: `cargo test -p nuschl-client-wasm` runs the Session through the real relay
  to a TLS server (and to Murmur with `NUSCHL_TEST_MURMUR`); `tests/web_client.py
  <binaries> --murmur <mumble-server>` runs the WebAssembly build in Node.js (the
  browser's WebSocket and Web Crypto) through the relay to a loopback Murmur, trusting
  the certificate it is shown.

Left for the web client: text, history and the Nuschl extension through the SDK
(today the browser session speaks plain Mumble only), voice (WebAudio and
getUserMedia, datagrams through the tunnel), storage (IndexedDB) and identity in a
Worker, the Svelte client itself, its version requirement, building the site with
the client in Nix, and a real-browser test.
