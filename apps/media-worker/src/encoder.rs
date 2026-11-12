// SPDX-License-Identifier: EUPL-1.2
//! Production TX construction shared by live capture and codec IPC.
use opus_pure::{Application, OpusEncoder};

pub fn new() -> Result<OpusEncoder, opus_pure::Error> {
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Audio)?;
    encoder.bitrate_bps = nuschl_core::audio::DEFAULT_VOICE_BITRATE as i32;
    Ok(encoder)
}

pub fn live() -> Result<OpusEncoder, opus_pure::Error> {
    let mut encoder = new()?;
    encoder.use_inband_fec = true;
    encoder.packet_loss_perc = 5;
    Ok(encoder)
}

#[cfg(test)]
mod cvbr;

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{OpusDecoder, RateControl, packet};

    #[test]
    fn production_factories_keep_audio_across_budget_updates_and_resets() {
        for mut encoder in [new().unwrap(), live().unwrap()] {
            assert_eq!(encoder.application(), Application::Audio);
            assert_eq!(encoder.bitrate_bps, 128_000);
            assert_eq!(encoder.complexity, 9);
            assert_eq!(encoder.rate_control, RateControl::ConstrainedVbr);
            // Query the codec; independent C CTL4027 at this rate/profile is312.
            assert_eq!(encoder.lookahead(), 312);
            let fec = encoder.use_inband_fec;
            let loss = encoder.packet_loss_perc;
            for bitrate in [128_000, 32_000, 48_000, 64_000, 128_000] {
                encoder.bitrate_bps = bitrate;
                encoder.reset_state().unwrap();
                assert_eq!(encoder.application(), Application::Audio);
                assert_eq!(encoder.use_inband_fec, fec);
                assert_eq!(encoder.packet_loss_perc, loss);
                let mut decoder = OpusDecoder::new(48_000, 1).unwrap();
                let mut output = [0.; 960];
                let mut packet = vec![0; bitrate as usize / 400];
                for frame in 0..8 {
                    let source: [f32; 960] = std::array::from_fn(|index| {
                        let t = (frame * 960 + index) as f32 / 48_000.;
                        0.2 * (std::f32::consts::TAU * 997. * t).sin()
                            + 0.08 * (std::f32::consts::TAU * 17_011. * t).sin()
                    });
                    let size = encoder.encode(&source, 960, &mut packet).unwrap();
                    packet::validate(&packet[..size]).unwrap();
                    assert_eq!(packet::samples_48k(&packet[..size]).unwrap(), 960);
                    assert_eq!(
                        decoder.decode(&packet[..size], 960, &mut output).unwrap(),
                        960
                    );
                    assert!(output.iter().all(|value| value.is_finite()));
                }
                assert!(output.iter().map(|value| value * value).sum::<f32>() > 1.);
            }
        }
        assert!(!new().unwrap().use_inband_fec);
        assert_eq!(new().unwrap().packet_loss_perc, 0);
        assert!(live().unwrap().use_inband_fec);
        assert_eq!(live().unwrap().packet_loss_perc, 5);
    }
}
