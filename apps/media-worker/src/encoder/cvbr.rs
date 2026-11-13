// SPDX-License-Identifier: EUPL-1.2
//! Real constrained-VBR packets from the production TX factory against the
//! per-frame admission every sender applies and a server's bandwidth record.
//! A scalar `min(requested, budget)` is not enough: Opus CVBR varies frame
//! sizes around its target, and a frame over the destination's 20 ms
//! allowance is dropped by admission, not sent late. Synthetic PCM only.
use crate::encoder;
use nuschl_core::{
    audio::{MIN_VOICE_BITRATE, VOICE_OVERHEAD_BITRATE},
    bitrate::{MAX_VOICE_BITRATE, VoiceBitrate, max_voice_frame_bytes, voice_frame_fits},
};
use opus_pure::{OpusDecoder, OpusEncoder, packet};

const FRAME: usize = 960;
const RATE: f32 = 48_000.;
/// Long enough for the 360-packet server window to wrap (7.2 s of frames).
const FRAMES: usize = 400;

/// Deterministic white noise in [-1, 1].
struct Noise(u64);
impl Noise {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32 / (1u64 << 23) as f32) - 1.
    }
}

#[derive(Clone, Copy, Debug)]
enum Signal {
    /// Voiced syllables with a gliding pitch, formant weighting and
    /// fricative noise between them.
    Speech,
    /// Full-band noise: the hardest content for a rate target.
    Noise,
    /// Sparse loud clicks in near silence.
    Transients,
    /// Chords with partials up to 16 kHz.
    Music,
}

fn synthesize(signal: Signal) -> Vec<f32> {
    let mut noise = Noise(0x6e75_7363_686c);
    (0..FRAMES * FRAME)
        .map(|index| {
            let t = index as f32 / RATE;
            let white = noise.next();
            let tau = std::f32::consts::TAU;
            match signal {
                Signal::Speech => {
                    let envelope = (tau * 4. * t).sin().max(0.).powf(0.7);
                    // Phase of a pitch gliding 80..160 Hz.
                    let phase = tau * (120. * t - 40. / (tau * 0.7) * (tau * 0.7 * t).cos());
                    let f0 = 120. + 40. * (tau * 0.7 * t).sin();
                    let mut voiced = 0.;
                    for harmonic in 1..48 {
                        let frequency = f0 * harmonic as f32;
                        if frequency > 7_500. {
                            break;
                        }
                        let formants = [(700., 1.), (1_200., 0.6), (2_600., 0.3)]
                            .iter()
                            .map(|(centre, gain)| {
                                gain * (-((frequency - centre) / 250.).powi(2)).exp()
                            })
                            .sum::<f32>()
                            + 0.02;
                        voiced += formants * (phase * harmonic as f32).sin();
                    }
                    0.25 * envelope * voiced + 0.04 * (1. - envelope) * white
                }
                Signal::Noise => 0.25 * white,
                Signal::Transients => {
                    let position = index % 7_200;
                    if position < 96 {
                        0.8 * white * (-(position as f32) / 24.).exp()
                    } else {
                        0.0005 * white
                    }
                }
                Signal::Music => {
                    let swell = 0.6 + 0.4 * (tau * 0.5 * t).sin();
                    [220., 277.2, 329.6, 440., 5_274., 9_956., 15_804.]
                        .iter()
                        .enumerate()
                        .map(|(n, f)| (tau * f * t).sin() / (1. + n as f32))
                        .sum::<f32>()
                        * 0.12
                        * swell
                }
            }
        })
        .map(|sample| sample.clamp(-1., 1.))
        .collect()
}

