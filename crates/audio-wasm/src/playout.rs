// SPDX-License-Identifier: EUPL-1.2
//! Sample-clock-driven receiver; all codec work stays in the isolated worker.
use super::{MAX_DECODED_SAMPLES, OUTPUT_CHANNELS, VoiceDecoder};
use nuschl_audio_jitter::{JitterBuffer, MAX_SEQUENCE, Pull};
use opus_pure::{MAX_PACKET_SAMPLES, OpusDecoder, packet};
use std::time::Duration;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use wasm_bindgen::prelude::*;

pub const PLAYOUT_FRAMES: usize = 480;

#[derive(Clone)]
struct Queued {
    opus: Vec<u8>,
    gain: f32,
}

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
pub struct VoicePlayout {
    decoder: OpusDecoder,
    jitter: JitterBuffer<Queued>,
    pcm: Vec<f32>,
    cursor: usize,
    ready: usize,
    gain: f32,
    last_input: Option<Duration>,
    last_kind: u8,
    flow: Flow,
}

/// Whether decoded audio is being rendered (issue #206). Only the edges of an
/// underrun fade; every other sample is the decoder's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// The decoder is new or reset: nothing has played since creation, a
    /// reset or the transmission's end, and nothing needs to fade.
    Stopped,
    Playing,
    /// Playout ran dry mid-transmission and faded out. The decoder still
    /// carries that waveform, so whatever plays next fades in.
    Underrun,
}

