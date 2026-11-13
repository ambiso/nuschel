// SPDX-License-Identifier: EUPL-1.2
//! Sample-clock-driven receiver; all codec work stays in the isolated worker.
use super::{MAX_DECODED_SAMPLES, OUTPUT_CHANNELS, VoiceDecoder};
use nuschl_audio_jitter::{JitterBuffer, MAX_SEQUENCE, Pull, UnderrunFade};
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
    fade: UnderrunFade,
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
            fade: UnderrunFade::default(),
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
            let was_playing = self.jitter.is_playing();
            let count = match self.jitter.pull() {
                Pull::Packet { opus: queued, end } => {
                    let plays = !end || self.fade.end();
                    let count = self
                        .decoder
                        .decode(&queued.opus, MAX_PACKET_SAMPLES, &mut self.pcm)
                        .map_err(|error| error.to_string())?;
                    self.jitter.observe_frames((count / PLAYOUT_FRAMES) as u64);
                    self.gain = queued.gain;
                    self.last_kind = 1;
                    if end {
                        self.decoder
                            .reset_state()
                            .map_err(|error| error.to_string())?;
                        if !plays {
                            self.last_kind = 0;
                            return Ok(Vec::new());
                        }
                    }
                    self.fade.play(&mut self.pcm[..count * OUTPUT_CHANNELS]);
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
                    self.fade.play(&mut self.pcm[..count * OUTPUT_CHANNELS]);
                    count
                }
                Pull::Idle if self.fade.ran_dry(was_playing) => {
                    self.last_kind = 3;
                    let count = self
                        .decoder
                        .decode(&[], PLAYOUT_FRAMES, &mut self.pcm)
                        .map_err(|error| error.to_string())?;
                    if count != PLAYOUT_FRAMES {
                        return Err("concealed duration mismatch".into());
                    }
                    self.fade.fade_out(&mut self.pcm[..count * OUTPUT_CHANNELS]);
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
        self.fade = UnderrunFade::default();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, MAX_PACKET_BYTES, OpusEncoder};
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

    #[test]
    fn an_underrun_fades_out_and_back_in_and_a_late_terminator_stays_silent() {
        // A 0.5 sine: packets 0..15 arrive on time, the sender stalls for
        // 300 ms, sends 15..30 and runs dry again, and its terminator comes
        // last. Playout fades to silence and back instead of stepping, and the
        // terminator only ends audio that has already faded out (issue #206).
        let mut encoder =
            OpusEncoder::new(crate::SAMPLE_RATE as i32, 1, Application::Audio).unwrap();
        let mut bytes = [0; MAX_PACKET_BYTES];
        let packets: Vec<Vec<u8>> = (0..31)
            .map(|index| {
                let pcm: Vec<f32> = (0..960)
                    .map(|n| {
                        let t = (index * 960 + n) as f32 / 48_000.;
                        0.5 * (std::f32::consts::TAU * 440. * t).sin()
                    })
                    .collect();
                let size = encoder.encode(&pcm, 960, &mut bytes).unwrap();
                bytes[..size].to_vec()
            })
            .collect();
        let mut receiver = VoicePlayout::new().unwrap();
        let mut left = Vec::new();
        for tick in 0..140 {
            for (index, opus) in packets.iter().enumerate() {
                let at = match index {
                    0..15 => 20 * index,
                    15..30 => 20 * index + 300,
                    _ => 1200,
                };
                if at == 10 * tick {
                    let (sequence, arrival) = (2 * index as u64, 1000 * at as u64);
                    receiver
                        .push(sequence, opus, index == 30, 1., arrival)
                        .unwrap();
                }
            }
            let block = receiver.render().unwrap();
            left.extend(block.iter().step_by(OUTPUT_CHANNELS));
            left.resize(PLAYOUT_FRAMES * (tick + 1), 0.);
        }
        let silent = left
            .chunks(PLAYOUT_FRAMES)
            .filter(|frame| frame.iter().all(|v| *v == 0.));
        assert!(silent.count() > 40, "the stream must run dry twice");
        // A 0.5 sine at 440 Hz changes by at most 0.029 per sample.
        let step = left
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0., f32::max);
        assert!(step < 0.05, "{step}");
        let after = &left[110 * PLAYOUT_FRAMES..];
        assert!(after.iter().all(|v| *v == 0.), "terminator played");
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
