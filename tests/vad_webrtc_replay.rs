// SPDX-License-Identifier: EUPL-1.2
// Explicit cache-only A/B; the supplied audio and all derived metrics stay
// outside the repository. Never prints PCM, encoded packets or transcripts.
use crate::{dsp, old_dsp};
use nuschl_core::audio::AudioSettings;
use serde_json::json;
use std::{fs, io::Write};

#[test]
#[ignore = "explicit user-owned/private Float32 WAV supplied by the caller"]
fn compare_owned_recording() {
    let bytes = fs::read(std::env::var_os("NUSCHL_PRIVATE_WAV").unwrap()).unwrap();
    assert!(
        bytes.len() >= 12
            && bytes.len() <= 16_000_000
            && &bytes[..4] == b"RIFF"
            && &bytes[8..12] == b"WAVE"
    );
    let mut fmt = None;
    let mut data = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let start = at + 8;
        let end = start.checked_add(len).unwrap();
        assert!(end <= bytes.len());
        if &bytes[at..at + 4] == b"fmt " {
            fmt = Some(&bytes[start..end]);
        }
        if &bytes[at..at + 4] == b"data" {
            data = Some(&bytes[start..end]);
        }
        at = end + (len % 2);
    }
    let fmt = fmt.unwrap();
    assert!(fmt.len() >= 16);
    assert_eq!(u16::from_le_bytes(fmt[..2].try_into().unwrap()), 3);
    assert_eq!(u16::from_le_bytes(fmt[2..4].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(fmt[4..8].try_into().unwrap()), 48000);
    assert_eq!(u16::from_le_bytes(fmt[14..16].try_into().unwrap()), 32);
    let samples: Vec<f32> = data
        .unwrap()
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    assert!(samples.iter().all(|s| s.is_finite()) && samples.iter().any(|s| s.abs() > 1e-8));
    let mut out = fs::File::create(std::env::var_os("NUSCHL_PRIVATE_METRICS").unwrap()).unwrap();
    let mut frame_out =
        std::env::var_os("NUSCHL_PRIVATE_FRAME_METRICS").map(|p| fs::File::create(p).unwrap());
    let thresholds = [
        (0.4, 0.1),
        (0.4, 0.2),
        (0.4, 0.25),
        (0.5, 0.2),
        (0.5, 0.25),
        (0.6, 0.3),
        (0.9, 0.45),
        (0.9, 0.6),
    ];
    for ns in [false, true] {
        let settings = AudioSettings {
            noise_suppression: ns,
            ..Default::default()
        };
        let mut old = old_dsp::CapturePipeline::new(&settings, false);
        let mut new = dsp::CapturePipeline::new(&settings, false);
        let mut gates: Vec<_> = thresholds
            .into_iter()
            .map(|(open, close)| dsp::diagnostic_thresholds(&settings, open, close))
            .collect();
        let mut counts = [0_usize; 9];
        let mut missed_loud = [0_usize; 9];
        let mut loud = 0_usize;
        let mut probability = [0_f64; 2];
        let mut audio = [0_f64; 2];
        for (n, input) in samples.chunks_exact(480).enumerate() {
            let input: &[f32; 480] = input.try_into().unwrap();
            let mut a = *input;
            let mut b = *input;
            let ar = old.process(&mut a, false, false);
            let br = new.process(&mut b, false, false);
            assert!(
                a.iter()
                    .zip(&b)
                    .all(|(old, new)| old.to_bits() == new.to_bits()),
                "VAD changed microphone samples or gain; sample data deliberately omitted"
            );
            let mut active = [false; 9];
            active[0] = ar.transmit;
            for (k, gate) in gates.iter_mut().enumerate() {
                active[k + 1] = gate(br.voice, br.level_db);
            }
            assert_eq!(
                br.transmit, active[2],
                "production default must match calibrated 0.4/0.2 policy"
            );
            let raw = dsp::level_db(input);
            let energetic = raw > -50.;
            loud += usize::from(energetic);
            for (k, on) in active.into_iter().enumerate() {
                counts[k] += usize::from(on);
                missed_loud[k] += usize::from(energetic && !on);
            }
            probability[0] += f64::from(ar.voice);
            probability[1] += f64::from(br.voice);
            audio[0] += input.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / 480.;
            audio[1] += b.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / 480.;
            if let Some(log) = &mut frame_out {
                writeln!(
                    log,
                    "{}",
                    json!([n * 10, ns, raw, br.level_db, ar.voice, br.voice, active])
                )
                .unwrap();
            }
            if (n + 1) % 100 == 0 {
                writeln!(out,"{}",json!({"second":(n+1)/100,"NS":ns,"gate_frames":counts,"thresholds_after_baseline":thresholds,"rnnoise_mean":probability[0]/100.,"webrtc_mean":probability[1]/100.,"input_dbfs":10.*(audio[0]/100.).max(1e-30).log10(),"processed_dbfs":10.*(audio[1]/100.).max(1e-30).log10(),"input_frames_above_minus50":loud,"gate_closed_while_input_above_minus50":missed_loud,"PCM_bit_identical":true,"limits":"energy is not a speech annotation; file bins are not phoneme onsets"})).unwrap();
                counts = [0; 9];
                missed_loud = [0; 9];
                loud = 0;
                probability = [0.; 2];
                audio = [0.; 2];
            }
        }
        out.flush().unwrap();
    }
}

