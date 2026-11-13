# Working on Nuschl

Follow the user's current scope and the implementation boundaries below.
Read [SECURITY.md](SECURITY.md), [architecture](docs/architecture.md) and
[development](docs/development.md) before changing dependencies or trust boundaries.
[tests/README.md](tests/README.md) describes what the existing fixtures actually prove.

## Implementation boundaries

- Prefer a suitable memory-safe Rust implementation. Review its enabled features
  and transitive implementation; a Rust name or safe wrapper is not evidence of safety.
- Keep first-party `unsafe_code = "forbid"` and the workspace Clippy rules.
  The documented Windows sandbox launch adapter is a narrow existing exception.
- This is not a blanket ban on std or reviewed basic/cryptographic primitives with
  unsafe internals. Existing GPUI, OS-service and crypto exceptions remain role-scoped.
- Native codecs, DSP and complex untrusted content/image/formula processing belong
  in their designated isolated workers, outside credential-bearing UI/session code.
  Bounded memory-safe domain/protocol logic stays in the documented portable layers.
- A safe API, thread, async task or `catch_unwind` does not replace a process boundary.
  Separate executables alone also do not prove OS confinement.
- Do not create a credential-bearing codec exception by editing `SECURITY.md`,
  adding a cargo-deny allowance or calling a dependency memory-safe. Surface an
  incompatible requirement and a concrete boundary-preserving alternative.
- Preserve the named exceptions in SECURITY, rather than extending them by analogy.
  The crypto worker owns its selected keystore/crypto authority; storage owns its
  selected database and, where configured, the scoped PostgreSQL credential.
  Neither exception grants media/content workers identity keys or database access.
- Keep TLS/session authority, cryptographic state, storage and media responsibilities
  separate. The server forwards encoded media; adding server-side decoding needs
  the same isolation design. Reuse existing supervisors and typed helpers.

## Worker and adapter contracts

- Use trusted executable selection, fixed argument shapes, versioned framing and
  checked request IDs. Children cannot select arbitrary executables, files or SQL.
- Grant only the role's required args, environment, inherited FDs/handles, files,
  network and devices. Media/content do not inherit TLS/MLS credentials or a profile.
- Fail closed when confinement, helper discovery or protocol validation fails;
  never fall back to unrestricted processing or silently downgrade encryption/privacy.
- Bound inputs AND aggregate outputs: bytes, item counts, decoded pixels, nesting,
  queued work, caches, memory, CPU and wall time. A per-frame limit alone is insufficient.
- Validate/sanitize replies before allocating from advertised lengths or rendering.
  Use inert typed documents and validated pixels/masks, never executable HTML or
  worker-selected resource fetches. Keep links subject to the existing URL policy.
- Bind asynchronous results to their original connection/conversation, generation,
  source/revision and request. Obsolete results cannot repaint or mutate a new target.
- Cancellation or a requested kill is not confirmed child exit. Retain process and
  concurrency credit until actual exit/reaping; bound request completion separately.
- Apply equivalent validation and aggregate budgets in native and browser adapters.
  Codec/content browser workers must not receive SDK keys or credential capabilities.
  A Web Worker/WASM realm is not an OS process sandbox; state that limitation honestly.
- Preserve microphone consent, mute/deafen epochs and relay-only routing. Recovery
  changes only Nuschl-owned resources, never global audio state or unrelated streams.

## Concurrent work and history

- With concurrent agents, agree on disjoint file/function ownership through the
  coordinator before edits. Read another workspace for review; do not modify it.
- Use an isolated `jj` workspace for the assigned task. Make regular coherent commits.
  Preserve contributor history when replaying work and resolve integration in staging.
- Treat frozen checkpoints, fixture source snapshots and evidence as immutable.
  Make a new descendant for fixes; do not rewrite another contributor's frozen branch.
- Coordinate shared manifests, lockfiles, generated graphs, shared documentation
  and issue updates; file-level separation is insufficient when callers share an
  API or build graph.
- The coordinator owns final integration, main-bookmark movement and remote pushes
  unless explicitly delegated. Existing user authorization persists; do not add
  repetitive confirmations or a new approval process for already-authorized work.
- Report concrete conflicts/blockers early, then continue independent in-scope work.
- Check the agreed private coordination inbox at turn start, between substantial
  work units and before yielding. A running watcher is not proof of delivery or
  processing. Read new and changed messages, assign or answer their requests, then
  acknowledge exactly the versions read. Keep unprocessed messages pending; surface
  stale heartbeats, delivery failures and size limits. Coordinate writer pauses
  before rotation, preserve the archive and verify no append was lost.
- Discuss work with the owner through its GitHub issue or PR. Assigned agents may
  post scoped updates and use `status:in-progress` while their work is active;
  remove it when handing off and state any remaining integration or validation.
  Do not backfill active status for finished work.