/// Murmur's (and nuschld's) `BandwidthRecord`: the last 360 packets, refused
/// when their rate over that window would exceed the per-user budget.
struct Record {
    sizes: [u64; 360],
    times: [u64; 360],
    next: usize,
    sum: u64,
}
impl Record {
    fn new() -> Self {
        Self {
            sizes: [0; 360],
            times: [0; 360],
            next: 0,
            sum: 0,
        }
    }
    fn add(&mut self, size: u64, bytes_per_second: u64, now_us: u64) -> bool {
        let elapsed = now_us - self.times[self.next];
        if elapsed == 0 {
            return false;
        }
        let sum = self.sum - self.sizes[self.next] + size;
        if sum * 1_000_000 / elapsed > bytes_per_second {
            return false;
        }
        self.sizes[self.next] = size;
        self.times[self.next] = now_us;
        self.sum = sum;
        self.next = (self.next + 1) % 360;
        true
    }
}

/// What one destination's real packets looked like.
#[derive(Debug, Default)]
struct Run {
    bytes: usize,
    largest: usize,
    at_allowance: usize,
    over_allowance: usize,
    refused_by_server: usize,
}

/// The worker's encoder at `target` (as Configure sets it), with a packet
/// buffer of `buffer` bytes: Transmit::encode passes the destination's
/// per-frame allowance; `None` shows what CVBR would do without it.
fn encode(
    pcm: &[f32],
    target: u32,
    buffer: Option<usize>,
    budget: u32,
    mut check: impl FnMut(&[u8]),
) -> Run {
    let mut encoder: OpusEncoder = encoder::live().unwrap();
    encoder.bitrate_bps = target.clamp(MIN_VOICE_BITRATE, MAX_VOICE_BITRATE) as i32;
    let allowance = max_voice_frame_bytes(target);
    let mut output = vec![0; buffer.unwrap_or(1275)];
    let mut record = Record::new();
    let mut run = Run::default();
    for (index, frame) in pcm.as_chunks::<FRAME>().0.iter().enumerate() {
        let size = encoder.encode(frame, FRAME, &mut output).unwrap();
        let opus = &output[..size];
        packet::validate(opus).unwrap();
        check(opus);
        run.bytes += size;
        run.largest = run.largest.max(size);
        run.at_allowance += usize::from(size == allowance);
        run.over_allowance += usize::from(size > allowance);
        // The conservative reserve the policy subtracts: 60 bytes a packet
        // covers IP, UDP, the OCB2 header and either Mumble voice format.
        let accounted = (size + VOICE_OVERHEAD_BITRATE as usize / 400) as u64;
        let now = (index as u64 + 1) * 20_000;
        if !record.add(accounted, u64::from(budget / 8), now) {
            run.refused_by_server += 1;
        }
    }
    run
}

fn mean_bitrate(run: &Run) -> u32 {
    (run.bytes as u64 * 8 * 50 / FRAMES as u64) as u32
}