#[test]
#[ignore = "explicit approximate host-local CPU measurement, no timing assertion"]
fn capture_cpu_cost() {
    let settings = AudioSettings::default();
    let frames: Vec<[f32; 480]> = (0..200)
        .map(|n| {
            std::array::from_fn(|i| {
                let t = (n * 480 + i) as f32 / 48000.;
                0.03 * (2. * std::f32::consts::PI * 220. * t).sin()
                    * (0.5 + 0.5 * (2. * std::f32::consts::PI * 3. * t).sin())
            })
        })
        .collect();
    for trial in 0..3 {
        let mut old = old_dsp::CapturePipeline::new(&settings, false);
        let mut new = dsp::CapturePipeline::new(&settings, false);
        for n in 0..200 {
            old.process(&mut frames[n].clone(), false, false);
            new.process(&mut frames[n].clone(), false, false);
        }
        let mut elapsed = [std::time::Duration::ZERO; 2];
        for n in 0..3000 {
            let mut a = frames[n % frames.len()];
            let mut b = a;
            for which in if n % 2 == 0 { [0, 1] } else { [1, 0] } {
                let at = std::time::Instant::now();
                if which == 0 {
                    std::hint::black_box(old.process(&mut a, false, false));
                } else {
                    std::hint::black_box(new.process(&mut b, false, false));
                }
                elapsed[which] += at.elapsed();
            }
            assert_eq!(a.map(f32::to_bits), b.map(f32::to_bits));
        }
        eprintln!(
            "{}",
            json!({"trial":trial,"frames":3000,"baseline_ns_per_10ms":elapsed[0].as_nanos()/3000,"candidate_ns_per_10ms":elapsed[1].as_nanos()/3000,"limits":"approximate host-local interleaved measurement, no portable speed claim","PCM_bit_identical":true})
        );
    }
}

#[test]
#[ignore = "explicit private numeric frame metrics; uses marker PCM, never user audio"]
fn audit_prefix_markers() {
    let text =
        fs::read_to_string(std::env::var_os("NUSCHL_PRIVATE_FRAME_METRICS").unwrap()).unwrap();
    assert!(text.len() <= 16_000_000);
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let start = std::env::var("NUSCHL_PRIVATE_SPEECH_START_SECONDS")
        .unwrap()
        .parse::<usize>()
        .unwrap()
        * 100;
    let noise_bounds = std::env::var("NUSCHL_PRIVATE_NOISE_START_SECONDS")
        .ok()
        .zip(std::env::var("NUSCHL_PRIVATE_NOISE_END_SECONDS").ok())
        .map(|(start, end)| {
            (
                start.parse::<usize>().unwrap() * 100,
                end.parse::<usize>().unwrap() * 100,
            )
        });
    let mut out =
        fs::File::create(std::env::var_os("NUSCHL_PRIVATE_PREFIX_METRICS").unwrap()).unwrap();
    for ns in [false, true] {
        let frames: Vec<_> = rows.iter().filter(|r| r[1].as_bool() == Some(ns)).collect();
        for policy in 0..9 {
            let mut queue = crate::capture_queue::CaptureQueue::default();
            let clock = std::time::Instant::now();
            let mut covered = vec![false; frames.len()];
            let mut delivered_at = vec![None; frames.len()];
            for n in 0..frames.len() + 12 {
                let marker = if n < frames.len() { (n + 1) as f32 } else { 0. };
                let gate = n < frames.len() && frames[n][6][policy].as_bool().unwrap();
                queue.push([marker; 480], gate, true);
                if let Some(packet) =
                    queue.due(clock + std::time::Duration::from_millis(n as u64 * 10))
                {
                    for value in [packet.samples[0], packet.samples[480]] {
                        if value > 0. {
                            let index = value as usize - 1;
                            assert!(index < frames.len());
                            covered[index] = true;
                            delivered_at[index].get_or_insert(n * 10);
                        }
                    }
                }
            }
            let loud: Vec<_> = (start..frames.len().saturating_sub(12))
                .filter(|&n| frames[n][2].as_f64().unwrap() > -50.)
                .collect();
            let missing: Vec<_> = loud
                .iter()
                .filter(|&&n| !covered[n])
                .map(|&n| n * 10)
                .collect();
            let first_loud = loud.first().copied();
            let noise_covered = noise_bounds.map(|(from, to)| {
                assert!(from < to && to <= frames.len());
                covered[from..to].iter().filter(|&&present| present).count()
            });
            let row = json!({"NS":ns,"policy":policy,"speech_start_frame":start,"energetic_frames":loud.len(),"uncovered_energetic_frames":missing.len(),"uncovered_frame_times_ms":missing,"first_energetic_frame_ms":first_loud.map(|n|n*10),"first_energetic_delivered_at_ms":first_loud.and_then(|n|delivered_at[n]),"requested_noise_bounds_frames":noise_bounds,"covered_noise_source_frames":noise_covered,"prefix_frames":crate::capture_queue::PREFIX_FRAMES,"clock":"actual CaptureQueue sample-clock, frame+1 marker values only","limits":"energy threshold is not phoneme annotation; exclude final120ms/clipped recording ending; final queue drained with marker-only silence, not a privacy-stop claim"});
            writeln!(out, "{row}").unwrap();
        }
    }
}