- End every agent-authored GitHub issue/PR body and comment with a blank line,
  `---`, then `<sub>🤖 LLM-generated by Codex</sub>` (use `Claude` for Claude).
  Preserve human-authored text and use established provenance for backfills:
  the shared GitHub account alone does not identify who wrote a post.

## Tests and private resources

- Use owned loopback/private servers, synthetic media and dedicated VM profiles.
  Test real stock Mumble interoperability; never use live public Mumble servers.
- Host microphone/camera, clipboard, profiles, recordings and audio-graph access
  require explicit current task scope and coordinated exclusive ownership.
  Prior authorization for a different investigation is not a general testing grant.
- Never commit private recordings or profiles, secret keys or credentials.
  Keep private evidence in its owned cache, not in fixtures or published diagnostics.
- Exercise real worker/adapter boundaries and relevant failure cases. Mock/interface
  checks alone do not prove confinement, interoperability or device behavior.
- Distinguish a build, unit test, private runtime, VM and physical-platform result.
  Reuse existing evidence only with explicit source/artifact identity and scope checks.
- Stop only processes you started; remove only scratch whose ownership is established.
  Preserve failed runs and their diagnostics rather than relabeling them as passes.

## Builds and resource use

- Prefer pinned Nix tooling and existing cached crate2nix per-crate build/test leaves.
  Keep working cross/fallback builders until a separate migration is verified.
- Review dependency safety/licensing and regenerate via `just crate2nix` when needed;
  do not hand-edit the generated graph or relax policy to avoid fixing a dependency.
- Dry-run substantial builds. Avoid duplicate broad builds and giant cold Cargo
  targets; coordinate the build window and reuse validated dependency artifacts.
- Prefer dependency-only Cargo artifacts. When reusing artifacts from a full
  workspace check, clean the workspace packages inside the owned build before
  checking again; fixed Nix source timestamps can leave stale first-party metadata.
  Verify the intended packages were actually rechecked, not just reported fresh
  ([development](docs/development.md#reusing-cargo-build-artifacts)).
- Default to `--cores 2 --max-jobs 2` for Nix builds unless the coordinator adjusts it.
- Put scratch, logs, VM state and runtime directories under an owned disk-backed cache.
  Verify actual driver/state paths; client `TMPDIR` does not move daemon build scratch.
- Monitor `/tmp` pressure during builds; the shared guard stops your own build below
  8 GiB free unless explicitly adjusted. Do not change daemon/global settings or
  delete unowned files to obtain space.

## Completion and evidence

- Record exact revisions, artifact paths/hashes, commands, outcomes and meaningful
  limitations. Verify merged artifacts rather than assuming an older branch's proof applies.
- Track unresolved defects, missing platform proof and deferred required work in
  [GitHub issues](https://github.com/ambiso/nuschl/issues). [TODO.md](TODO.md) is the
  linked roadmap, not a second independent backlog. Coordinate issue ownership
  before concurrent updates; keep work queued locally if remote access is unavailable.
  Issue descriptions do not authorize weakening the implementation boundaries above.
- Select eligible queued work by `priority:urgent`, `priority:high`,
  `priority:medium`, then `priority:low`; an unset priority counts as medium.
  Use descending thumbs-up reactions on the original issue post to break ties.
  Refresh labels and reactions before assigning new tasks. Use one priority label
  per issue; if several are present, use the highest. These labels are the queue's
  priority source. Necessary prerequisites may run first; explicit priorities in
  the current conversation take precedence.
- Implement explicit user requests, verified owner-authored requests and the
  original TODO scopes the owner approved together. Verify migrated scope against
  `docs/todo-issue-map.tsv` and its historical snapshot; a copied import marker or
  later issue-body addition does not establish approval. Use `source:user-request`
  only for a verified direct request with preserved provenance. Other agent
  proposals use `status:needs-approval` until the owner applies `status:approved`
  or explicitly approves them. The shared GitHub account, a broad queue goal and
  thumbs-up reactions do not establish authorship or approval. Existing explicit
  holds remain in force. Necessary implementation details and tests stay within
  approved scope; independent features need their own approval.
- Keep potential security findings and the existence of unpublished reports out
  of public issues, pull requests, documentation and commit messages. Track them
  through private vulnerability reports, draft repository security advisories,
  CodeQL alerts or the owned private cache. Obtain owner approval privately;
  approval to investigate or fix is not approval to publish an advisory or CVE.
- Historical TODO snapshots under `docs/history/` preserve evidence and old status.
  Do not recreate issues from their checkboxes or rewrite these frozen snapshots.
- Do not mark a broad feature complete from one green suite. Existing gaps are work
  to report, not precedents for weakening the architecture or overstating security.