#[test]
fn constrained_vbr_packets_fit_admission_and_server_accounting_at_every_cap() {
    let signals = [
        Signal::Speech,
        Signal::Noise,
        Signal::Transients,
        Signal::Music,
    ];
    let pcm = signals.map(synthesize);
    // (server budget, requested): the Mumble 1.3/1.4 default 72k, 64k, very
    // low budgets, one just too small for voice, and the Murmur 1.5/nuschld
    // default with the requested choices it leaves in effect.
    let cases = [
        (72_000, 128_000),
        (64_000, 96_000),
        (40_000, 256_000),
        (30_000, 64_000),
        (29_999, 128_000),
        (558_000, 64_000),
        (558_000, 128_000),
        (558_000, 256_000),
        (558_000, 192_000),
    ];
    let mut report = Vec::new();
    for (budget, requested) in cases {
        let resolved = VoiceBitrate::resolve(requested, budget);
        let target = resolved.effective;
        if !resolved.transmits() {
            // No capture for this destination (speaking_to is false), and
            // admission refuses anything that would still arrive.
            for size in [1, 3, 15, 120, 960] {
                assert!(!voice_frame_fits(target, size));
            }
            report.push(format!("{budget:>7} {requested:>7} -> not sending"));
            continue;
        }
        for (signal, pcm) in signals.iter().zip(&pcm) {
            let mut decoder = OpusDecoder::new(48_000, 1).unwrap();
            let mut decoded = vec![0.; FRAME];
            let capped = encode(
                pcm,
                target,
                Some(max_voice_frame_bytes(target)),
                budget,
                |opus| {
                    assert!(
                        voice_frame_fits(target, opus.len()),
                        "{signal:?}: {} bytes at {target}",
                        opus.len()
                    );
                    assert_eq!(packet::samples_48k(opus).unwrap(), FRAME);
                    assert_eq!(decoder.decode(opus, FRAME, &mut decoded).unwrap(), FRAME);
                    assert!(decoded.iter().all(|value| value.is_finite()));
                },
            );
            assert_eq!(capped.over_allowance, 0);
            assert_eq!(
                capped.refused_by_server, 0,
                "{signal:?} at {target} against {budget}: the server would drop packets"
            );
            // On average the encoder stays at or under its target.
            assert!(
                mean_bitrate(&capped) <= target + 400,
                "{signal:?} {capped:?}"
            );
            let uncapped = encode(pcm, target, None, budget, |_| {});
            report.push(format!(
                "{budget:>7} {requested:>7} -> {target:>6} {signal:<10?} capped mean {:>6} max {:>3}/{:>3} at-cap {:>3}/{FRAMES} | uncapped mean {:>6} max {:>4} over-allowance {:>3}/{FRAMES} server-refused {:>3}",
                mean_bitrate(&capped),
                capped.largest,
                max_voice_frame_bytes(target),
                capped.at_allowance,
                mean_bitrate(&uncapped),
                uncapped.largest,
                uncapped.over_allowance,
                uncapped.refused_by_server,
            ));
        }
    }
    println!("budget requested -> effective signal | real packets");
    for line in report {
        println!("{line}");
    }
}

#[test]
fn one_encoder_for_two_destinations_fits_the_stricter_admission() {
    // Speaking on a 72k stock server and a 558k nuschld at once: the
    // supervisor encodes at the lowest effective target.
    let strict = VoiceBitrate::resolve(192_000, 72_000);
    let wide = VoiceBitrate::resolve(192_000, 558_000);
    let target = strict.effective.min(wide.effective);
    assert_eq!(wide.shared(target).effective, 48_000);
    let pcm = synthesize(Signal::Speech);
    encode(
        &pcm,
        target,
        Some(max_voice_frame_bytes(target)),
        72_000,
        |opus| {
            assert!(voice_frame_fits(strict.effective, opus.len()));
            assert!(voice_frame_fits(wide.effective, opus.len()));
        },
    );
}

#[test]
fn lowering_the_target_drops_queued_over_budget_frames_instead_of_replaying_them() {
    // Frames already encoded at 128k wait in a queue when the destination's
    // effective target falls to 48k (a lower request, or a ServerConfig).
    let pcm = synthesize(Signal::Speech);
    let mut queued = Vec::new();
    encode(
        &pcm[..50 * FRAME],
        128_000,
        Some(max_voice_frame_bytes(128_000)),
        558_000,
        |opus| queued.push(opus.to_vec()),
    );
    let lowered = VoiceBitrate::resolve(128_000, 72_000).effective;
    let admitted = queued
        .iter()
        .filter(|opus| voice_frame_fits(lowered, opus.len()))
        .count();
    let over = queued
        .iter()
        .filter(|opus| opus.len() > max_voice_frame_bytes(lowered))
        .count();
    // Admission is a filter: what does not fit is gone, never re-encoded or
    // resent, and what fits was already within the new budget.
    assert_eq!(admitted + over, queued.len());
    assert!(over > 0, "128k speech should exceed a 48k frame allowance");
    for opus in queued
        .iter()
        .filter(|opus| voice_frame_fits(lowered, opus.len()))
    {
        assert!(opus.len() <= max_voice_frame_bytes(lowered));
    }
    // New frames at the lowered target all pass.
    encode(
        &pcm[50 * FRAME..100 * FRAME],
        lowered,
        Some(max_voice_frame_bytes(lowered)),
        72_000,
        |opus| assert!(voice_frame_fits(lowered, opus.len())),
    );
}
