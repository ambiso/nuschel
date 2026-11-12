# Emphasized domains in welcome notices

The November 12 report was `<b>something.de</b>` appearing as
`bsomething.de/b` after joining a server. The current native package
`/nix/store/462ayz1f9900zn9iwwjxw71dax28xfna-nuschl-0.1.0`, from root
`95a1a27f`, passes the exact-domain check against owned loopback stock Murmur
inside private GNOME/null Pulse. The worker returns visible `something.de` with
a bold span covering bytes 0–12. The desktop shows that text without tag names
or storage markers. No new native production fix is claimed: this is precise
regression evidence for the existing typed welcome-document path.

`tests/welcome_domain.py` additionally checks strong emphasis, an explicit HTTPS
URL inside bold, a bold explicit anchor, escaped literal tags, multiline text
and a Rust code fence. Remote images/scripts remain inert and no public URL is
activated. The existing welcome harness is unchanged. The first fixture trial
stopped before GUI startup because its escaped literal `<b>` control conflicted
with that harness's broad assertion; the final GUI control uses escaped `<em>`
and the worker samples retain the literal `<b>` case.

Evidence is retained in
`~/.cache/nuschl-next/welcome-domain/native-current2/`: `proof.json`,
`domain-worker.json`, `domain-visible.txt` and `domain-welcome.png`. The screenshot
was visually reviewed. Worker-only initial samples against the earlier native
bundle are separate in `worker-initial.json`.

The web client has a distinct open gap: its Mumble `SERVER_SYNC` decoder does
not forward `welcome_text`, and its snapshot/UI has no server welcome-document
surface. It currently omits the notice rather than reproducing the reported
marker text. Targeted safe-renderer/worker/snapshot/UI parity work is authorized
but was paused before any production edits for higher-priority administration
work. Preserve literal text/code, bounded sources, connection/source leases and
explicit safe-link activation when implementing it.
