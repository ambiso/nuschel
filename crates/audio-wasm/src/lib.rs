// SPDX-License-Identifier: EUPL-1.2
//! Codecs and caller-clocked playout: no device, platform clock, network, storage
//! or session-key interface.
//! Load the generated WASM in its own bounded audio worker, never the TLS worker.
#![forbid(unsafe_code)]

use opus_pure::{Application, MAX_PACKET_SAMPLES, OpusDecoder, OpusEncoder, packet};
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use wasm_bindgen::prelude::*;

mod playout;
pub use playout::VoicePlayout;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 960;
pub const DEFAULT_BITRATE: u32 = 128_000;
pub const MIN_BITRATE: u32 = 6_000;
/// The highest requested target (nuschl_core::bitrate::MAX_VOICE_BITRATE):
/// Opus lowers a mono encoder to 300 kb/s, and 640 bytes per 20 ms frame
/// stays well inside Mumble's voice datagram.
pub const MAX_BITRATE: u32 = 256_000;
/// Leaves space for Mumble's header inside its 1024-byte tunneled voice bound.
pub const MAX_ENCODED_BYTES: usize = 960;
pub const OUTPUT_CHANNELS: usize = 2;
pub const MAX_DECODED_SAMPLES: usize = MAX_PACKET_SAMPLES * OUTPUT_CHANNELS;

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
        let mut encoder = OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::Audio)
            .map_err(|error| error.to_string())?;
        encoder.bitrate_bps = DEFAULT_BITRATE as i32;
        encoder.complexity = 5;
        Ok(Self { encoder })
    }

    /// Query this codec's actual algorithmic delay, including its profile.
    pub fn lookahead_samples(&self) -> u32 {
        self.encoder.lookahead() as u32
    }

    /// The effective target the SDK chose from the requested one and the
    /// authenticated session's current server budget. Invalid controls fail
    /// before mutating the running codec.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), String> {
        if !(MIN_BITRATE..=MAX_BITRATE).contains(&bitrate) {
            return Err("invalid voice bitrate".into());
        }
        self.encoder.bitrate_bps = bitrate as i32;
        Ok(())
    }

    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, String> {
        if samples.len() != FRAME_SAMPLES
            || samples
                .iter()
                .any(|value| !value.is_finite() || value.abs() > 1.)
        {
            return Err("expected 20 ms of finite mono PCM in [-1, 1]".into());
        }
        // Bound VBR packets to this 20 ms destination payload allowance.
        let mut output = vec![0; (self.encoder.bitrate_bps as usize / 400).min(MAX_ENCODED_BYTES)];
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
        let mut decoder = OpusDecoder::new(SAMPLE_RATE as i32, OUTPUT_CHANNELS)
            .map_err(|error| error.to_string())?;
        // Explicit receive setting, matching native/Mumble mono downmix behavior.
        decoder.set_phase_inversion_disabled(true);
        Ok(Self { decoder })
    }

    /// Interleaved stereo PCM; mono Opus is duplicated without losing stereo
    /// input. The return length is frame count times OUTPUT_CHANNELS.
    pub fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>, String> {
        if encoded.is_empty() || encoded.len() > 1024 {
            return Err("invalid Opus packet size".into());
        }
        let expected = packet::samples_48k(encoded).map_err(|error| error.to_string())?;
        if expected == 0 || expected > MAX_PACKET_SAMPLES || expected % 480 != 0 {
            return Err("invalid Opus packet duration".into());
        }
        let mut output = vec![0.; MAX_DECODED_SAMPLES];
        let count = self
            .decoder
            .decode(encoded, MAX_PACKET_SAMPLES, &mut output)
            .map_err(|error| error.to_string())?;
        if count != expected {
            return Err("decoded Opus duration mismatch".into());
        }
        output.truncate(count * OUTPUT_CHANNELS);
        if output.iter().any(|value| !value.is_finite()) {
            return Err("non-finite decoded audio".into());
        }
        // Preserve finite codec headroom until requested gain is applied by
        // the bounded mixer. Clipping here distorts even attenuated playback.
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transmit_audio_profile_survives_server_forced_bitrates() {
        let mut encoder = VoiceEncoder::new().unwrap();
        assert_eq!(encoder.encoder.complexity, 5);
        assert!(!encoder.encoder.use_inband_fec);
        assert_eq!(encoder.encoder.packet_loss_perc, 0);
        assert_eq!(
            encoder.encoder.rate_control,
            opus_pure::RateControl::ConstrainedVbr
        );
        for bitrate in [128_000, 32_000, 48_000, 64_000, 128_000] {
            encoder.set_bitrate(bitrate).unwrap();
            assert_eq!(encoder.encoder.application(), Application::Audio);
            assert_eq!(encoder.lookahead_samples(), 312);
            let samples: Vec<_> = (0..FRAME_SAMPLES)
                .map(|index| {
                    let time = index as f32 / SAMPLE_RATE as f32;
                    0.2 * (std::f32::consts::TAU * 997. * time).sin()
                        + 0.08 * (std::f32::consts::TAU * 17_011. * time).sin()
                })
                .collect();
            let encoded = encoder.encode(&samples).unwrap();
            packet::validate(&encoded).unwrap();
            assert_eq!(packet::samples_48k(&encoded).unwrap(), FRAME_SAMPLES);
            assert!(encoded.len() <= bitrate as usize / 400);
        }
    }

    #[test]
    fn receive_decoder_uses_mumbles_phase_setting() {
        assert!(
            VoiceDecoder::new()
                .unwrap()
                .decoder
                .phase_inversion_disabled()
        );
    }

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
            assert_eq!(output.len(), FRAME_SAMPLES * OUTPUT_CHANNELS);
            samples.extend(output.chunks_exact(OUTPUT_CHANNELS).map(|frame| frame[0]));
        }
        let energy: f32 = samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32;
        assert!(
            energy > 0.005 && energy < 0.06,
            "unexpected decoded tone energy {energy}"
        );
        // Codec startup/quantization noise can add zero crossings. Measure the
        // actual carrier spectrum after startup, rather than counting those.
        let stable = &samples[FRAME_SAMPLES * 2..];
        let amplitude = |frequency: f32| {
            let (mut real, mut imaginary) = (0., 0.);
            for (index, value) in stable.iter().enumerate() {
                let phase = std::f32::consts::TAU * frequency * index as f32 / SAMPLE_RATE as f32;
                real += value * phase.cos();
                imaginary += value * phase.sin();
            }
            real * real + imaginary * imaginary
        };
        let carrier = amplitude(440.);
        for other in [100., 200., 400., 480., 880., 1200.] {
            assert!(
                carrier > amplitude(other) * 10.,
                "carrier was lost relative to {other} Hz"
            );
        }
    }

    #[test]
    fn live_targets_bound_real_encoded_packets_and_preserve_decodable_duration() {
        let mut encoder = VoiceEncoder::new().unwrap();
        assert_eq!(encoder.encoder.bitrate_bps, 128_000);
        let mut decoder = VoiceDecoder::new().unwrap();
        for bitrate in [
            128_000,
            24_000,
            6_000,
            48_000,
            MAX_BITRATE,
            192_000,
            128_000,
        ] {
            encoder.set_bitrate(bitrate).unwrap();
            for frame in 0..8 {
                let pcm: Vec<_> = (0..FRAME_SAMPLES)
                    .map(|i| {
                        (std::f32::consts::TAU * 997. * (frame * FRAME_SAMPLES + i) as f32
                            / SAMPLE_RATE as f32)
                            .sin()
                            * 0.3
                    })
                    .collect();
                let encoded = encoder.encode(&pcm).unwrap();
                assert!(encoded.len() <= (bitrate / 400) as usize);
                assert_eq!(
                    decoder.decode(&encoded).unwrap().len(),
                    FRAME_SAMPLES * OUTPUT_CHANNELS
                );
            }
        }
        for invalid in [0, 5999, MAX_BITRATE + 1, 384_000, 510_000, u32::MAX] {
            assert!(encoder.set_bitrate(invalid).is_err());
            assert_eq!(encoder.encoder.bitrate_bps, 128_000);
        }
    }

    /// Constrained VBR at the browser's complexity on varied content: every
    /// packet fits the 20 ms allowance the SDK admits (bitrate / 400 bytes).
    #[test]
    fn constrained_vbr_packets_fit_the_frame_allowance_at_every_choice() {
        let mut seed = 0x6e75_7363_u64;
        let mut noise = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 40) as f32 / (1u64 << 23) as f32) - 1.
        };
        for bitrate in [
            16_000,
            40_000,
            48_000,
            64_000,
            96_000,
            128_000,
            192_000,
            MAX_BITRATE,
        ] {
            let mut encoder = VoiceEncoder::new().unwrap();
            encoder.set_bitrate(bitrate).unwrap();
            let mut decoder = VoiceDecoder::new().unwrap();
            for frame in 0..150 {
                let pcm: Vec<_> = (0..FRAME_SAMPLES)
                    .map(|i| {
                        let t = (frame * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                        let syllable = (std::f32::consts::TAU * 4. * t).sin().max(0.);
                        (0.3 * syllable * (std::f32::consts::TAU * 180. * t).sin() + 0.2 * noise())
                            .clamp(-1., 1.)
                    })
                    .collect();
                let encoded = encoder.encode(&pcm).unwrap();
                assert!(
                    encoded.len() <= (bitrate / 400) as usize,
                    "{} bytes at {bitrate}",
                    encoded.len()
                );
                assert!(encoded.len() <= MAX_ENCODED_BYTES);
                assert_eq!(
                    decoder.decode(&encoded).unwrap().len(),
                    FRAME_SAMPLES * OUTPUT_CHANNELS
                );
            }
        }
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
        assert_eq!(
            decoder.decode(&packet).unwrap().len(),
            FRAME_SAMPLES * OUTPUT_CHANNELS
        );
    }
}
