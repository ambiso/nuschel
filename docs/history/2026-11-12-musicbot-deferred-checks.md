# Deferred workspace checks from the musicbot submission

These observations record the checkpoint below. Current work is tracked through
[the roadmap](../../TODO.md) and its linked GitHub issues.

Observed during the musicbot rebase onto `origin/main` at `b5e301e`, with the
pinned Rust 1.99.1 toolchain. The user explicitly requested that these unrelated
findings be recorded and left unfixed. Temporary source edits were reverted;
they are not part of the musicbot submission.

`direnv exec . just check` stopped at workspace Clippy on the findings below, so
that invocation did **not** reach the full workspace tests or live advisory check.
Musicbot-scoped tests, Clippy and private-server checks are separate evidence.
Do not describe the whole workspace as green based on those scoped results.

- `crates/core/src/preview.rs`, `Preview::validate`: nested image validation
  triggers `clippy::collapsible_if`. This was an observed lint refusal, not a
  reproduced preview-behavior failure.
- `crates/session/src/connection.rs`, `VoiceRoute::suspend` and
  `suspend_revision`: `AtomicU64::fetch_update` is deprecated in Rust 1.99.
  `try_update` is available at the declared Rust 1.95 MSRV. The
  `SelectVoiceTargetAt` authority guard also triggers `collapsible_if`;
  preserve its stale-target/privacy conditions in any later cleanup.
- `crates/client-wasm/src/session.rs`: UDP voice and permission-query guards
  trigger `collapsible_if`; the permission value also has a redundant `u32`
  cast. No browser behavior fix was included.
- `crates/client-wasm/src/storage.rs`, `Pending`: Clippy reports a 504-byte
  enum, with Append at least 500 bytes versus Page at least 144 bytes. Review
  bounded storage layout before adding per-message heap allocation merely
  to satisfy `large_enum_variant`.
- `crates/audio-wasm/src/lib.rs`, encoded-voice regression: constant
  `chunks_exact(OUTPUT_CHANNELS)` triggers `chunks_exact_to_as_chunks`.
- `apps/media-worker/src/capture_queue.rs`, `mixer.rs`, `dsp.rs`,
  `capture_fixture.rs`, `output_fixture.rs`, `playback.rs` and `reference.rs`:
  fixed-size `chunks_exact`/`chunks_exact_mut` calls trigger the new array-chunk
  lint. The DSP diagnostic also triggers `manual_is_multiple_of`. These
  traversal-only edits were reverted, not included in the bot work.
- `crates/update/src/trust.rs`: fixed two-byte hex traversal triggers the
  array-chunk lint. `crates/update/src/supervisor.rs` explicitly drops a
  non-Drop containment value on Linux. Any later lifetime cleanup must still
  retain the real Windows containment guard until verifier completion.
- `apps/tui/src/graphics.rs`: `Picker::from_fontsize` is deprecated at both
  configuration and raster-test callsites. Check half-block font/raster
  behavior before replacing it with the canonical constructor.
- `apps/tui/src/view.rs`, `draw_tree`: the generic Channel/Direct style arm
  makes the later Direct unread arm unreachable. Before making it reachable,
  review its `channels[*index]` lookup against the Direct row model. Runtime
  impact of that dormant branch is unverified.
- Vendored `speexdsp-resampler` emits four function-item-to-integer cast
  warnings; vendored `magic-wormhole` uses deprecated `time` Year/Month
  formatting components. These were warnings, not the scoped bot failures.
