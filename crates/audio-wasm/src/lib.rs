// SPDX-License-Identifier: EUPL-1.2
//! Codecs only: no device, clock, network, storage or session-key interface.
//! Load the generated WASM in its own bounded audio worker, never the TLS worker.
#![forbid(unsafe_code)]

use opus_pure::{Application, MAX_PACKET_SAMPLES, OpusDecoder, OpusEncoder, packet};
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use wasm_bindgen::prelude::*;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;
/// Leaves space for Mumble's header inside its 1024-byte tunneled voice bound.
pub const MAX_ENCODED_BYTES: usize = 960;
pub const MAX_DECODED_SAMPLES: usize = MAX_PACKET_SAMPLES;

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
pub struct VoiceEncoder {
    encoder: OpusEncoder,
}

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
impl VoiceEncoder {
    #[cfg_attr(
        all(target_arch = "wasm32", target_os = "unknown"),
        wasm_bindgen(constructor)
    )]
    pub fn new() -> Result<Self, String> {
        let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::Voip)
            .map_err(|error| error.to_string())?;
        encoder.bitrate_bps = 64_000;
        encoder.complexity = 5;
        Ok(Self { encoder })
    }

    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, String> {
        if samples.len() != FRAME_SAMPLES
            || samples
                .iter()
                .any(|value| !value.is_finite() || value.abs() > 1.)
        {
            return Err("expected 20 ms of finite mono PCM in [-1, 1]".into());
        }
        let mut output = vec![0; MAX_ENCODED_BYTES];
        let size = self
            .encoder
            .encode(samples, FRAME_SAMPLES, &mut output)
            .map_err(|error| error.to_string())?;
        output.truncate(size);
        if output.is_empty() {
            return Err("encoder produced no Opus packet".into());
        }
        Ok(output)
    }
}

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
pub struct VoiceDecoder {
    decoder: OpusDecoder,
}

#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
impl VoiceDecoder {
    #[cfg_attr(
        all(target_arch = "wasm32", target_os = "unknown"),
        wasm_bindgen(constructor)
    )]
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            decoder: OpusDecoder::new(SAMPLE_RATE as i32, 1).map_err(|error| error.to_string())?,
        })
    }

    /// Accept normal raw Opus packets, including stereo packets downmixed by
    /// Opus to mono. Reject invalid duration before entering the decoder.
    pub fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>, String> {
        if encoded.is_empty() || encoded.len() > 1024 {
            return Err("invalid Opus packet size".into());
        }
        let expected = packet::samples_48k(encoded).map_err(|error| error.to_string())?;
        if expected == 0 || expected > MAX_DECODED_SAMPLES {
            return Err("invalid Opus packet duration".into());
        }
        let mut output = vec![0.; MAX_DECODED_SAMPLES];
        let count = self
            .decoder
            .decode(encoded, MAX_DECODED_SAMPLES, &mut output)
            .map_err(|error| error.to_string())?;
        if count != expected {
            return Err("decoded Opus duration mismatch".into());
        }
        output.truncate(count);
        if output.iter().any(|value| !value.is_finite()) {
            return Err("non-finite decoded audio".into());
        }
        // Clipping is bounded before mixing. No speech DSP or extra AEC here;
        // microphone echo/noise processing belongs to the browser capture API.
        for value in &mut output {
            *value = value.clamp(-1., 1.);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_voice_retains_a_real_tone_and_the_expected_clock_duration() {
        let mut encoder = VoiceEncoder::new().unwrap();
        let mut decoder = VoiceDecoder::new().unwrap();
        let mut samples = Vec::new();
        for frame in 0..12 {
            let pcm: Vec<_> = (0..FRAME_SAMPLES)
                .map(|i| {
                    (std::f32::consts::TAU * 440. * (frame * FRAME_SAMPLES + i) as f32
                        / SAMPLE_RATE as f32)
                        .sin()
                        * 0.25
                })
                .collect();
            let encoded = encoder.encode(&pcm).unwrap();
            assert!(encoded.len() <= MAX_ENCODED_BYTES);
            let output = decoder.decode(&encoded).unwrap();
            assert_eq!(output.len(), FRAME_SAMPLES);
            samples.extend(output);
        }
        let energy: f32 = samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32;
        assert!(
            energy > 0.005 && energy < 0.06,
            "unexpected decoded tone energy {energy}"
        );
        let crossing = samples
            .windows(2)
            .filter(|v| v[0] <= 0. && v[1] > 0.)
            .count();
        let frequency = crossing as f32 * SAMPLE_RATE as f32 / samples.len() as f32;
        assert!(
            (frequency - 440.).abs() < 20.,
            "tone frequency changed to {frequency}"
        );
    }

    #[test]
    fn malformed_or_unbounded_input_is_rejected_without_poisoning_the_codec() {
        let mut encoder = VoiceEncoder::new().unwrap();
        for pcm in [
            vec![],
            vec![0.; FRAME_SAMPLES - 1],
            vec![f32::NAN; FRAME_SAMPLES],
            vec![1.1; FRAME_SAMPLES],
        ] {
            assert!(encoder.encode(&pcm).is_err());
        }
        let packet = encoder.encode(&vec![0.; FRAME_SAMPLES]).unwrap();
        let mut decoder = VoiceDecoder::new().unwrap();
        assert!(decoder.decode(&[]).is_err());
        assert!(decoder.decode(&vec![0; 1025]).is_err());
        assert!(decoder.decode(&[0xff, 0xff]).is_err());
        assert_eq!(decoder.decode(&packet).unwrap().len(), FRAME_SAMPLES);
    }
}
