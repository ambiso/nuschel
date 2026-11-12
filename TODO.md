# Nuschl roadmap

Open work is tracked in [GitHub issues](https://github.com/ambiso/nuschl/issues). Use the [roadmap and task index](https://github.com/ambiso/nuschl/issues/181) to browse all work by area. Issue status is authoritative; this file is not a separate checklist.

## Tracking issues

- [Deferred: iOS client and Linux-hosted Simulator feasibility](https://github.com/ambiso/nuschl/issues/3)
- [Track measured runtime memory reductions without weakening isolation](https://github.com/ambiso/nuschl/issues/47)
- [Track remaining E2EE feature and historical-membership work](https://github.com/ambiso/nuschl/issues/133)
- [Track reproducible Nix-built Linux distribution artifacts and runtime gates](https://github.com/ambiso/nuschl/issues/148)
- [Provide simple profile-preserving desktop installers](https://github.com/ambiso/nuschl/issues/163)
- [Track portable SDK, browser, terminal and mobile client milestones](https://github.com/ambiso/nuschl/issues/167)
- [Plan the Android client after the initial iOS milestone](https://github.com/ambiso/nuschl/issues/170)
- [Track remaining browser client parity and real SDK coverage](https://github.com/ambiso/nuschl/issues/171)

## Active work areas

- Audio: [Measure and reduce remaining unmute-to-first-speech latency](https://github.com/ambiso/nuschl/issues/72); [Preserve incoming playback across microphone mute/unmute](https://github.com/ambiso/nuschl/issues/73); [Validate the integrated VAD fix across quiet speech, noise and AEC conditions](https://github.com/ambiso/nuschl/issues/76).
- Worker boundaries: [Make worker failure recovery bounded, reaped and role-isolated](https://github.com/ambiso/nuschl/issues/20); [Remove runtime panic paths from credential-bearing supervisors](https://github.com/ambiso/nuschl/issues/19).
- Browser: [Complete the receipt-bound browser content broker and readable recovery](https://github.com/ambiso/nuschl/issues/172); [Implement bounded image selection, paste and sending in the browser](https://github.com/ambiso/nuschl/issues/12).
- Chat: [Implement sender-generated link previews with explicit fetch privacy](https://github.com/ambiso/nuschl/issues/11); [Implement persistent accessible polls with authenticated voting](https://github.com/ambiso/nuschl/issues/26).
- Music bot: [Integrate the musicbot branch with isolated codecs and bounded runtime checks](https://github.com/ambiso/nuschl/issues/22).

## Working with the queue

Record new work in an issue, agree on ownership before concurrent edits, and link any real prerequisites. Keep implementation and validation gaps open until their stated completion conditions are met. A staged branch or a passing unit suite does not establish platform or deployment readiness. Follow [AGENTS.md](AGENTS.md) for implementation boundaries and evidence.

Only explicit user requests or user-approved issues are eligible for implementation. `source:user-request` records verified direct requests; agent proposals carry `status:needs-approval` until the user applies `status:approved` or explicitly approves them in the working conversation. Imported issues were created through the owner's account, so their author alone does not prove approval. 👍 reactions rank eligible work; they do not approve it.

For eligible work, set the issue's **Priority** field to Urgent, High, Medium or Low. Work is selected by that priority first, then by the number of 👍 reactions on the original issue post within the same priority; necessary prerequisites may run first. See [GitHub's issue-field instructions](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/adding-and-managing-issue-fields). To browse a priority by votes, filter with `field.priority:high sort:reactions-+1-desc`.

Private security reports, credentials, recordings and profiles belong outside Git and public issues. Security findings and even the existence of unpublished reports stay in private vulnerability reports, draft security advisories, CodeQL alerts or the owned private cache. Obtain approval privately; do not create public security approval trackers or publish advisories without explicit owner instruction.

## Historical evidence

The [2026-11-13 TODO snapshot](docs/history/2026-11-13-todo-snapshot.md) preserves the completed history and original scoped evidence. Its checkbox states are historical, not a current backlog. The [source-to-issue map](docs/todo-issue-map.tsv) accounts for all 206 original unchecked entries, including two already-resolved entries, plus the residual work found in prose. Deferred work retains its existing status.

The [musicbot submission's deferred workspace checks](docs/history/2026-11-12-musicbot-deferred-checks.md)
preserve the findings left unfixed at the user's request, with their original
checkpoint and verification limits.