/// Raised-cosine ramp over the first 10 ms of interleaved stereo: up from
/// silence when `rising`, else down to silence.
fn fade(samples: &mut [f32], rising: bool) {
    let frames = samples.as_chunks_mut::<OUTPUT_CHANNELS>().0;
    for (index, frame) in frames.iter_mut().take(PLAYOUT_FRAMES).enumerate() {
        let up =
            0.5 - 0.5 * (std::f32::consts::PI * (index as f32 + 0.5) / PLAYOUT_FRAMES as f32).cos();
        let gain = if rising { up } else { 1. - up };
        for sample in frame {
            *sample *= gain;
        }
    }
}

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
impl VoicePlayout {
    #[cfg_attr(
        all(target_arch = "wasm32", target_os = "unknown"),
        wasm_bindgen(constructor)
    )]
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            decoder: VoiceDecoder::new()?.decoder,
            jitter: JitterBuffer::default(),
            pcm: vec![0.; MAX_DECODED_SAMPLES],
            cursor: 0,
            ready: 0,
            gain: 1.,
            last_input: None,
            last_kind: 0,
            flow: Flow::Stopped,
        })
    }

    /// Encoded payload/gain stay together while reordered. Arrival time is
    /// monotonic microseconds supplied by the worker, never a platform clock.
    pub fn push(
        &mut self,
        sequence: u64,
        opus: &[u8],
        end: bool,
        gain: f32,
        arrival_us: u64,
    ) -> Result<(), String> {
        if sequence > MAX_SEQUENCE
            || opus.is_empty()
            || opus.len() > 1024
            || !gain.is_finite()
            || !(0. ..=4.).contains(&gain)
            || arrival_us > 9_007_199_254_740_991
        {
            return Err("invalid bounded voice packet".into());
        }
        Self::validate_packet(opus)?;
        let now = Duration::from_micros(arrival_us);
        if self.last_input.is_some_and(|previous| now < previous) {
            return Err("non-monotonic arrival clock".into());
        }
        if self
            .last_input
            .is_some_and(|previous| now.saturating_sub(previous) > Duration::from_secs(1))
            && !self.jitter.is_playing()
        {
            self.reset()?;
        }
        self.last_input = Some(now);
        self.jitter.push(
            sequence,
            Queued {
                opus: opus.to_vec(),
                gain,
            },
            end,
            now,
        );
        Ok(())
    }

    /// Stateless framing/duration check, also used before allocating a speaker.
    /// This validates Opus packing, not entropy-coded content or authenticity.
    pub fn validate_packet(opus: &[u8]) -> Result<(), String> {
        if opus.is_empty() || opus.len() > 1024 {
            return Err("invalid bounded voice packet".into());
        }
        packet::validate(opus).map_err(|error| error.to_string())?;
        let frames = packet::samples_48k(opus).map_err(|error| error.to_string())?;
        if frames == 0 || frames > MAX_PACKET_SAMPLES || frames % PLAYOUT_FRAMES != 0 {
            return Err("invalid Mumble packet duration".into());
        }
        Ok(())
    }

    /// Exactly 10 ms of raw stereo, or empty while buffering/idle. At most one
    /// packet/FEC/PLC decode is performed per pull; long packets use cached PCM.
    pub fn render(&mut self) -> Result<Vec<f32>, String> {
        self.last_kind = 0;
        if self.cursor == self.ready {
            let resuming = self.flow == Flow::Underrun;
            let count = match self.jitter.pull() {
                Pull::Packet { opus: queued, end } => {
                    let count = self
                        .decoder
                        .decode(&queued.opus, MAX_PACKET_SAMPLES, &mut self.pcm)
                        .map_err(|error| error.to_string())?;
                    self.jitter.observe_frames((count / PLAYOUT_FRAMES) as u64);
                    self.gain = queued.gain;
                    self.last_kind = 1;
                    self.flow = Flow::Playing;
                    if end {
                        self.decoder
                            .reset_state()
                            .map_err(|error| error.to_string())?;
                        self.flow = Flow::Stopped;
                        // A terminator straight after an underrun only ends
                        // audio that has already faded out.
                        if resuming {
                            self.last_kind = 0;
                            return Ok(Vec::new());
                        }
                    }
                    count
                }
                Pull::Lost { fec, frames } => {
                    let wanted = frames as usize * PLAYOUT_FRAMES;
                    let count = if let Some(next) = fec {
                        self.last_kind = 2;
                        self.decoder.decode_fec(&next.opus, wanted, &mut self.pcm)
                    } else {
                        self.last_kind = 3;
                        self.decoder.decode(&[], wanted, &mut self.pcm)
                    }
                    .map_err(|error| error.to_string())?;
                    if count != wanted {
                        return Err("concealed duration mismatch".into());
                    }
                    self.flow = Flow::Playing;
                    count
                }
                // Ran dry mid-waveform: continue it with one 10 ms block of
                // concealment that fades to silence, instead of a step.
                Pull::Idle if self.flow == Flow::Playing => {
                    self.flow = Flow::Underrun;
                    self.last_kind = 3;
                    let count = self
                        .decoder
                        .decode(&[], PLAYOUT_FRAMES, &mut self.pcm)
                        .map_err(|error| error.to_string())?;
                    if count != PLAYOUT_FRAMES {
                        return Err("concealed duration mismatch".into());
                    }
                    fade(&mut self.pcm[..count * OUTPUT_CHANNELS], false);
                    count
                }
                Pull::Idle => return Ok(Vec::new()),
            };
            if count == 0
                || count > MAX_PACKET_SAMPLES
                || count % PLAYOUT_FRAMES != 0
                || self.pcm[..count * OUTPUT_CHANNELS]
                    .iter()
                    .any(|value| !value.is_finite())
            {
                return Err("invalid decoded playout frame".into());
            }
            if resuming && self.flow == Flow::Playing {
                fade(&mut self.pcm[..count * OUTPUT_CHANNELS], true);
            }
            self.cursor = 0;
            self.ready = count * OUTPUT_CHANNELS;
        }
        let end = self.cursor + PLAYOUT_FRAMES * OUTPUT_CHANNELS;
        let samples = self.pcm[self.cursor..end].to_vec();
        self.cursor = end;
        Ok(samples)
    }

    /// Gain for the just-rendered block; concealment keeps the preceding gain.
    pub fn gain(&self) -> f32 {
        self.gain
    }
    /// 0=cache/idle,1=packet,2=following-packet FEC,3=PLC. Diagnostic, no UI event.
    pub fn last_render_kind(&self) -> u8 {
        self.last_kind
    }
    pub fn active(&self) -> bool {
        self.cursor < self.ready || self.jitter.is_playing() || self.jitter.buffered_ms() > 0.
    }
    pub fn target_ms(&self) -> f32 {
        self.jitter.target_ms()
    }
    pub fn buffered_ms(&self) -> f32 {
        self.jitter.buffered_ms()
    }

    /// Playback-only rebuffer after a clock stall. No capture/permission state.
    pub fn reset(&mut self) -> Result<(), String> {
        self.decoder
            .reset_state()
            .map_err(|error| error.to_string())?;
        self.jitter = JitterBuffer::default();
        self.pcm.fill(0.);
        self.cursor = 0;
        self.ready = 0;
        self.gain = 1.;
        self.last_input = None;
        self.last_kind = 0;
        self.flow = Flow::Stopped;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder, RateControl};
    fn packets() -> Vec<Vec<u8>> {
        let mut encoder =
            OpusEncoder::new(crate::SAMPLE_RATE as i32, 1, Application::Voip).unwrap();
        let mut bytes = [0; MAX_PACKET_BYTES];
        (0..6)
            .map(|frame| {
                let pcm: Vec<_> = (0..960)
                    .map(|sample| {
                        (std::f32::consts::TAU * 997. * (frame * 960 + sample) as f32 / 48000.)
                            .sin()
                            * 0.2
                    })
                    .collect();
                let size = encoder.encode(&pcm, 960, &mut bytes).unwrap();
                bytes[..size].to_vec()
            })
            .collect()
    }

    /// 20 ms packets of `channels`-channel PCM from `sample(time)`.
    fn encoded(
        count: usize,
        channels: usize,
        application: Application,
        sample: impl Fn(f32) -> [f32; 2],
    ) -> Vec<Vec<u8>> {
        let mut encoder =
            OpusEncoder::new(crate::SAMPLE_RATE as i32, channels, application).unwrap();
        encoder.rate_control = RateControl::ConstrainedVbr;
        encoder.bitrate_bps = 96_000;
        let mut bytes = [0; MAX_PACKET_BYTES];
        (0..count)
            .map(|index| {
                let pcm: Vec<f32> = (0..960)
                    .flat_map(|n| {
                        sample((index * 960 + n) as f32 / 48_000.)
                            .into_iter()
                            .take(channels)
                    })
                    .collect();
                let size = encoder.encode(&pcm, 960, &mut bytes).unwrap();
                bytes[..size].to_vec()
            })
            .collect()
    }

    fn wave(hz: f32, time: f32) -> f32 {
        (std::f32::consts::TAU * hz * time).sin()
    }

    /// Stereo synthetic music, encoded like the music bot's: a chord with
    /// overtones, a bass line and a melody with soft attacks.
    fn music(count: usize) -> Vec<Vec<u8>> {
        encoded(count, 2, Application::Audio, |t| {
            let note = [440., 494., 554., 659.][(t * 4.) as usize % 4];
            let attack = (t * 4.).fract();
            let envelope = (attack * 40.).min(1.) * (1. - 0.6 * attack);
            let chord: f32 = [220., 277.18, 329.63]
                .iter()
                .map(|hz| wave(*hz, t) + 0.3 * wave(2. * hz, t))
                .sum();
            let bass = 0.2 * wave(110., t);
            [
                0.08 * chord + bass + 0.15 * envelope * wave(note, t),
                0.06 * chord + bass + 0.12 * envelope * wave(1.5 * note, t),
            ]
        })
    }

    /// One packet, pushed when the virtual clock reaches `at` ms.
    struct Arrival {
        at: u64,
        sequence: u64,
        opus: Vec<u8>,
        end: bool,
        gain: f32,
    }

    /// One 10 ms render: raw stereo (empty while silent) and what it reported.
    struct Block {
        samples: Vec<f32>,
        gain: f32,
        kind: u8,
        active: bool,
    }

    /// Render `ticks` blocks on a virtual clock.
    fn clocked(arrivals: &[Arrival], ticks: u64) -> Vec<Block> {
        let mut receiver = VoicePlayout::new().unwrap();
        let mut blocks = Vec::new();
        let mut next = 0;
        for tick in 0..ticks {
            while let Some(arrival) = arrivals.get(next).filter(|a| a.at <= 10 * tick) {
                receiver
                    .push(
                        arrival.sequence,
                        &arrival.opus,
                        arrival.end,
                        arrival.gain,
                        1000 * arrival.at,
                    )
                    .unwrap();
                next += 1;
            }
            let samples = receiver.render().unwrap();
            blocks.push(Block {
                samples,
                gain: receiver.gain(),
                kind: receiver.last_render_kind(),
                active: receiver.active(),
            });
        }
        blocks
    }

    /// The receiver's decoder on its own, as an independent oracle.
    fn plain() -> OpusDecoder {
        let mut decoder = OpusDecoder::new(48_000, 2).unwrap();
        decoder.set_phase_inversion_disabled(true);
        decoder
    }

    /// Decode a packet, or conceal `frames` when `opus` is empty.
    fn decoded(decoder: &mut OpusDecoder, opus: &[u8], frames: usize) -> Vec<f32> {
        let mut pcm = vec![0.; MAX_DECODED_SAMPLES];
        let count = decoder.decode(opus, frames, &mut pcm).unwrap();
        pcm.truncate(count * OUTPUT_CHANNELS);
        pcm
    }

    /// Recover the 20 ms before `next` from its in-band FEC.
    fn recovered(decoder: &mut OpusDecoder, next: &[u8]) -> Vec<f32> {
        let mut pcm = vec![0.; MAX_DECODED_SAMPLES];
        let count = decoder.decode_fec(next, 960, &mut pcm).unwrap();
        pcm.truncate(count * OUTPUT_CHANNELS);
        pcm
    }

    /// The specified 10 ms raised-cosine edge: up from or down to silence.
    fn ramped(mut pcm: Vec<f32>, rising: bool) -> Vec<f32> {
        for (index, frame) in pcm.chunks_exact_mut(2).take(480).enumerate() {
            let up = 0.5 - 0.5 * (std::f32::consts::PI * (index as f32 + 0.5) / 480.).cos();
            for sample in frame {
                *sample *= if rising { up } else { 1. - up };
            }
        }
        pcm
    }

    /// Expected 10 ms blocks of `pcm`, as sample bits with their gain.
    fn blocks(pcm: &[f32], gain: f32) -> Vec<(Vec<u32>, f32)> {
        pcm.chunks(2 * PLAYOUT_FRAMES)
            .map(|block| (block.iter().map(|v| v.to_bits()).collect(), gain))
            .collect()
    }

    /// Rendered runs between silences must be exactly `expected`, bit for bit.
    fn assert_runs(rendered: &[Block], expected: &[Vec<(Vec<u32>, f32)>]) {
        let runs: Vec<Vec<(Vec<u32>, f32)>> = rendered
            .split(|block| block.samples.is_empty())
            .filter(|run| !run.is_empty())
            .map(|run| {
                let bits = |block: &Block| block.samples.iter().map(|v| v.to_bits()).collect();
                run.iter().map(|block| (bits(block), block.gain)).collect()
            })
            .collect();
        let lengths = |runs: &[Vec<(Vec<u32>, f32)>]| runs.iter().map(Vec::len).collect::<Vec<_>>();
        assert_eq!(lengths(&runs), lengths(expected), "10 ms blocks per run");
        for (index, (actual, expected)) in runs.iter().zip(expected).enumerate() {
            let differs = actual.iter().zip(expected).position(|(a, b)| a != b);
            assert_eq!(differs, None, "first differing block of run {index}");
        }
    }

    #[test]
    fn an_underrun_fades_out_and_the_resumed_stream_fades_in() {
        // Packets 0..15 arrive on time, then the sender stalls for 300 ms:
        // playout fades to silence and back instead of stepping (issue #206).
        let arrivals: Vec<Arrival> =
            encoded(40, 1, Application::Audio, |t| [0.5 * wave(440., t); 2])
                .into_iter()
                .enumerate()
                .map(|(index, opus)| Arrival {
                    at: 20 * index as u64 + if index < 15 { 0 } else { 300 },
                    sequence: 2 * index as u64,
                    opus,
                    end: false,
                    gain: 1.,
                })
                .collect();
        let rendered = clocked(&arrivals, 140);
        let silent = rendered.iter().filter(|block| block.samples.is_empty());
        assert!(silent.count() > 12, "the stall must underrun");
        let left: Vec<f32> = rendered
            .iter()
            .flat_map(|block| {
                let mut left: Vec<f32> = block
                    .samples
                    .iter()
                    .step_by(OUTPUT_CHANNELS)
                    .copied()
                    .collect();
                left.resize(PLAYOUT_FRAMES, 0.);
                left
            })
            .collect();
        // A 0.5 sine at 440 Hz changes by at most 0.029 per sample.
        let step = left
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0., f32::max);
        assert!(step < 0.05, "{step}");
    }

    #[test]
    fn an_underrun_changes_only_its_two_10_ms_edges() {
        // Music packets 0..20 arrive on time, then the sender stalls for 300 ms
        // and sends 20..40, the last as its terminator. The jitter buffer
        // conceals 40 ms as before and runs dry; one more 10 ms concealment
        // block fades out, and the resumed packet fades in over its first
        // 10 ms. Every other sample is the decoder's own.
        let packets = music(40);
        let gain = |index: usize| [1., 0.5, 2.][index % 3];
        let arrivals: Vec<Arrival> = packets
            .iter()
            .enumerate()
            .map(|(index, opus)| Arrival {
                at: 20 * index as u64 + if index < 20 { 0 } else { 300 },
                sequence: 2 * index as u64,
                opus: opus.clone(),
                end: index == 39,
                gain: gain(index),
            })
            .collect();
        let rendered = clocked(&arrivals, 160);
        let mut decoder = plain();
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for (index, opus) in packets.iter().enumerate() {
            let pcm = decoded(&mut decoder, opus, MAX_PACKET_SAMPLES);
            match index {
                0..20 => before.extend(blocks(&pcm, gain(index))),
                20 => after.extend(blocks(&ramped(pcm, true), gain(index))),
                _ => after.extend(blocks(&pcm, gain(index))),
            }
            if index == 19 {
                for _ in 0..2 {
                    before.extend(blocks(&decoded(&mut decoder, &[], 960), gain(19)));
                }
                let tail = ramped(decoded(&mut decoder, &[], 480), false);
                before.extend(blocks(&tail, gain(19)));
            }
        }
        assert_runs(&rendered, &[before, after]);
        // Concealment is the same 40 ms of PLC; only one 10 ms block is added.
        let kinds: Vec<u8> = rendered
            .iter()
            .filter(|block| !block.samples.is_empty())
            .map(|block| block.kind)
            .collect();
        let expected: Vec<u8> = [[1, 0]; 20]
            .into_iter()
            .chain([[3, 0]; 2])
            .flatten()
            .chain([3])
            .chain([[1, 0]; 20].into_iter().flatten())
            .collect();
        assert_eq!(kinds, expected);
        let gap = rendered.iter().skip_while(|block| block.kind != 3);
        let gap = gap.skip(5).take_while(|block| block.samples.is_empty());
        assert!(gap.count() > 20, "the stall must underrun");
    }

    #[test]
    fn without_an_underrun_music_is_bit_identical_to_plain_decoding() {
        // Two transmissions of music, each closed by its terminator, with a
        // reordered pair, a packet recovered from FEC, one concealed and
        // changing gains. Nothing runs dry, so nothing fades: steady-state
        // output and concealment are unchanged (issue #206).
        let packets = music(50);
        let gain = |index: usize| [1., 0.5, 2., 0.75][index % 4];
        let mut arrivals: Vec<Arrival> = packets
            .iter()
            .enumerate()
            .filter(|(index, _)| ![10, 20, 21].contains(index))
            .map(|(index, opus)| {
                let index = index as u64;
                let (at, sequence) = match index {
                    5 => (110, 10),
                    6 => (105, 12),
                    0..30 => (20 * index, 2 * index),
                    // The next transmission restarts its sequence numbers.
                    _ => (750 + 20 * (index - 30), 2 * (index - 30)),
                };
                Arrival {
                    at,
                    sequence,
                    opus: opus.clone(),
                    end: index == 29 || index == 49,
                    gain: gain(index as usize),
                }
            })
            .collect();
        arrivals.sort_by_key(|arrival| arrival.at);
        let rendered = clocked(&arrivals, 130);
        let mut decoder = plain();
        let (mut first, mut second, mut known) = (Vec::new(), Vec::new(), 1.);
        for (index, opus) in packets.iter().enumerate() {
            let pcm = match index {
                10 => recovered(&mut decoder, &packets[11]),
                // Nothing after it has arrived yet: concealment, then FEC.
                20 => decoded(&mut decoder, &[], 960),
                21 => recovered(&mut decoder, &packets[22]),
                _ => {
                    known = gain(index);
                    decoded(&mut decoder, opus, MAX_PACKET_SAMPLES)
                }
            };
            let transmission = if index < 30 { &mut first } else { &mut second };
            transmission.extend(blocks(&pcm, known));
            if index == 29 {
                decoder.reset_state().unwrap();
            }
        }
        assert_runs(&rendered, &[first, second]);
    }

    #[test]
    fn a_terminator_after_an_underrun_is_dropped_and_the_next_stream_starts_clean() {
        // The sender stalls after packet 14 and later sends only its
        // terminator: that is the tail of audio which already faded out, so
        // it renders nothing. The decoder resets as for any terminator, and
        // the next transmission plays bit-identically, without a fade.
        let tone = encoded(16, 1, Application::Audio, |t| [0.5 * wave(440., t); 2]);
        let music = music(10);
        let mut arrivals: Vec<Arrival> = tone
            .iter()
            .enumerate()
            .map(|(index, opus)| Arrival {
                at: if index < 15 { 20 * index as u64 } else { 600 },
                sequence: 2 * index as u64,
                opus: opus.clone(),
                end: index == 15,
                gain: 1.,
            })
            .collect();
        arrivals.extend(music.iter().enumerate().map(|(index, opus)| Arrival {
            at: 800 + 20 * index as u64,
            sequence: 2 * index as u64,
            opus: opus.clone(),
            end: index == 9,
            gain: 1.,
        }));
        let rendered = clocked(&arrivals, 120);
        assert!(
            rendered[40..80]
                .iter()
                .all(|block| block.samples.is_empty() && block.kind == 0 && !block.active)
        );
        let mut decoder = plain();
        let mut first = Vec::new();
        for opus in &tone[..15] {
            first.extend(blocks(&decoded(&mut decoder, opus, MAX_PACKET_SAMPLES), 1.));
        }
        for _ in 0..2 {
            first.extend(blocks(&decoded(&mut decoder, &[], 960), 1.));
        }
        first.extend(blocks(&ramped(decoded(&mut decoder, &[], 480), false), 1.));
        let mut decoder = plain();
        let second: Vec<_> = music
            .iter()
            .flat_map(|opus| blocks(&decoded(&mut decoder, opus, MAX_PACKET_SAMPLES), 1.))
            .collect();
        assert_runs(&rendered, &[first, second]);
    }

    #[test]
    fn reorder_keeps_gain_with_packets_and_end_never_extends_plc() {
        let packets = packets();
        let mut receiver = VoicePlayout::new().unwrap();
        for (index, at) in [(0, 0), (2, 20_000), (1, 25_000)] {
            receiver
                .push(
                    index as u64 * 2,
                    &packets[index],
                    index == 2,
                    0.25 + index as f32 * 0.25,
                    at,
                )
                .unwrap();
        }
        for gain in [0.25, 0.5, 0.75] {
            for _ in 0..2 {
                assert_eq!(receiver.render().unwrap().len(), 960);
                assert_eq!(receiver.gain(), gain);
            }
        }
        assert!(receiver.render().unwrap().is_empty());
        assert!(!receiver.active());
        assert!(receiver.decoder.phase_inversion_disabled());
        receiver.push(0, &packets[0], true, 1., 30_000).unwrap();
        assert_eq!(receiver.render().unwrap().len(), 960);
        assert_eq!(receiver.render().unwrap().len(), 960);
        assert!(receiver.render().unwrap().is_empty());
    }
    #[test]
    fn malformed_framing_cannot_change_timing_gain_or_the_following_packet() {
        let packets = packets();
        let mut clean = VoicePlayout::new().unwrap();
        let mut injected = VoicePlayout::new().unwrap();
        for receiver in [&mut clean, &mut injected] {
            receiver.push(0, &packets[0], false, 0.25, 0).unwrap();
            receiver.push(4, &packets[2], true, 0.5, 40_000).unwrap();
        }
        assert!(injected.push(2, &[0x09, 0], false, 4., 99_000).is_err());
        // A rejected future arrival must not advance the monotonic input clock.
        for receiver in [&mut clean, &mut injected] {
            receiver.push(2, &packets[1], false, 0.75, 45_000).unwrap();
        }
        for _ in 0..12 {
            assert_eq!(clean.render().unwrap(), injected.render().unwrap());
            assert_eq!(clean.gain(), injected.gain());
            assert_eq!(clean.last_render_kind(), injected.last_render_kind());
        }
    }

    #[test]
    fn loss_is_clock_driven_and_reset_discards_cached_pcm() {
        let packets = packets();
        let mut receiver = VoicePlayout::new().unwrap();
        for index in [0, 1, 3, 4] {
            receiver
                .push(
                    index as u64 * 2,
                    &packets[index],
                    false,
                    if index <= 1 { 0.25 } else { 3. },
                    index as u64 * 20_000,
                )
                .unwrap();
        }
        let mut fec = false;
        for _ in 0..10 {
            receiver.render().unwrap();
            if receiver.last_render_kind() == 2 {
                assert_eq!(receiver.gain(), 0.25, "FEC must not borrow future gain");
                fec = true;
            }
        }
        assert!(fec);
        receiver.reset().unwrap();
        assert!(!receiver.active());
        assert!(receiver.render().unwrap().is_empty());
        assert!(receiver.decoder.phase_inversion_disabled());
        assert!(receiver.push(0, &packets[0], false, f32::NAN, 0).is_err());
        assert!(receiver.push(0, &[0xff; 1025], false, 1., 0).is_err());
        receiver.push(0, &packets[0], false, 1., 20_000).unwrap();
        assert!(receiver.push(2, &packets[1], false, 1., 19_999).is_err());
    }
}
