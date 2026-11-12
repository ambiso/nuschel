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
        let frames = packet::samples_48k(opus).map_err(|error| error.to_string())?;
        if frames == 0 || frames > MAX_PACKET_SAMPLES || frames % PLAYOUT_FRAMES != 0 {
            return Err("invalid Mumble packet duration".into());
        }
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

    /// Exactly 10 ms of raw stereo, or empty while buffering/idle. At most one
    /// packet/FEC/PLC decode is performed per pull; long packets use cached PCM.
    pub fn render(&mut self) -> Result<Vec<f32>, String> {
        self.last_kind = 0;
        if self.cursor == self.ready {
            let count = match self.jitter.pull() {
                Pull::Packet { opus: queued, end } => {
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
