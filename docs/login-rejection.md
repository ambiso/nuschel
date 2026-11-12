# Initial login rejection ordering

A server can send a Mumble `Reject` and close immediately after reading
`Authenticate`. The native session also sends its initial `UserState`. If that
write fails first, the previous session implementation aborted its reader and
returned the I/O error even when the explicit rejection was buffered. The
existing native client only opens password retry for the typed rejection, so
the user could instead see `Broken pipe`.

The session now resolves an **initial-login I/O failure only** against its same
verified TLS reader before aborting it. A valid explicit `Reject` retains its
normal type and bounded reason. The drain has an absolute 100 ms deadline and a
32-frame budget. A queued `ServerSync`, reader error/EOF or malformed `Reject`
stops it and preserves the original I/O failure. It does not classify EOF as a
bad password, retransmit credentials, reconnect, change certificate handling or
relax privacy/authentication. Completed logins and other error classes do not
wait. Login completion is latched immediately after handling the server frame,
before the next outgoing flush can fail.

The existing native error classifier and retry form are unchanged. This fixture
proves their typed error input; it is not an additional graphical retry-form
test or a general guarantee that every abruptly disconnected server delivered
an intact rejection.

## Reproduction and evidence

Run the session's cached per-crate Nix tests, then the real native CLI fixture:

```sh
python3 tests/login_rejection.py \
  --binaries /path/to/new-cli/bin \
  --server-binaries /path/to/nuschld-and-storage-helper/bin \
  --murmur /path/to/mumble-server \
  --output /disk/cache/login-rejection
```

The fixture starts only its own loopback servers, uses temporary disk profiles
and fixture passwords, confirms valid SuperUser login, then makes 40 independent
wrong-password attempts per server. This is explicit test activity, not an
automatic production credential retry. It never starts audio or a GUI and stops
only its own processes. The server's account-verification limiter may return
`AuthenticatorFail`; that explicit type is preserved rather than invented as a
password error.

On 2026-11-12 the unchanged baseline client produced **4 Broken pipe results in
40 nuschld attempts**, alongside 6 explicit wrong-password and 30 explicit
limiter rejections. Stock Murmur 1.5.939 returned 40 explicit wrong-password
rejections. The corrected client returned 10 wrong-password and 30 limiter
rejections from nuschld and 40 wrong-password rejections from Murmur, with no
generic I/O failures. Valid-login controls passed for both servers.

The session suite passed **96 tests, with one existing ignored test**. Four new
tests cover an actual in-memory transport write failure with a buffered framed
rejection (32 repetitions), EOF/malformed/Sync controls, post-login/non-I/O
zero-wait behavior, the absolute deadline under trickled traffic, frame limits
and shared rejection reason/type bounds. These transport controls do not replace
TLS verification; the real server fixture uses explicit independently obtained
certificate pins.

Exact source, artifacts, retained failed harness run and final receipts are in
`~/.cache/nuschl-login-reject/handoff.json` on the test host. No public server or
deployment was probed.
