// SPDX-License-Identifier: EUPL-1.2
mod adaptive;
mod dsp;
mod graph;
mod jitter;
mod live;
mod probe;
mod reference;
mod sounds;

use std::io;

use nuschel_ipc::{Envelope, MediaRequest, MediaResponse, read_frame, write_frame};
use opus_pure::{Application, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusDecoder, OpusEncoder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // On Windows, the launch stage starts this worker again in its AppContainer.
    #[cfg(windows)]
    nuschel_sandbox_windows::launch_if_requested();
    if std::env::args().any(|argument| argument == "--live") {
        return live::run();
    }
    if std::env::args().any(|argument| argument == "--probe-route") {
        return live::probe_route();
    }
    // Native device I/O and DSP will live here too. This process receives no server credentials.
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Voip)?;
    encoder.bitrate_bps = 64_000;
    let mut decoder = OpusDecoder::new(48_000, 1)?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<MediaRequest>(&mut input)? {
        let payload = handle(request.payload, &mut encoder, &mut decoder)
            .unwrap_or_else(|error| MediaResponse::Rejected(error.to_string()));
        write_frame(&mut output, &Envelope::new(request.request_id, payload))?;
    }
    Ok(())
}

fn handle(
    request: MediaRequest,
    encoder: &mut OpusEncoder,
    decoder: &mut OpusDecoder,
) -> Result<MediaResponse, opus_pure::Error> {
    match request {
        MediaRequest::DecodeOpus { packet } => {
            if packet.is_empty() || packet.len() > MAX_PACKET_BYTES {
                return Ok(MediaResponse::Rejected("invalid Opus packet length".into()));
            }
            let mut samples = vec![0.0; MAX_PACKET_SAMPLES];
            let count = decoder.decode(&packet, MAX_PACKET_SAMPLES, &mut samples)?;
            samples.truncate(count);
            if samples.iter().any(|value| !value.is_finite()) {
                return Ok(MediaResponse::Rejected("non-finite decoded audio".into()));
            }
            Ok(MediaResponse::Samples(samples))
        }
        MediaRequest::EncodeOpus { samples } => {
            if samples.len() != 960
                || samples
                    .iter()
                    .any(|value| !value.is_finite() || value.abs() > 1.0)
            {
                return Ok(MediaResponse::Rejected(
                    "expected 20 ms of finite mono PCM in [-1, 1]".into(),
                ));
            }
            let mut packet = vec![0; MAX_PACKET_BYTES];
            let size = encoder.encode(&samples, 960, &mut packet)?;
            packet.truncate(size);
            Ok(MediaResponse::Packet(packet))
        }
    }
}
